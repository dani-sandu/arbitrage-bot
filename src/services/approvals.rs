use crate::config::Env;
use alloy::network::TransactionBuilder;
use anyhow::{anyhow, Result};
use colored::*;

const POLYGON_CHAIN_ID: u64 = 137;

// Polygon contract addresses
const USDC_E: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";
const CTF_CONTRACT: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";
const POLYMARKET_EXCHANGE: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
const NEG_RISK_EXCHANGE: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";
const NEG_RISK_ADAPTER: &str = "0xd91E80cF2E7be2e162c6513ceD06f1dD0dA35296";

// ERC-20 selectors
const ALLOWANCE_SELECTOR: &str = "0xdd62ed3e";
const APPROVE_SELECTOR: &str = "0x095ea7b3";
// ERC-1155 selectors
const IS_APPROVED_FOR_ALL_SELECTOR: &str = "0xe985e9c5";
const SET_APPROVAL_FOR_ALL_SELECTOR: &str = "0xa22cb465";
// Max uint256
const MAX_UINT256_HEX: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

fn hex_to_bytes(hex_str: &str) -> Vec<u8> {
    let s = hex_str.trim_start_matches("0x");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0))
        .collect()
}

fn pad_address(addr: &str) -> String {
    format!("{:0>64}", addr.trim().trim_start_matches("0x").to_lowercase())
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
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?;
    let json: serde_json::Value = resp.json().await?;
    json.get("result")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("RPC eth_call failed: {:?}", json.get("error")))
}

/// Minimum USDC.e allowance required (100 USDC with 6 decimals = 100_000_000).
const MIN_USDC_ALLOWANCE_HEX: u128 = 100_000_000;

async fn check_erc20_allowance(rpc_url: &str, owner: &str, spender: &str) -> Result<bool> {
    let data = format!("{}{}{}", ALLOWANCE_SELECTOR, pad_address(owner), pad_address(spender));
    let result = rpc_eth_call(rpc_url, USDC_E, &data).await?;
    let hex = result.trim_start_matches("0x");
    if hex.is_empty() || hex.chars().all(|c| c == '0') {
        return Ok(false);
    }
    // Parse last 32 hex chars to fit into u128 and compare against minimum
    let segment = if hex.len() > 32 { &hex[hex.len() - 32..] } else { hex };
    let allowance = u128::from_str_radix(segment, 16).unwrap_or(0);
    Ok(allowance >= MIN_USDC_ALLOWANCE_HEX)
}

async fn check_ctf_approval(rpc_url: &str, owner: &str, operator: &str) -> Result<bool> {
    let data = format!("{}{}{}", IS_APPROVED_FOR_ALL_SELECTOR, pad_address(owner), pad_address(operator));
    let result = rpc_eth_call(rpc_url, CTF_CONTRACT, &data).await?;
    let hex = result.trim_start_matches("0x");
    let last_byte = u8::from_str_radix(&hex[hex.len().saturating_sub(2)..], 16).unwrap_or(0);
    Ok(last_byte != 0)
}

async fn send_approve_usdc(rpc_url: &str, spender: &str, signer: alloy::signers::local::PrivateKeySigner) -> Result<()> {
    let calldata = format!("{}{}{}", APPROVE_SELECTOR, pad_address(spender), MAX_UINT256_HEX);
    let data_bytes = hex_to_bytes(&calldata);

    let to_addr = USDC_E.parse::<alloy::primitives::Address>()?;
    let tx = alloy::rpc::types::TransactionRequest::default()
        .with_to(to_addr)
        .with_gas_limit(100_000)
        .with_input(alloy::primitives::Bytes::from(data_bytes));

    let url: url::Url = rpc_url.parse()?;
    let provider = alloy::providers::ProviderBuilder::new()
        .wallet(signer)
        .with_chain_id(POLYGON_CHAIN_ID)
        .connect_http(url);

    use alloy::providers::Provider;
    let pending = provider.send_transaction(tx).await?;
    let tx_hash = *pending.tx_hash();
    println!("  Tx sent: 0x{:x} — waiting for confirmation...", tx_hash);
    let receipt = pending.get_receipt().await?;
    if receipt.status() {
        println!("{}", format!("  ✓ USDC.e approved for {}", &spender[..10]).green());
    } else {
        return Err(anyhow!("USDC.e approve tx reverted"));
    }
    Ok(())
}

