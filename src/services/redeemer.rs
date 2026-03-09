use crate::config::Env;
use crate::services::chain_reader::{get_usdc_balance, pad_address, parse_uint256_as_f64, rpc_eth_call};
use anyhow::{anyhow, Result};
use alloy::signers::Signer;
use colored::*;
use serde::Deserialize;
use std::str::FromStr;

// Polygon contract addresses
const CTF_CONTRACT: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";
const NEG_RISK_ADAPTER: &str = "0xd91E80cF2E7be2e162c6513ceD06f1dD0dA35296";
const USDC_E: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";
const POLYGON_CHAIN_ID: u64 = 137;

// Function selectors
const CTF_BALANCE_OF_SELECTOR: &str = "0x00fdd58e";
const IS_APPROVED_FOR_ALL_SELECTOR: &str = "0xe985e9c5";
const SET_APPROVAL_FOR_ALL_SELECTOR: &str = "0xa22cb465";
const REDEEM_NEG_RISK_SELECTOR: &str = "0x01a9313e";

const GAMMA_API: &str = "https://gamma-api.polymarket.com";
const POLYMARKET_DATA_API: &str = "https://data-api.polymarket.com";

lazy_static::lazy_static! {
    static ref HTTP_CLIENT: reqwest::Client = reqwest::Client::new();
}

/// Summary returned after a sweep so the caller can send Telegram alerts.
pub struct RedemptionSummary {
    pub redeemed: usize,
    pub failed: usize,
    pub usdc_gained: f64,
    pub dry_run: bool,
}

// ─── API types ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ApiPosition {
    #[serde(rename = "conditionId", default)]
    condition_id: String,
    #[serde(default)]
    asset: String,
    #[serde(default)]
    size: f64,
    #[serde(rename = "initialValue", default)]
    initial_value: f64,
    #[serde(rename = "currentValue", default)]
    current_value: f64,
    #[serde(default)]
    redeemable: bool,
    #[serde(default)]
    title: String,
    #[serde(default)]
    outcome: String,
    #[serde(rename = "outcomeIndex", default)]
    outcome_index: u64,
}

struct Position {
    condition_id: String,
    title: String,
    outcome: String,
    #[allow(dead_code)]
    outcome_index: u64,
    token_id: String,
    tokens: f64,
    #[allow(dead_code)]
    cost_usdc: f64,
    #[allow(dead_code)]
    current_value: f64,
    redeemable: bool,
}

#[derive(Debug, Deserialize)]
struct GammaMarket {
    resolved: Option<bool>,
    tokens: Option<Vec<GammaToken>>,
    #[serde(rename = "negRisk", default)]
    neg_risk: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct GammaToken {
    outcome: Option<String>,
    winner: Option<bool>,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn hex_to_bytes(hex_str: &str) -> Vec<u8> {
    let s = hex_str.trim_start_matches("0x");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0))
        .collect()
}

fn pad_uint256_decimal(dec_str: &str) -> String {
    let val = alloy::primitives::U256::from_str_radix(dec_str, 10)
        .unwrap_or(alloy::primitives::U256::ZERO);
    format!("{:0>64x}", val)
}

// ─── On-chain reads ───────────────────────────────────────────────────────────

async fn get_ctf_balance_raw(rpc_url: &str, wallet: &str, token_id: &str) -> f64 {
    let token_hex = if token_id.starts_with("0x") {
        format!("{:0>64}", token_id.trim_start_matches("0x"))
    } else {
        pad_uint256_decimal(token_id)
    };
    let data = format!("{}{}{}", CTF_BALANCE_OF_SELECTOR, pad_address(wallet), token_hex);
    match rpc_eth_call(rpc_url, CTF_CONTRACT, &data).await {
        Ok(result) => parse_uint256_as_f64(&result, 6),
        Err(_) => 0.0,
    }
}

async fn check_approval(rpc_url: &str, owner: &str, operator: &str) -> bool {
    let data = format!(
        "{}{}{}",
        IS_APPROVED_FOR_ALL_SELECTOR,
        pad_address(owner),
        pad_address(operator),
    );
    match rpc_eth_call(rpc_url, CTF_CONTRACT, &data).await {
        Ok(result) => {
            let hex = result.trim_start_matches("0x");
            !hex.is_empty() && !hex.chars().all(|c| c == '0')
        }
        Err(_) => false,
    }
}

// ─── Transaction helpers ──────────────────────────────────────────────────────

