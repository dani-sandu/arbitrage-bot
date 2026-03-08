use anyhow::{anyhow, Result};
use alloy::network::TransactionBuilder;
use colored::*;
use serde::Deserialize;
use std::env;

const POLYGON_CHAIN_ID: u64 = 137;

// Polygon contract addresses
const USDC_E: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";
const CTF_CONTRACT: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";
const NEG_RISK_ADAPTER: &str = "0xd91E80cF2E7be2e162c6513ceD06f1dD0dA35296";

// Function selectors
const CTF_BALANCE_OF_SELECTOR: &str = "0x00fdd58e"; // balanceOf(address,uint256)
const ERC20_BALANCE_OF_SELECTOR: &str = "0x70a08231"; // balanceOf(address)
const REDEEM_NEG_RISK_SELECTOR: &str = "0x01a9313e"; // NegRiskAdapter.redeemPositions(bytes32,uint256[])
const IS_APPROVED_FOR_ALL_SELECTOR: &str = "0xe985e9c5"; // isApprovedForAll(address,address)
const SET_APPROVAL_FOR_ALL_SELECTOR: &str = "0xa22cb465"; // setApprovalForAll(address,bool)

// Gamma API
const GAMMA_API: &str = "https://gamma-api.polymarket.com";

/// A position returned by the Polymarket data-api /positions endpoint.
#[derive(Debug, Clone, Deserialize)]
struct ApiPosition {
    #[serde(rename = "conditionId", default)]
    condition_id: String,
    #[serde(default)]
    asset: String, // token_id
    #[serde(default)]
    size: f64,
    #[serde(rename = "initialValue", default)]
    initial_value: f64,
    #[serde(rename = "currentValue", default)]
    current_value: f64,
    #[serde(rename = "curPrice", default)]
    cur_price: f64,
    #[serde(default)]
    redeemable: bool,
    #[serde(default)]
    title: String,
    #[serde(default)]
    outcome: String,
    #[serde(rename = "outcomeIndex", default)]
    outcome_index: u64,
}

/// Aggregated position per token.
struct Position {
    condition_id: String,
    title: String,
    outcome: String,
    outcome_index: u64,
    token_id: String,
    tokens: f64,
    cost_usdc: f64,
    current_value: f64,
    redeemable: bool,
}

/// Market resolution info from Gamma API.
#[derive(Debug, Deserialize)]
struct GammaMarket {
    #[serde(rename = "conditionId")]
    condition_id: Option<String>,
    resolved: Option<bool>,
    question: Option<String>,
    tokens: Option<Vec<GammaToken>>,
    #[serde(rename = "negRisk", default)]
    neg_risk: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct GammaToken {
    outcome: Option<String>,
    token_id: Option<String>,
    winner: Option<bool>,
}

fn hex_to_bytes(hex_str: &str) -> Vec<u8> {
    let s = hex_str.trim_start_matches("0x");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0))
        .collect()
}

fn pad_address(addr: &str) -> String {
    format!(
        "{:0>64}",
        addr.trim().trim_start_matches("0x").to_lowercase()
    )
}

fn pad_uint256_from_decimal(dec_str: &str) -> String {
    let val = alloy::primitives::U256::from_str_radix(dec_str, 10)
        .unwrap_or(alloy::primitives::U256::ZERO);
    format!("{:0>64x}", val)
}

fn parse_uint256_as_f64(hex_result: &str, decimals: u8) -> f64 {
    let hex = hex_result.trim_start_matches("0x");
    if hex.is_empty() || hex.chars().all(|c| c == '0') {
        return 0.0;
    }
    let segment = if hex.len() > 32 {
        &hex[hex.len() - 32..]
    } else {
        hex
    };
    let value = u128::from_str_radix(segment, 16).unwrap_or(0) as f64;
    value / 10_f64.powi(decimals as i32)
}

async fn rpc_eth_call(rpc_url: &str, to: &str, data: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{"to": to, "data": data}, "latest"],
        "id": 1
    });
    let resp = client
        .post(rpc_url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;
    let json: serde_json::Value = resp.json().await?;
    json.get("result")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("RPC eth_call failed: {:?}", json.get("error")))
}

