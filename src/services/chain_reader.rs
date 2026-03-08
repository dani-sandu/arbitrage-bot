use crate::config::Env;
use anyhow::{anyhow, Result};

const USDC_E: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";
const BALANCE_OF_SELECTOR: &str = "0x70a08231";
const CTF_CONTRACT: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";
// ERC-1155 balanceOf(address,uint256) selector
const CTF_BALANCE_OF_SELECTOR: &str = "0x00fdd58e";

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

fn parse_uint256_as_f64(hex_result: &str, decimals: u8) -> f64 {
    let hex = hex_result.trim_start_matches("0x");
    if hex.is_empty() || hex.chars().all(|c| c == '0') {
        return 0.0;
    }
    // Parse last 32 hex chars (128 bits) to avoid overflow
    let segment = if hex.len() > 32 { &hex[hex.len() - 32..] } else { hex };
    let value = u128::from_str_radix(segment, 16).unwrap_or(0) as f64;
    value / 10_f64.powi(decimals as i32)
}

/// Query USDC.e balance for a wallet address.
pub async fn get_usdc_balance(env: &Env) -> Result<f64> {
    let wallet = env.proxy_wallet.as_ref()
        .ok_or_else(|| anyhow!("PROXY_WALLET required for balance check"))?;
    let data = format!("{}{}", BALANCE_OF_SELECTOR, pad_address(wallet));
    let result = rpc_eth_call(&env.rpc_url, USDC_E, &data).await?;
    Ok(parse_uint256_as_f64(&result, 6)) // USDC.e has 6 decimals
}

/// Query CTF (Conditional Token) balance for a specific token ID.
/// Returns the number of outcome tokens held.
pub async fn get_ctf_balance(env: &Env, token_id: &str) -> Result<f64> {
    let wallet = env.proxy_wallet.as_ref()
        .ok_or_else(|| anyhow!("PROXY_WALLET required for balance check"))?;
    // balanceOf(address, uint256) — pad address + pad token_id as uint256
    let token_id_hex = if token_id.starts_with("0x") {
        format!("{:0>64}", token_id.trim_start_matches("0x"))
    } else {
        // Decimal token ID → convert to hex (token IDs are 256-bit, too large for u128)
        let val = alloy::primitives::U256::from_str_radix(token_id, 10)
            .map_err(|_| anyhow!("Invalid token ID"))?;
        format!("{:0>64x}", val)
    };
    let data = format!("{}{}{}", CTF_BALANCE_OF_SELECTOR, pad_address(wallet), token_id_hex);
    let result = rpc_eth_call(&env.rpc_url, CTF_CONTRACT, &data).await?;
    Ok(parse_uint256_as_f64(&result, 6)) // CTF tokens have 6 decimals
}
