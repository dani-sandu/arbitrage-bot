use crate::config::{Env, MIN_ORDER_SIZE_USD};
use crate::services::chain_reader;
use crate::services::create_clob_client::{ClobClient, OrderSide};
use crate::utils::logger::log_error;
use crate::utils::telegram::send_telegram_alert;
use anyhow::{anyhow, Result};
use colored::*;
use std::sync::Arc;

const PRICE_DECIMALS: usize = 2;
const TOKEN_DECIMALS: usize = 2;
const MIN_TOKEN_AMOUNT: f64 = 5.0;
// Minimum spread after fees to justify execution (0.5% = $0.005 per token pair)
pub const MIN_NET_SPREAD: f64 = 0.005;
// Absolute floor price for unwind sells (never go below this)
const UNWIND_PRICE_FLOOR: f64 = 0.01;

#[derive(Debug, Clone)]
pub struct ArbitrageOrderResult {
    pub success: bool,
    pub token_id: String,
    pub side: String,
    pub amount: f64,
    pub price: f64,
    pub tokens_bought: Option<f64>,
    pub order_id: Option<String>,
    pub error: Option<String>,
}

fn floor_to_decimals(value: f64, decimals: usize) -> f64 {
    let multiplier = 10_f64.powi(decimals as i32);
    (value * multiplier).floor() / multiplier
}

fn create_error_result(token_id: &str, side: &str, error: String) -> ArbitrageOrderResult {
    ArbitrageOrderResult {
        success: false,
        token_id: token_id.to_string(),
        side: side.to_string(),
        amount: 0.0,
        price: 0.0,
        tokens_bought: None,
        order_id: None,
        error: Some(error),
    }
}

/// Check if an opportunity is profitable after fees.
/// Returns (net_spread, is_profitable).
pub fn check_profitability(ask_sum: f64, threshold: f64, fee_rate: f64) -> (f64, bool) {
    let gross_spread = threshold - ask_sum;
    let fee_cost = ask_sum * fee_rate;
    let net_spread = gross_spread - fee_cost;
    (net_spread, net_spread >= MIN_NET_SPREAD)
}

/// Compute a bounded unwind sell price from the original buy price.
/// Applies `max_slippage` (e.g. 0.10 = 10%) below the buy price,
/// floored to PRICE_DECIMALS and clamped to UNWIND_PRICE_FLOOR.
fn compute_unwind_price(buy_price: f64, max_slippage: f64) -> f64 {
    let raw = buy_price * (1.0 - max_slippage);
    let floored = floor_to_decimals(raw, PRICE_DECIMALS);
    if floored < UNWIND_PRICE_FLOOR { UNWIND_PRICE_FLOOR } else { floored }
}