async fn get_ctf_balance(rpc_url: &str, wallet: &str, token_id: &str) -> Result<f64> {
    let token_id_hex = if token_id.starts_with("0x") {
        format!("{:0>64}", token_id.trim_start_matches("0x"))
    } else {
        pad_uint256_from_decimal(token_id)
    };
    let data = format!(
        "{}{}{}",
        CTF_BALANCE_OF_SELECTOR,
        pad_address(wallet),
        token_id_hex
    );
    let result = rpc_eth_call(rpc_url, CTF_CONTRACT, &data).await?;
    Ok(parse_uint256_as_f64(&result, 6))
}

async fn get_usdc_balance(rpc_url: &str, wallet: &str) -> Result<f64> {
    let data = format!("{}{}", ERC20_BALANCE_OF_SELECTOR, pad_address(wallet));
    let result = rpc_eth_call(rpc_url, USDC_E, &data).await?;
    Ok(parse_uint256_as_f64(&result, 6))
}

/// Fetch market resolution status from Gamma API.
/// Returns (resolved, winner_outcome, is_neg_risk).
async fn fetch_market_resolution(condition_id: &str) -> Result<(bool, Option<String>, bool)> {
    let url = format!("{}/markets?conditionIds={}", GAMMA_API, condition_id);
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;
    let markets: Vec<GammaMarket> = resp.json().await?;

    if let Some(market) = markets.first() {
        let resolved = market.resolved.unwrap_or(false);
        let neg_risk = market.neg_risk.unwrap_or(false);
        let winner = market
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.iter().find(|t| t.winner == Some(true)))
            .and_then(|t| t.outcome.clone());
        Ok((resolved, winner, neg_risk))
    } else {
        Err(anyhow!("Market not found for conditionId {}", condition_id))
    }
}

/// Build calldata for NegRiskAdapter.redeemPositions(bytes32 conditionId, uint256[] indexSets)
fn build_negrisk_redeem_calldata(condition_id: &str) -> Vec<u8> {
    let cond_padded = format!(
        "{:0>64}",
        condition_id.trim_start_matches("0x").to_lowercase()
    );
    let offset = format!("{:0>64x}", 64u64);
    let arr_len = format!("{:0>64x}", 2u64);
    let idx_0 = format!("{:0>64x}", 1u64);
    let idx_1 = format!("{:0>64x}", 2u64);
    let calldata_hex = format!(
        "{}{}{}{}{}{}",
        REDEEM_NEG_RISK_SELECTOR.trim_start_matches("0x"),
        cond_padded, offset, arr_len, idx_0, idx_1,
    );
    hex_to_bytes(&calldata_hex)
}

/// Build calldata for CTF.redeemPositions(address collateralToken, bytes32 parentCollectionId, bytes32 conditionId, uint256[] indexSets)
fn build_ctf_redeem_calldata(condition_id: &str) -> Vec<u8> {
    // Compute selector: keccak256("redeemPositions(address,bytes32,bytes32,uint256[])")
    let selector = &alloy::primitives::keccak256(
        "redeemPositions(address,bytes32,bytes32,uint256[])".as_bytes(),
    )[..4];
    let selector_hex = hex::encode(selector);

    let collateral_padded = pad_address(USDC_E);
    let parent_collection = format!("{:0>64}", "0"); // bytes32(0) for top-level
    let cond_padded = format!(
        "{:0>64}",
        condition_id.trim_start_matches("0x").to_lowercase()
    );
    // offset to dynamic data: 4 static params × 32 = 128 bytes = 0x80
    let offset = format!("{:0>64x}", 128u64);
    let arr_len = format!("{:0>64x}", 2u64);
    let idx_0 = format!("{:0>64x}", 1u64);
    let idx_1 = format!("{:0>64x}", 2u64);
    let calldata_hex = format!(
        "{}{}{}{}{}{}{}{}",
        selector_hex, collateral_padded, parent_collection,
        cond_padded, offset, arr_len, idx_0, idx_1,
    );
    hex_to_bytes(&calldata_hex)
}