async fn simulate_call(rpc_url: &str, from: &str, to: &str, calldata: &[u8]) -> Result<()> {
    let data_hex = format!("0x{}", hex::encode(calldata));
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{"from": from, "to": to, "data": data_hex}, "latest"],
        "id": 1
    });
    let resp = HTTP_CLIENT
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

fn build_negrisk_calldata(condition_id: &str) -> Vec<u8> {
    let cond = format!("{:0>64}", condition_id.trim_start_matches("0x").to_lowercase());
    let offset = format!("{:0>64x}", 64u64);
    let arr_len = format!("{:0>64x}", 2u64);
    let idx_0 = format!("{:0>64x}", 1u64);
    let idx_1 = format!("{:0>64x}", 2u64);
    hex_to_bytes(&format!(
        "{}{}{}{}{}{}",
        REDEEM_NEG_RISK_SELECTOR.trim_start_matches("0x"),
        cond, offset, arr_len, idx_0, idx_1,
    ))
}

fn build_ctf_calldata(condition_id: &str) -> Vec<u8> {
    let selector = &alloy::primitives::keccak256(
        "redeemPositions(address,bytes32,bytes32,uint256[])".as_bytes(),
    )[..4];
    let selector_hex = hex::encode(selector);
    let collateral = pad_address(USDC_E);
    let parent = format!("{:0>64}", "0");
    let cond = format!("{:0>64}", condition_id.trim_start_matches("0x").to_lowercase());
    let offset = format!("{:0>64x}", 128u64);
    let arr_len = format!("{:0>64x}", 2u64);
    let idx_0 = format!("{:0>64x}", 1u64);
    let idx_1 = format!("{:0>64x}", 2u64);
    hex_to_bytes(&format!(
        "{}{}{}{}{}{}{}{}",
        selector_hex, collateral, parent, cond, offset, arr_len, idx_0, idx_1,
    ))
}

async fn send_approval_tx(
    rpc_url: &str,
    operator: &str,
    signer: alloy::signers::local::PrivateKeySigner,
) -> Result<()> {
    use alloy::network::TransactionBuilder;
    use alloy::providers::Provider;

    let data_hex = format!(
        "{}{}{}",
        SET_APPROVAL_FOR_ALL_SELECTOR.trim_start_matches("0x"),
        pad_address(operator),
        format!("{:0>64x}", 1u64),
    );
    let calldata = hex_to_bytes(&data_hex);
    let to_addr = CTF_CONTRACT.parse::<alloy::primitives::Address>()
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

    let pending = provider.send_transaction(tx).await?;
    let receipt = pending.get_receipt().await?;
    if receipt.status() {
        println!("    {} Approval confirmed", "✓".green());
        Ok(())
    } else {
        Err(anyhow!("Approval tx reverted"))
    }
}

async fn send_redeem_tx(
    rpc_url: &str,
    condition_id: &str,
    is_neg_risk: bool,
    signer: alloy::signers::local::PrivateKeySigner,
) -> Result<String> {
    use alloy::network::TransactionBuilder;
    use alloy::providers::Provider;

    let from_addr = format!("0x{:x}", signer.address());

    // Choose the correct contract and calldata, with simulation fallback.
    let (calldata, target) = if is_neg_risk {
        let nr = build_negrisk_calldata(condition_id);
        match simulate_call(rpc_url, &from_addr, NEG_RISK_ADAPTER, &nr).await {
            Ok(()) => {
                println!("    {} NegRisk simulation OK", "✓".green());
                (nr, NEG_RISK_ADAPTER)
            }
            Err(e) => {
                println!("    {} NegRisk simulation failed ({}), trying CTF direct", "⚠".yellow(), e);
                let ctf = build_ctf_calldata(condition_id);
                simulate_call(rpc_url, &from_addr, CTF_CONTRACT, &ctf).await
                    .map_err(|e2| anyhow!("Both NegRisk and CTF failed. NegRisk: {} CTF: {}", e, e2))?;
                println!("    {} CTF direct simulation OK", "✓".green());
                (ctf, CTF_CONTRACT)
            }
        }
    } else {
        let ctf = build_ctf_calldata(condition_id);
        match simulate_call(rpc_url, &from_addr, CTF_CONTRACT, &ctf).await {
            Ok(()) => {
                println!("    {} CTF direct simulation OK", "✓".green());
                (ctf, CTF_CONTRACT)
            }
            Err(e) => {
                println!("    {} CTF direct simulation failed ({}), trying NegRisk", "⚠".yellow(), e);
                let nr = build_negrisk_calldata(condition_id);
                simulate_call(rpc_url, &from_addr, NEG_RISK_ADAPTER, &nr).await
                    .map_err(|e2| anyhow!("Both CTF and NegRisk failed. CTF: {} NegRisk: {}", e, e2))?;
                println!("    {} NegRisk simulation OK", "✓".green());
                (nr, NEG_RISK_ADAPTER)
            }
        }
    };

    let to_addr = target.parse::<alloy::primitives::Address>()
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

    let pending = provider.send_transaction(tx).await?;
    let tx_hash = *pending.tx_hash();
    println!("    {} Tx sent: 0x{:x} — waiting...", "→".cyan(), tx_hash);
    let receipt = pending.get_receipt().await?;
    if receipt.status() {
        Ok(format!("0x{:x}", tx_hash))
    } else {
        Err(anyhow!("Redeem tx reverted: 0x{:x}", tx_hash))
    }
}