async fn execute_buy_order(
    clob_client: &Arc<ClobClient>,
    token_id: &str,
    side: &str,
    token_amount: f64,
    ask_price: f64,
) -> ArbitrageOrderResult {
    if token_id.trim().is_empty() {
        return create_error_result(token_id, side, "Invalid tokenId".to_string());
    }

    if ask_price <= 0.0 || !ask_price.is_finite() {
        return create_error_result(token_id, side, format!("Invalid ask price: {}", ask_price));
    }

    let floored_price = floor_to_decimals(ask_price, PRICE_DECIMALS);
    if floored_price <= 0.0 {
        return create_error_result(token_id, side, format!("Invalid floored price: {}", floored_price));
    }

    let size = floor_to_decimals(token_amount, TOKEN_DECIMALS);
    if size < MIN_TOKEN_AMOUNT {
        return create_error_result(token_id, side, format!("Size {:.2} below minimum {:.2}", size, MIN_TOKEN_AMOUNT));
    }

    let usdc_cost = floor_to_decimals(size * floored_price, PRICE_DECIMALS);
    if usdc_cost < MIN_ORDER_SIZE_USD {
        return create_error_result(
            token_id,
            side,
            format!("USDC amount (${:.2}) below minimum (${:.2})", usdc_cost, MIN_ORDER_SIZE_USD),
        );
    }

    println!(
        "{}",
        format!(
            "[{}] Submitting at ${:.4} | {:.2} tokens | ${:.4} USDC | TokenID: {}...",
            side, floored_price, size, usdc_cost, &token_id[..token_id.len().min(20)]
        )
        .cyan()
    );

    match clob_client.submit_order(token_id, OrderSide::Buy, floored_price, size).await {
        Ok(resp) => {
            let tokens_bought = size;
            println!(
                "{}",
                format!(
                    "✓ [{}] Order ID: {} | {:.2} tokens @ ${:.4}",
                    side,
                    resp.order_id.as_deref().unwrap_or("N/A"),
                    tokens_bought,
                    floored_price,
                )
                .green()
            );
            ArbitrageOrderResult {
                success: true,
                token_id: token_id.to_string(),
                side: side.to_string(),
                amount: usdc_cost,
                price: floored_price,
                tokens_bought: Some(tokens_bought),
                order_id: resp.order_id.clone(),
                error: None,
            }
        }
        Err(e) => {
            let error_msg = format!("{}", e);
            println!("{}", format!("✗ [{}] {}", side, error_msg).red());
            log_error(&error_msg, Some(&format!("execute_buy-{}", side)));
            ArbitrageOrderResult {
                success: false,
                token_id: token_id.to_string(),
                side: side.to_string(),
                amount: 0.0,
                price: 0.0,
                tokens_bought: None,
                order_id: None,
                error: Some(error_msg),
            }
        }
    }
}