/// Simulate a call via eth_call and return Ok if it doesn't revert, Err with reason if it does.
async fn simulate_call(rpc_url: &str, from: &str, to: &str, calldata: &[u8]) -> Result<()> {
    let client = reqwest::Client::new();
    let data_hex = format!("0x{}", hex::encode(calldata));
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{"from": from, "to": to, "data": data_hex}, "latest"],
        "id": 1
    });
    let resp = client
        .post(rpc_url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;
    let json: serde_json::Value = resp.json().await?;
    if let Some(err) = json.get("error") {
        Err(anyhow!("eth_call reverted: {}", err))
    } else {
        Ok(())
    }
}

/// Check isApprovedForAll(owner, operator) on CTF contract.
async fn check_approval(rpc_url: &str, owner: &str, operator: &str) -> Result<bool> {
    let data = format!(
        "{}{}{}",
        IS_APPROVED_FOR_ALL_SELECTOR,
        pad_address(owner),
        pad_address(operator),
    );
    let result = rpc_eth_call(rpc_url, CTF_CONTRACT, &data).await?;
    let hex = result.trim_start_matches("0x");
    Ok(!hex.is_empty() && !hex.chars().all(|c| c == '0'))
}

/// Send setApprovalForAll(operator, true) on CTF contract.
async fn send_approval_tx(
    rpc_url: &str,
    operator: &str,
    signer: alloy::signers::local::PrivateKeySigner,
) -> Result<()> {
    let data_hex = format!(
        "{}{}{}",
        SET_APPROVAL_FOR_ALL_SELECTOR.trim_start_matches("0x"),
        pad_address(operator),
        format!("{:0>64x}", 1u64), // true
    );
    let calldata = hex_to_bytes(&data_hex);

    let to_addr = CTF_CONTRACT
        .parse::<alloy::primitives::Address>()
        .map_err(|e| anyhow!("Invalid address: {}", e))?;

    let tx = alloy::rpc::types::TransactionRequest::default()
        .with_to(to_addr)
        .with_gas_limit(100_000)
        .with_input(alloy::primitives::Bytes::from(calldata));

    let url: url::Url = rpc_url.parse()?;
    let provider = alloy::providers::ProviderBuilder::new()
        .wallet(signer)
        .with_chain_id(POLYGON_CHAIN_ID)
        .connect_http(url);

    use alloy::providers::Provider;
    let pending = provider.send_transaction(tx).await?;
    let tx_hash = *pending.tx_hash();
    println!(
        "  {} Approval tx sent: 0x{:x} — waiting...",
        "→".cyan(),
        tx_hash
    );
    let receipt = pending.get_receipt().await?;
    if receipt.status() {
        println!("  {} Approval confirmed", "✓".green());
        Ok(())
    } else {
        Err(anyhow!("Approval tx reverted: 0x{:x}", tx_hash))
    }
}