// ─── API calls ────────────────────────────────────────────────────────────────

async fn fetch_positions(proxy_wallet: &str) -> Result<Vec<Position>> {
    let url = format!(
        "{}/positions?user={}&limit=200&sizeThreshold=0",
        POLYMARKET_DATA_API, proxy_wallet
    );
    let resp = HTTP_CLIENT
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(anyhow!("Positions API returned {}", resp.status()));
    }

    let api_positions: Vec<ApiPosition> = resp.json().await?;
    Ok(api_positions
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
        .collect())
}

/// Returns (resolved, winner_outcome, is_neg_risk).
async fn fetch_market_resolution(condition_id: &str) -> Result<(bool, Option<String>, bool)> {
    let url = format!("{}/markets?conditionIds={}", GAMMA_API, condition_id);
    let resp = HTTP_CLIENT
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;
    let markets: Vec<GammaMarket> = resp.json().await?;

    let market = markets
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("Market not found for conditionId {}", condition_id))?;

    let resolved = market.resolved.unwrap_or(false);
    let neg_risk = market.neg_risk.unwrap_or(false);
    let winner = market
        .tokens
        .as_ref()
        .and_then(|tokens| tokens.iter().find(|t| t.winner == Some(true)))
        .and_then(|t| t.outcome.clone());

    Ok((resolved, winner, neg_risk))
}

// ─── Public entry point ───────────────────────────────────────────────────────