/// Execute arbitrage: buy both UP and DOWN tokens in parallel.
/// `available_up` and `available_down` are the sizes available at the best ask on each side.
/// The trade size is capped to the minimum of token_amount, available_up, and available_down.
pub async fn execute_arbitrage_trade(
    clob_client: &Arc<ClobClient>,
    up_token_id: &str,
    down_token_id: &str,
    up_price: f64,
    down_price: f64,
    available_up: f64,
    available_down: f64,
    env: &Env,
) -> Result<(ArbitrageOrderResult, ArbitrageOrderResult, bool)> {
    if up_token_id.trim().is_empty() || down_token_id.trim().is_empty() {
        return Err(anyhow!("Invalid token IDs"));
    }
    if up_price <= 0.0 || !up_price.is_finite() || down_price <= 0.0 || !down_price.is_finite() {
        return Err(anyhow!("Invalid prices"));
    }

    // Cap token_amount by the available liquidity on each side
    let token_amount = floor_to_decimals(
        env.token_amount.min(available_up).min(available_down),
        TOKEN_DECIMALS,
    );
    if token_amount < MIN_TOKEN_AMOUNT {
        return Err(anyhow!(
            "Insufficient liquidity: requested {:.2}, available UP={:.2} DOWN={:.2} (min {:.2})",
            env.token_amount, available_up, available_down, MIN_TOKEN_AMOUNT
        ));
    }
    let up_usdc = floor_to_decimals(token_amount * up_price, PRICE_DECIMALS);
    let down_usdc = floor_to_decimals(token_amount * down_price, PRICE_DECIMALS);

    if up_usdc < MIN_ORDER_SIZE_USD || down_usdc < MIN_ORDER_SIZE_USD {
        return Err(anyhow!(
            "Order sizes below minimum: UP=${:.2}, DOWN=${:.2}",
            up_usdc, down_usdc
        ));
    }

    // Check profitability after fees
    let ask_sum = up_price + down_price;
    let (net_spread, profitable) = check_profitability(ask_sum, env.arbitrage_threshold, env.taker_fee_rate);
    if !profitable {
        return Err(anyhow!(
            "Not profitable after fees: net spread {:.4} (min {:.4})",
            net_spread, MIN_NET_SPREAD
        ));
    }

    println!(
        "{}",
        format!(
            "\n⚡ Executing arbitrage ({:.2} tokens each)\n  UP: ${:.4} → ${:.2} USDC\n  DOWN: ${:.4} → ${:.2} USDC\n  Net spread after fees: {:.4} ({:.2}%)\n",
            token_amount, up_price, up_usdc, down_price, down_usdc,
            net_spread, net_spread * 100.0
        )
        .green()
        .bold()
    );

    // Execute both orders in parallel to minimize leg risk
    let client_up = clob_client.clone();
    let client_down = clob_client.clone();
    let up_tid = up_token_id.to_string();
    let down_tid = down_token_id.to_string();

    let (up_result, down_result) = tokio::join!(
        execute_buy_order(&client_up, &up_tid, "UP", token_amount, up_price),
        execute_buy_order(&client_down, &down_tid, "DOWN", token_amount, down_price),
    );

    let both_success = up_result.success && down_result.success;

    if both_success {
        println!(
            "{}",
            format!(
                "\n╔═══════════════════════════════════════════════╗\n║  🎉 ARBITRAGE COMPLETED SUCCESSFULLY          ║\n╚═══════════════════════════════════════════════╝\n  UP:   {:.2} tokens @ ${:.4} = ${:.2}\n  DOWN: {:.2} tokens @ ${:.4} = ${:.2}\n  Total: ${:.2} USDC | Net profit: ~${:.4}\n",
                up_result.tokens_bought.unwrap_or(0.0), up_result.price, up_result.amount,
                down_result.tokens_bought.unwrap_or(0.0), down_result.price, down_result.amount,
                up_result.amount + down_result.amount,
                net_spread * token_amount,
            )
            .green()
            .bold()
        );
        let msg = format!(
            "🎉 ARBITRAGE COMPLETED\nUP: {:.2} tokens @ ${:.4} = ${:.2}\nDOWN: {:.2} tokens @ ${:.4} = ${:.2}\nTotal: ${:.2} USDC\nNet profit: ~${:.4}",
            up_result.tokens_bought.unwrap_or(0.0), up_result.price, up_result.amount,
            down_result.tokens_bought.unwrap_or(0.0), down_result.price, down_result.amount,
            up_result.amount + down_result.amount,
            net_spread * token_amount,
        );
        send_telegram_alert(&msg).await;
    } else {
        // Attempt to unwind the filled leg to prevent directional exposure.
        // Use a bounded exit price derived from the buy price and max_unwind_slippage,
        // instead of a hardcoded fire-sale price.
        if up_result.success && !down_result.success {
            let size = floor_to_decimals(up_result.tokens_bought.unwrap_or(0.0), TOKEN_DECIMALS);
            if size >= MIN_TOKEN_AMOUNT {
                let unwind_price = compute_unwind_price(up_result.price, env.max_unwind_slippage);
                println!(
                    "{}",
                    format!(
                        "Attempting to unwind UP leg... (sell {:.2} @ ${:.4}, max slippage {:.0}%)",
                        size, unwind_price, env.max_unwind_slippage * 100.0
                    )
                    .yellow()
                );
                match clob_client.submit_order(up_token_id, OrderSide::Sell, unwind_price, size).await {
                    Ok(_) => {
                        println!("{}", "✓ UP leg unwound".green());
                        send_telegram_alert(&format!(
                            "🔄 Unwound UP leg ({:.2} tokens @ ${:.4})", size, unwind_price
                        )).await;
                    }
                    Err(e) => {
                        let msg = format!("⚠️ UNWIND FAILED (UP): {} — {:.2} tokens may remain exposed", e, size);
                        println!("{}", msg.red());
                        send_telegram_alert(&msg).await;
                        log_error(&msg, Some("unwind_up"));
                    }
                }
            }
        } else if down_result.success && !up_result.success {
            let size = floor_to_decimals(down_result.tokens_bought.unwrap_or(0.0), TOKEN_DECIMALS);
            if size >= MIN_TOKEN_AMOUNT {
                let unwind_price = compute_unwind_price(down_result.price, env.max_unwind_slippage);
                println!(
                    "{}",
                    format!(
                        "Attempting to unwind DOWN leg... (sell {:.2} @ ${:.4}, max slippage {:.0}%)",
                        size, unwind_price, env.max_unwind_slippage * 100.0
                    )
                    .yellow()
                );
                match clob_client.submit_order(down_token_id, OrderSide::Sell, unwind_price, size).await {
                    Ok(_) => {
                        println!("{}", "✓ DOWN leg unwound".green());
                        send_telegram_alert(&format!(
                            "🔄 Unwound DOWN leg ({:.2} tokens @ ${:.4})", size, unwind_price
                        )).await;
                    }
                    Err(e) => {
                        let msg = format!("⚠️ UNWIND FAILED (DOWN): {} — {:.2} tokens may remain exposed", e, size);
                        println!("{}", msg.red());
                        send_telegram_alert(&msg).await;
                        log_error(&msg, Some("unwind_down"));
                    }
                }
            }
        }

        let error_msg = format!(
            "Arbitrage partially failed - UP: {}, DOWN: {}",
            if up_result.success { "OK".to_string() } else { up_result.error.clone().unwrap_or_default() },
            if down_result.success { "OK".to_string() } else { down_result.error.clone().unwrap_or_default() },
        );
        println!("{}", format!("⚠️  {}", error_msg).red().bold());
        log_error(&error_msg, Some("execute_arbitrage_trade"));
        let msg = format!("⚠️ ARBITRAGE FAILED\n{}", error_msg);
        send_telegram_alert(&msg).await;
    }

    // Best-effort on-chain fill verification
    verify_positions_on_chain(env, up_token_id, down_token_id, &up_result, &down_result).await;

    Ok((up_result, down_result, both_success))
}