/// Send the redemption transaction via the EOA private key.
/// Tries NegRisk adapter first (with simulation), falls back to CTF direct.
async fn send_redeem_tx(
    rpc_url: &str,
    condition_id: &str,
    is_neg_risk: bool,
    wallet: &str,
    signer: alloy::signers::local::PrivateKeySigner,
) -> Result<String> {
    let from_addr = format!("0x{:x}", {
        use alloy::signers::Signer;
        signer.address()
    });

    // Try NegRisk adapter first if flagged
    let (calldata, target_contract, method_name) = if is_neg_risk {
        // Simulate NegRisk call
        let nr_calldata = build_negrisk_redeem_calldata(condition_id);
        match simulate_call(rpc_url, &from_addr, NEG_RISK_ADAPTER, &nr_calldata).await {
            Ok(()) => {
                println!("  {} NegRisk simulation OK", "✓".green());
                (nr_calldata, NEG_RISK_ADAPTER, "NegRisk")
            }
            Err(e) => {
                println!("  {} NegRisk simulation failed: {}", "⚠".yellow(), e);
                println!("  {} Trying CTF direct redemption...", "→".cyan());
                let ctf_calldata = build_ctf_redeem_calldata(condition_id);
                match simulate_call(rpc_url, &from_addr, CTF_CONTRACT, &ctf_calldata).await {
                    Ok(()) => {
                        println!("  {} CTF direct simulation OK", "✓".green());
                        (ctf_calldata, CTF_CONTRACT, "CTF direct")
                    }
                    Err(e2) => {
                        return Err(anyhow!("Both NegRisk and CTF direct failed.\n    NegRisk: {}\n    CTF: {}", e, e2));
                    }
                }
            }
        }
    } else {
        // Not NegRisk — try CTF direct first
        let ctf_calldata = build_ctf_redeem_calldata(condition_id);
        match simulate_call(rpc_url, &from_addr, CTF_CONTRACT, &ctf_calldata).await {
            Ok(()) => {
                println!("  {} CTF direct simulation OK", "✓".green());
                (ctf_calldata, CTF_CONTRACT, "CTF direct")
            }
            Err(e) => {
                println!("  {} CTF direct simulation failed: {}", "⚠".yellow(), e);
                println!("  {} Trying NegRisk adapter...", "→".cyan());
                let nr_calldata = build_negrisk_redeem_calldata(condition_id);
                match simulate_call(rpc_url, &from_addr, NEG_RISK_ADAPTER, &nr_calldata).await {
                    Ok(()) => {
                        println!("  {} NegRisk simulation OK", "✓".green());
                        (nr_calldata, NEG_RISK_ADAPTER, "NegRisk")
                    }
                    Err(e2) => {
                        return Err(anyhow!("Both CTF direct and NegRisk failed.\n    CTF: {}\n    NegRisk: {}", e, e2));
                    }
                }
            }
        }
    };

    println!("  {} Sending {} redemption tx...", "→".cyan(), method_name);

    let to_addr = target_contract
        .parse::<alloy::primitives::Address>()
        .map_err(|e| anyhow!("Invalid address: {}", e))?;

    let tx = alloy::rpc::types::TransactionRequest::default()
        .with_to(to_addr)
        .with_gas_limit(300_000)
        .with_input(alloy::primitives::Bytes::from(calldata));

    let url: url::Url = rpc_url.parse()?;
    let provider = alloy::providers::ProviderBuilder::new()
        .wallet(signer)
        .with_chain_id(POLYGON_CHAIN_ID)
        .connect_http(url);

    use alloy::providers::Provider;
    let pending = provider.send_transaction(tx).await?;
    let tx_hash = *pending.tx_hash();
    println!(
        "  {} Tx sent: 0x{:x} — waiting for confirmation...",
        "→".cyan(),
        tx_hash
    );
    let receipt = pending.get_receipt().await?;
    if receipt.status() {
        Ok(format!("0x{:x}", tx_hash))
    } else {
        Err(anyhow!("Redeem tx reverted: 0x{:x}", tx_hash))
    }
}

const POLYMARKET_DATA_API: &str = "https://data-api.polymarket.com";