/// Fetch all open positions, identify resolved markets, and redeem them.
/// Shares the same PRIVATE_KEY/PROXY_WALLET/RPC_URL as the main bot.
/// When `dry_run` is true, logs what would happen but sends no transactions.
pub async fn run_redemption_sweep(env: &Env, dry_run: bool) -> Result<RedemptionSummary> {
    let proxy_wallet = match env.proxy_wallet.as_ref() {
        Some(w) => w.clone(),
        None => return Err(anyhow!("PROXY_WALLET required for redemption")),
    };
    let private_key = match env.private_key.as_ref() {
        Some(k) => k.clone(),
        None => return Err(anyhow!("PRIVATE_KEY required for redemption")),
    };

    println!("{}", "\n[REDEEMER] Starting sweep...".cyan());
    if dry_run {
        println!("{}", "[REDEEMER] DRY RUN — no transactions will be sent".yellow());
    }

    // Build signer (needed even in dry_run for the address)
    let key = if private_key.trim().starts_with("0x") {
        private_key.trim().to_string()
    } else {
        format!("0x{}", private_key.trim())
    };
    let signer = alloy::signers::local::PrivateKeySigner::from_str(&key)
        .map_err(|e| anyhow!("Invalid PRIVATE_KEY: {}", e))?
        .with_chain_id(Some(POLYGON_CHAIN_ID));

    // ── Phase 1: Fetch positions ─────────────────────────────────────────────
    let positions = match fetch_positions(&proxy_wallet).await {
        Ok(p) => p,
        Err(e) => {
            println!("{}", format!("[REDEEMER] Failed to fetch positions: {}", e).red());
            return Ok(RedemptionSummary { redeemed: 0, failed: 0, usdc_gained: 0.0, dry_run });
        }
    };

    if positions.is_empty() {
        println!("{}", "[REDEEMER] No open positions found.".bright_black());
        return Ok(RedemptionSummary { redeemed: 0, failed: 0, usdc_gained: 0.0, dry_run });
    }

    println!(
        "{}",
        format!("[REDEEMER] {} open positions found", positions.len()).bright_black()
    );

    // ── Phase 2: Identify redeemable markets ────────────────────────────────
    // Deduplicate by condition_id; each condition needs at most one tx.
    let mut seen_conditions: Vec<String> = Vec::new();
    let mut redeemable: Vec<(String, String, bool)> = Vec::new(); // (conditionId, title, neg_risk)

    for p in &positions {
        if seen_conditions.contains(&p.condition_id) {
            continue;
        }
        seen_conditions.push(p.condition_id.clone());

        // API flag is the fastest path
        let is_candidate = p.redeemable || {
            // Fall back to Gamma API resolution check
            match fetch_market_resolution(&p.condition_id).await {
                Ok((true, _, _)) => true,
                _ => false,
            }
        };

        if !is_candidate {
            continue;
        }

        // Confirm on-chain balance — skip if already redeemed
        let on_chain = get_ctf_balance_raw(&env.rpc_url, &proxy_wallet, &p.token_id).await;
        if on_chain < 0.001 {
            println!(
                "{}",
                format!("[REDEEMER] {} — already redeemed (0 on-chain)", p.title).bright_black()
            );
            continue;
        }

        // Fetch neg_risk flag (needed for calldata selection)
        let neg_risk = match fetch_market_resolution(&p.condition_id).await {
            Ok((_, _, nr)) => nr,
            Err(_) => false,
        };

        println!(
            "{}",
            format!(
                "[REDEEMER] REDEEMABLE  {} [{}]  {:.4} tokens on-chain  neg_risk={}",
                p.title, p.outcome, on_chain, neg_risk
            ).green()
        );
        redeemable.push((p.condition_id.clone(), p.title.clone(), neg_risk));
    }

    if redeemable.is_empty() {
        println!("{}", "[REDEEMER] Nothing to redeem this sweep.".bright_black());
        return Ok(RedemptionSummary { redeemed: 0, failed: 0, usdc_gained: 0.0, dry_run });
    }

    // ── Phase 3: Redeem ──────────────────────────────────────────────────────
    if dry_run {
        for (cid, title, neg_risk) in &redeemable {
            println!(
                "{}",
                format!("[REDEEMER] Would redeem: {} ({:.8}…) neg_risk={}", title, cid, neg_risk).yellow()
            );
        }
        return Ok(RedemptionSummary {
            redeemed: 0,
            failed: 0,
            usdc_gained: 0.0,
            dry_run: true,
        });
    }

    // Ensure NegRiskAdapter approval (one-time, idempotent)
    let nr_approved = check_approval(&env.rpc_url, &proxy_wallet, NEG_RISK_ADAPTER).await;
    if !nr_approved {
        println!("{}", "[REDEEMER] Approving NegRiskAdapter...".yellow());
        if let Err(e) = send_approval_tx(&env.rpc_url, NEG_RISK_ADAPTER, signer.clone()).await {
            println!("{}", format!("[REDEEMER] Approval failed: {}", e).red());
        }
    }

    let usdc_before = get_usdc_balance(env).await.unwrap_or(0.0);

    let mut success_count = 0usize;
    let mut fail_count = 0usize;

    for (cid, title, neg_risk) in &redeemable {
        println!(
            "{}",
            format!("[REDEEMER] Redeeming: {}", title).white().bold()
        );
        match send_redeem_tx(&env.rpc_url, cid, *neg_risk, signer.clone()).await {
            Ok(tx_hash) => {
                println!(
                    "{}",
                    format!("[REDEEMER] ✓ {} — tx: {}", title, tx_hash).green()
                );
                success_count += 1;
            }
            Err(e) => {
                println!(
                    "{}",
                    format!("[REDEEMER] ✗ {} — {}", title, e).red()
                );
                fail_count += 1;
            }
        }
    }

    let usdc_after = get_usdc_balance(env).await.unwrap_or(0.0);
    let usdc_gained = (usdc_after - usdc_before).max(0.0);

    if success_count > 0 {
        println!(
            "{}",
            format!(
                "[REDEEMER] Sweep complete — {} redeemed, {} failed, +${:.4} USDC",
                success_count, fail_count, usdc_gained
            ).green().bold()
        );
    } else {
        println!(
            "{}",
            format!("[REDEEMER] Sweep complete — {} failed", fail_count).red()
        );
    }

    Ok(RedemptionSummary {
        redeemed: success_count,
        failed: fail_count,
        usdc_gained,
        dry_run: false,
    })
}