async fn send_set_approval_for_all(rpc_url: &str, operator: &str, signer: alloy::signers::local::PrivateKeySigner) -> Result<()> {
    let calldata = format!(
        "{}{}{}",
        SET_APPROVAL_FOR_ALL_SELECTOR,
        pad_address(operator),
        format!("{:0>64}", "1") // true
    );
    let data_bytes = hex_to_bytes(&calldata);

    let to_addr = CTF_CONTRACT.parse::<alloy::primitives::Address>()?;
    let tx = alloy::rpc::types::TransactionRequest::default()
        .with_to(to_addr)
        .with_gas_limit(100_000)
        .with_input(alloy::primitives::Bytes::from(data_bytes));

    let url: url::Url = rpc_url.parse()?;
    let provider = alloy::providers::ProviderBuilder::new()
        .wallet(signer)
        .with_chain_id(POLYGON_CHAIN_ID)
        .connect_http(url);

    use alloy::providers::Provider;
    let pending = provider.send_transaction(tx).await?;
    let tx_hash = *pending.tx_hash();
    println!("  Tx sent: 0x{:x} — waiting for confirmation...", tx_hash);
    let receipt = pending.get_receipt().await?;
    if receipt.status() {
        println!("{}", format!("  ✓ CTF approved for {}", &operator[..10]).green());
    } else {
        return Err(anyhow!("CTF setApprovalForAll tx reverted"));
    }
    Ok(())
}

/// Check and set all required approvals for Polymarket trading.
/// - USDC.e approve for Exchange, NegRisk Exchange, NegRisk Adapter
/// - CTF setApprovalForAll for Exchange, NegRisk Exchange
pub async fn ensure_approvals(env: &Env) -> Result<()> {
    let private_key = env
        .private_key
        .as_ref()
        .ok_or_else(|| anyhow!("PRIVATE_KEY required for approvals"))?;

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

    println!("{}", "\n🔐 Checking token approvals...".cyan());

    let usdc_spenders = [
        ("Exchange", POLYMARKET_EXCHANGE),
        ("NegRisk Exchange", NEG_RISK_EXCHANGE),
        ("NegRisk Adapter", NEG_RISK_ADAPTER),
    ];

    for (name, spender) in &usdc_spenders {
        match check_erc20_allowance(&env.rpc_url, &eoa, spender).await {
            Ok(true) => {
                println!("{}", format!("  ✓ USDC.e → {} approved", name).green());
            }
            Ok(false) => {
                println!("{}", format!("  ⚠ USDC.e → {} not approved, sending tx...", name).yellow());
                send_approve_usdc(&env.rpc_url, spender, signer.clone()).await?;
            }
            Err(e) => {
                println!("{}", format!("  ⚠ Could not check USDC.e → {}: {}", name, e).yellow());
            }
        }
    }

    let ctf_operators = [
        ("Exchange", POLYMARKET_EXCHANGE),
        ("NegRisk Exchange", NEG_RISK_EXCHANGE),
    ];

    for (name, operator) in &ctf_operators {
        match check_ctf_approval(&env.rpc_url, &eoa, operator).await {
            Ok(true) => {
                println!("{}", format!("  ✓ CTF    → {} approved", name).green());
            }
            Ok(false) => {
                println!("{}", format!("  ⚠ CTF    → {} not approved, sending tx...", name).yellow());
                send_set_approval_for_all(&env.rpc_url, operator, signer.clone()).await?;
            }
            Err(e) => {
                println!("{}", format!("  ⚠ Could not check CTF → {}: {}", name, e).yellow());
            }
        }
    }

    println!("{}", "✓ All approvals verified\n".green());
    Ok(())
}

use std::str::FromStr;