/// Fetch positions from Polymarket data-api for a given wallet.
async fn fetch_positions(proxy_wallet: &str) -> Result<Vec<Position>> {
    let url = format!(
        "{}/positions?user={}&limit=200&sizeThreshold=0",
        POLYMARKET_DATA_API, proxy_wallet
    );
    println!("  Fetching positions from data-api...");

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(anyhow!("Positions API returned status {}", resp.status()));
    }

    let api_positions: Vec<ApiPosition> = resp.json().await?;

    let positions: Vec<Position> = api_positions
        .into_iter()
        .filter(|p| p.size > 0.001)
        .map(|p| Position {
            condition_id: p.condition_id,
            title: p.title,
            outcome: p.outcome,
            outcome_index: p.outcome_index,
            token_id: p.asset,
            tokens: p.size,
            cost_usdc: p.initial_value,
            current_value: p.current_value,
            redeemable: p.redeemable,
        })
        .collect();

    println!(
        "  {}",
        format!("Found {} positions", positions.len()).green()
    );
    Ok(positions)
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();

    println!("{}", "\n═══════════════════════════════════════════".cyan());
    println!("{}", "  Polymarket Position Redeemer".cyan().bold());
    println!("{}", "═══════════════════════════════════════════\n".cyan());

    // Load config
    let private_key = env::var("PRIVATE_KEY")
        .map_err(|_| anyhow!("PRIVATE_KEY env var required"))?;
    let proxy_wallet = env::var("PROXY_WALLET")
        .map_err(|_| anyhow!("PROXY_WALLET env var required"))?;
    let rpc_url =
        env::var("RPC_URL").unwrap_or_else(|_| "https://polygon-rpc.com".to_string());
    let dry_run = env::var("DRY_RUN").unwrap_or_else(|_| "true".to_string()) == "true";

    if dry_run {
        println!(
            "{}",
            "⚠  DRY RUN MODE — no transactions will be sent. Set DRY_RUN=false to execute.\n"
                .yellow()
        );
    }

    // Build signer
    let trimmed = private_key.trim();
    let key_with_prefix = if trimmed.starts_with("0x") {
        trimmed.to_string()
    } else {
        format!("0x{}", trimmed)
    };
    let signer = alloy::signers::local::PrivateKeySigner::from_str(&key_with_prefix)
        .map_err(|e| anyhow!("Invalid PRIVATE_KEY: {}", e))?
        .with_chain_id(Some(POLYGON_CHAIN_ID));

    use alloy::signers::Signer;
    let eoa = format!("0x{:x}", signer.address());
    println!("  EOA Address:   {}", eoa.green());
    println!("  Proxy Wallet:  {}", proxy_wallet.green());
    println!("  RPC:           {}", rpc_url);
    println!();

    // Check USDC balance before
    match get_usdc_balance(&rpc_url, &proxy_wallet).await {
        Ok(bal) => println!("  USDC.e Balance (before): {}", format!("${:.6}", bal).green()),
        Err(e) => println!("  {}", format!("Could not fetch USDC balance: {}", e).yellow()),
    }
    println!();

    let positions = match fetch_positions(&proxy_wallet).await {
        Ok(p) => p,
        Err(e) => {
            println!("  {}", format!("Failed to fetch positions: {}", e).red());
            return Ok(());
        }
    };

    if positions.is_empty() {
        println!(
            "{}",
            "No active positions found for this wallet.".yellow()
        );
        return Ok(());
    }

    // Deduplicate condition IDs for redemption (one tx per market)
    let mut unique_conditions: Vec<(String, String)> = Vec::new();
    for p in &positions {
        if !unique_conditions
            .iter()
            .any(|(c, _)| c == &p.condition_id)
        {
            unique_conditions.push((p.condition_id.clone(), p.title.clone()));
        }
    }

    // ─── Phase 1: Check on-chain balances ───
    println!(
        "{}",
        "─── Phase 1: Checking on-chain CTF balances ───\n".cyan()
    );

    let mut total_potential_payout = 0.0;
    let mut total_cost = 0.0;

    for p in &positions {
        let bal = get_ctf_balance(&rpc_url, &proxy_wallet, &p.token_id)
            .await
            .unwrap_or(0.0);
        let status = if bal > 0.001 {
            format!("{:.6} tokens", bal).green()
        } else {
            "0 (already redeemed or not held)".dimmed()
        };
        let redeem_tag = if p.redeemable {
            " [REDEEMABLE]".green()
        } else {
            " [not redeemable]".dimmed()
        };
        println!(
            "  {} [{}]{} → {}",
            p.title.white(),
            p.outcome,
            redeem_tag,
            status
        );
        total_cost += p.cost_usdc;
        if bal > 0.001 {
            total_potential_payout += bal;
        }
    }

    println!(
        "\n  Total cost basis:     {}",
        format!("${:.2}", total_cost).yellow()
    );
    println!(
        "  Tokens still on-chain: {}",
        format!("{:.4}", total_potential_payout).yellow()
    );
    println!(
        "  Max payout if all win: {}",
        format!("${:.2}", total_potential_payout).green()
    );

    // ─── Phase 2: Identify redeemable markets ───
    println!(
        "\n{}",
        "─── Phase 2: Checking market resolution status ───\n".cyan()
    );

    // (conditionId, title, is_neg_risk)
    let mut redeemable: Vec<(String, String, bool)> = Vec::new();

    for p in &positions {
        if p.redeemable {
            if !redeemable.iter().any(|(c, _, _)| c == &p.condition_id) {
                let (winner_info, neg_risk) = match fetch_market_resolution(&p.condition_id).await {
                    Ok((true, Some(w), nr)) => (format!("Winner: {} (neg_risk={})", w, nr).green().bold(), nr),
                    Ok((true, None, nr)) => (format!("Resolved (neg_risk={})", nr).green().normal(), nr),
                    _ => ("Resolved (redeemable)".green().normal(), false),
                };
                println!(
                    "  {} {} → {}",
                    "✓".green(),
                    p.title,
                    winner_info
                );
                redeemable.push((p.condition_id.clone(), p.title.clone(), neg_risk));
            }
        }
    }

    // Also check non-redeemable ones via Gamma as fallback
    for (cid, title) in &unique_conditions {
        if redeemable.iter().any(|(c, _, _)| c == cid) {
            continue;
        }
        match fetch_market_resolution(cid).await {
            Ok((true, winner, neg_risk)) => {
                let w = winner.as_deref().unwrap_or("Unknown");
                println!(
                    "  {} {} → Winner: {} (neg_risk={}, API says redeemable)",
                    "✓".green(),
                    title,
                    w.green().bold(),
                    neg_risk,
                );
                redeemable.push((cid.clone(), title.clone(), neg_risk));
            }
            Ok((false, _, _)) => {
                println!("  {} {} → {}", "⏳".yellow(), title, "Not yet resolved".yellow());
            }
            Err(e) => {
                println!(
                    "  {} {} → {}",
                    "?".red(),
                    title,
                    format!("Could not check: {}", e).red()
                );
            }
        }
    }

    if redeemable.is_empty() {
        println!(
            "\n{}",
            "No markets are resolved yet. Nothing to redeem.".yellow()
        );
        return Ok(());
    }

    // ─── Phase 3: Send redemption transactions ───
    println!(
        "\n{}",
        "─── Phase 3: Redeeming positions ───\n".cyan()
    );

    if dry_run {
        println!(
            "{}",
            "DRY RUN — skipping transactions. Set DRY_RUN=false to execute.\n".yellow()
        );
        for (cid, title, neg_risk) in &redeemable {
            println!(
                "  Would redeem: {} ({}) [neg_risk={}]",
                title,
                &cid[..16],
                neg_risk,
            );
        }
    } else {
        // Check and set approvals before redeeming
        println!("  Checking approvals...");
        let nr_approved = check_approval(&rpc_url, &proxy_wallet, NEG_RISK_ADAPTER).await.unwrap_or(false);
        let ctf_needs_approval = !check_approval(&rpc_url, &proxy_wallet, CTF_CONTRACT).await.unwrap_or(false);
        println!("  NegRisk adapter approved: {}", if nr_approved { "yes".green() } else { "no".red() });
        println!("  CTF self-approval: {}", if !ctf_needs_approval { "yes".green() } else { "no".red() });

        // For CTF direct redemption: the CTF checks that msg.sender can burn.
        // If calling CTF.redeemPositions directly, the caller (EOA) must own the tokens.
        // No special approval needed if caller == token holder.
        println!();

        let mut success_count = 0;
        let mut fail_count = 0;

        for (cid, title, neg_risk) in &redeemable {
            println!("  Redeeming: {}", title.white().bold());
            match send_redeem_tx(&rpc_url, cid, *neg_risk, &proxy_wallet, signer.clone()).await {
                Ok(tx_hash) => {
                    println!(
                        "  {} Redeemed! tx: {}\n",
                        "✓".green(),
                        tx_hash.green()
                    );
                    success_count += 1;
                }
                Err(e) => {
                    println!(
                        "  {} Failed: {}\n",
                        "✗".red(),
                        format!("{}", e).red()
                    );
                    fail_count += 1;
                }
            }
        }

        // Check USDC balance after
        println!();
        match get_usdc_balance(&rpc_url, &proxy_wallet).await {
            Ok(bal) => println!(
                "  USDC.e Balance (after): {}",
                format!("${:.6}", bal).green()
            ),
            Err(e) => println!(
                "  {}",
                format!("Could not fetch USDC balance: {}", e).yellow()
            ),
        }

        println!(
            "\n  Results: {} succeeded, {} failed",
            format!("{}", success_count).green(),
            format!("{}", fail_count).red()
        );
    }

    println!(
        "\n{}",
        "═══════════════════════════════════════════\n".cyan()
    );

    Ok(())
}

use std::str::FromStr;