/// Query on-chain CTF balances and log whether they match expected fills.
/// This is best-effort — failures here do not block the trade flow.
async fn verify_positions_on_chain(
    env: &Env,
    up_token_id: &str,
    down_token_id: &str,
    up_result: &ArbitrageOrderResult,
    down_result: &ArbitrageOrderResult,
) {
    // Short delay to let on-chain state settle after CLOB fill
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    let up_balance = chain_reader::get_ctf_balance(env, up_token_id).await;
    let down_balance = chain_reader::get_ctf_balance(env, down_token_id).await;

    match (up_balance, down_balance) {
        (Ok(up_bal), Ok(down_bal)) => {
            let expected_up = if up_result.success { up_result.tokens_bought.unwrap_or(0.0) } else { 0.0 };
            let expected_down = if down_result.success { down_result.tokens_bought.unwrap_or(0.0) } else { 0.0 };
            println!(
                "{}",
                format!(
                    "[VERIFY] On-chain CTF balances — UP: {:.2} (expected ~{:.2}), DOWN: {:.2} (expected ~{:.2})",
                    up_bal, expected_up, down_bal, expected_down
                )
                .bright_black()
            );
            // Alert if there's a significant mismatch (> 1 token difference)
            if up_result.success && (up_bal - expected_up).abs() > 1.0 {
                let msg = format!(
                    "⚠️ POSITION MISMATCH — UP expected ~{:.2}, on-chain {:.2}",
                    expected_up, up_bal
                );
                log_error(&msg, Some("verify_positions"));
                send_telegram_alert(&msg).await;
            }
            if down_result.success && (down_bal - expected_down).abs() > 1.0 {
                let msg = format!(
                    "⚠️ POSITION MISMATCH — DOWN expected ~{:.2}, on-chain {:.2}",
                    expected_down, down_bal
                );
                log_error(&msg, Some("verify_positions"));
                send_telegram_alert(&msg).await;
            }
        }
        _ => {
            println!(
                "{}",
                "[VERIFY] Could not query on-chain CTF balances (RPC error)".yellow()
            );
        }
    }
}
