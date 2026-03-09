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
// Absolute ceiling for buy price buffer (never exceed the complement price)
const MAX_BUFFERED_PRICE: f64 = 0.99;

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
    market_slug: &str,
    coin: &str,
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
    // Apply price buffer to improve fill probability
    // The buffer adds a small amount above the ask price so our limit order
    // is more likely to match against resting asks even if they shifted slightly.
    let buffered_up_price = (up_price + env.buy_price_buffer).min(MAX_BUFFERED_PRICE);
    let buffered_down_price = (down_price + env.buy_price_buffer).min(MAX_BUFFERED_PRICE);

    // Re-check profitability with buffered prices
    let buffered_ask_sum = buffered_up_price + buffered_down_price;
    let (net_spread, profitable) = check_profitability(buffered_ask_sum, env.arbitrage_threshold, env.taker_fee_rate);
    if !profitable {
        return Err(anyhow!(
            "Not profitable after price buffer: buffered sum {:.4} (buffer {:.2}¢ each), net spread {:.4}",
            buffered_ask_sum, env.buy_price_buffer * 100.0, net_spread
        ));
    }

    let up_usdc = floor_to_decimals(token_amount * buffered_up_price, PRICE_DECIMALS);
    let down_usdc = floor_to_decimals(token_amount * buffered_down_price, PRICE_DECIMALS);

    if up_usdc < MIN_ORDER_SIZE_USD || down_usdc < MIN_ORDER_SIZE_USD {
        return Err(anyhow!(
            "Order sizes below minimum: UP=${:.2}, DOWN=${:.2}",
            up_usdc, down_usdc
        ));
    }

    println!(
        "{}",
        format!(
            "\n⚡ Executing arbitrage [{} / {}] ({:.2} tokens each)\n  UP: ${:.4} (ask ${:.4} + {:.2}¢ buffer) → ${:.2} USDC\n  DOWN: ${:.4} (ask ${:.4} + {:.2}¢ buffer) → ${:.2} USDC\n  Net spread after fees: {:.4} ({:.2}%)\n  Mode: {}\n",
            coin, market_slug,
            token_amount, buffered_up_price, up_price, env.buy_price_buffer * 100.0, up_usdc,
            buffered_down_price, down_price, env.buy_price_buffer * 100.0, down_usdc,
            net_spread, net_spread * 100.0,
            if env.sequential_execution { "SEQUENTIAL (safer)" } else { "PARALLEL (faster)" }
        )
        .green()
        .bold()
    );

    let (up_result, down_result) = if env.sequential_execution {
        // Sequential mode: execute first leg, verify fill, then execute second leg.
        // This eliminates one-legged risk entirely.
        let up_result = execute_buy_order(clob_client, up_token_id, "UP", token_amount, buffered_up_price).await;

        if !up_result.success {
            // First leg failed to submit — no risk, just abort
            let down_result = create_error_result(down_token_id, "DOWN", "Skipped: UP leg failed to submit".to_string());
            return Ok((up_result, down_result, false));
        }

        // Verify the UP leg actually filled on-chain before committing to DOWN
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        let up_balance = chain_reader::get_ctf_balance(env, up_token_id).await.unwrap_or(0.0);
        let fill_threshold = token_amount * 0.5;

        if up_balance < fill_threshold {
            println!(
                "{}",
                format!(
                    "[SEQUENTIAL] UP leg submitted but not filled on-chain ({:.2}/{:.2}). Aborting DOWN leg.",
                    up_balance, token_amount
                ).yellow()
            );
            let down_result = create_error_result(down_token_id, "DOWN", "Skipped: UP leg not filled on-chain".to_string());
            return Ok((up_result, down_result, false));
        }

        println!(
            "{}",
            format!("[SEQUENTIAL] UP leg confirmed on-chain ({:.2} tokens). Executing DOWN leg...", up_balance).green()
        );

        let down_result = execute_buy_order(clob_client, down_token_id, "DOWN", token_amount, buffered_down_price).await;
        (up_result, down_result)
    } else {
        // Parallel mode: execute both legs simultaneously (original behavior)
        let client_up = clob_client.clone();
        let client_down = clob_client.clone();
        let up_tid = up_token_id.to_string();
        let down_tid = down_token_id.to_string();

        tokio::join!(
            execute_buy_order(&client_up, &up_tid, "UP", token_amount, buffered_up_price),
            execute_buy_order(&client_down, &down_tid, "DOWN", token_amount, buffered_down_price),
        )
    };

    let both_submitted = up_result.success && down_result.success;
    let mut both_filled = false;

    if both_submitted {
        // Both orders accepted by CLOB — verify actual fills on-chain before celebrating.
        // FAK orders can be accepted but not filled if liquidity disappeared.
        let (up_actual, down_actual) = poll_fill_balances(
            env, up_token_id, down_token_id, token_amount,
        ).await;

        let fill_threshold = token_amount * 0.5;
        let up_filled = up_actual >= fill_threshold;
        let down_filled = down_actual >= fill_threshold;

        if up_filled && down_filled {
            both_filled = true;
            println!(
                "{}",
                format!(
                    "\n╔═══════════════════════════════════════════════╗\n║  🎉 ARBITRAGE COMPLETED SUCCESSFULLY          ║\n╚═══════════════════════════════════════════════╝\n  UP:   {:.2} tokens @ ${:.4} = ${:.2} (on-chain: {:.2})\n  DOWN: {:.2} tokens @ ${:.4} = ${:.2} (on-chain: {:.2})\n  Total: ${:.2} USDC | Net profit: ~${:.4}\n",
                    up_result.tokens_bought.unwrap_or(0.0), up_result.price, up_result.amount, up_actual,
                    down_result.tokens_bought.unwrap_or(0.0), down_result.price, down_result.amount, down_actual,
                    up_result.amount + down_result.amount,
                    net_spread * token_amount,
                )
                .green()
                .bold()
            );
            let msg = format!(
                "🎉 ARBITRAGE COMPLETED\n📊 {} | {}\nUP: {:.2} tokens @ ${:.4} = ${:.2}\nDOWN: {:.2} tokens @ ${:.4} = ${:.2}\nTotal: ${:.2} USDC\nNet profit: ~${:.4}",
                coin, market_slug,
                up_result.tokens_bought.unwrap_or(0.0), up_result.price, up_result.amount,
                down_result.tokens_bought.unwrap_or(0.0), down_result.price, down_result.amount,
                up_result.amount + down_result.amount,
                net_spread * token_amount,
            );
            send_telegram_alert(&msg).await;
        } else {
            // One or both orders were accepted but NOT filled on-chain
            let fail_detail = format!(
                "ON-CHAIN FILL MISMATCH — UP: {:.2}/{:.2}, DOWN: {:.2}/{:.2}",
                up_actual, token_amount, down_actual, token_amount
            );
            println!("{}", format!("⚠️  {}", fail_detail).red().bold());
            log_error(&fail_detail, Some("fill_verification"));

            attempt_unwind_after_partial_fill(
                clob_client, env,
                up_token_id, down_token_id,
                &up_result, &down_result,
                up_filled, down_filled,
                up_actual, down_actual,                market_slug, coin,            ).await;

            let msg = format!("⚠️ ARBITRAGE FILL FAILED\n📊 {} | {}\n{}", coin, market_slug, fail_detail);
            send_telegram_alert(&msg).await;
        }
    } else {
        // One order failed to even submit to CLOB — unwind the submitted leg
        attempt_unwind_after_partial_fill(
            clob_client, env,
            up_token_id, down_token_id,
            &up_result, &down_result,
            up_result.success, down_result.success,
            if up_result.success { up_result.tokens_bought.unwrap_or(0.0) } else { 0.0 },
            if down_result.success { down_result.tokens_bought.unwrap_or(0.0) } else { 0.0 },
            market_slug, coin,
        ).await;

        let error_msg = format!(
            "Arbitrage submission failed - UP: {}, DOWN: {}",
            if up_result.success { "OK".to_string() } else { up_result.error.clone().unwrap_or_default() },
            if down_result.success { "OK".to_string() } else { down_result.error.clone().unwrap_or_default() },
        );
        println!("{}", format!("⚠️  {}", error_msg).red().bold());
        log_error(&error_msg, Some("execute_arbitrage_trade"));
        let msg = format!("⚠️ ARBITRAGE FAILED\n📊 {} | {}\n{}", coin, market_slug, error_msg);
        send_telegram_alert(&msg).await;
    }

    Ok((up_result, down_result, both_filled))
}

/// Poll on-chain CTF balances to verify actual fills after order submission.
/// Retries a few times with delays to account for on-chain settlement lag.
/// Returns (up_balance, down_balance).
async fn poll_fill_balances(
    env: &Env,
    up_token_id: &str,
    down_token_id: &str,
    expected_tokens: f64,
) -> (f64, f64) {
    const MAX_ATTEMPTS: u32 = 5;
    const DELAY_SECS: u64 = 3;
    let fill_threshold = expected_tokens * 0.5;

    for attempt in 0..MAX_ATTEMPTS {
        tokio::time::sleep(tokio::time::Duration::from_secs(DELAY_SECS)).await;

        let (up_bal_res, down_bal_res) = tokio::join!(
            chain_reader::get_ctf_balance(env, up_token_id),
            chain_reader::get_ctf_balance(env, down_token_id),
        );

        let up_bal = up_bal_res.unwrap_or(0.0);
        let down_bal = down_bal_res.unwrap_or(0.0);

        println!(
            "{}",
            format!(
                "[FILL CHECK {}/{}] UP: {:.2}, DOWN: {:.2} (expected ~{:.2} each)",
                attempt + 1, MAX_ATTEMPTS, up_bal, down_bal, expected_tokens
            )
            .bright_black()
        );

        // Both sufficiently filled → return early
        if up_bal >= fill_threshold && down_bal >= fill_threshold {
            return (up_bal, down_bal);
        }
    }

    // Final attempt balances (re-read to avoid returning stale 0s)
    let up_final = chain_reader::get_ctf_balance(env, up_token_id).await.unwrap_or(0.0);
    let down_final = chain_reader::get_ctf_balance(env, down_token_id).await.unwrap_or(0.0);
    (up_final, down_final)
}

/// Two-stage unwind: first attempt at configured slippage, then retry at floor price.
async fn try_unwind_sell(
    clob_client: &Arc<ClobClient>,
    token_id: &str,
    label: &str,
    size: f64,
    buy_price: f64,
    max_slippage: f64,
    market_slug: &str,
    coin: &str,
) -> bool {
    // Stage 1: configured slippage
    let stage1_price = compute_unwind_price(buy_price, max_slippage);
    println!(
        "{}",
        format!(
            "[UNWIND-1] {} sell {:.2} @ ${:.4} ({:.0}% slippage)",
            label, size, stage1_price, max_slippage * 100.0
        ).yellow()
    );
    match clob_client.submit_order(token_id, OrderSide::Sell, stage1_price, size).await {
        Ok(_) => {
            println!("{}", format!("✓ {} leg unwound (stage 1)", label).green());
            send_telegram_alert(&format!(
                "🔄 Unwound {} leg ({:.2} tokens @ ${:.4})\n📊 {} | {}", label, size, stage1_price, coin, market_slug
            )).await;
            return true;
        }
        Err(e) => {
            println!("{}", format!("✗ Stage 1 unwind failed: {}", e).red());
        }
    }

    // Stage 2: emergency sell at floor price ($0.01) to exit at any cost
    println!(
        "{}",
        format!(
            "[UNWIND-2] {} emergency sell {:.2} @ ${:.4} (floor price)",
            label, size, UNWIND_PRICE_FLOOR
        ).red().bold()
    );
    match clob_client.submit_order(token_id, OrderSide::Sell, UNWIND_PRICE_FLOOR, size).await {
        Ok(_) => {
            println!("{}", format!("✓ {} leg unwound (stage 2 — emergency)", label).green());
            send_telegram_alert(&format!(
                "🚨 Emergency unwind {} ({:.2} tokens @ ${:.4})\n📊 {} | {}", label, size, UNWIND_PRICE_FLOOR, coin, market_slug
            )).await;
            return true;
        }
        Err(e) => {
            let msg = format!("⚠️ UNWIND FAILED ({}) [{}|{}]: {} — {:.2} tokens remain exposed", label, coin, market_slug, e, size);
            println!("{}", msg.red());
            send_telegram_alert(&msg).await;
            log_error(&msg, Some(&format!("unwind_{}", label.to_lowercase())));
            return false;
        }
    }
}

/// Attempt to retry the unfilled leg before resorting to unwind.
/// Returns true if the retry succeeded and both legs are now filled.
///
/// SAFETY: Before each retry, we check the on-chain balance to see if
/// previous orders already filled (settlement can lag behind CLOB execution).
/// This prevents accumulating 2-3× the intended position when FAK orders
/// settle after the balance check window.
async fn retry_unfilled_leg(
    clob_client: &Arc<ClobClient>,
    env: &Env,
    unfilled_token_id: &str,
    label: &str,
    token_amount: f64,
    original_price: f64,
    market_slug: &str,
    coin: &str,
) -> bool {
    const MAX_RETRIES: u32 = 2;
    const RETRY_DELAY_MS: u64 = 3000;
    let fill_threshold = token_amount * 0.5;

    for attempt in 1..=MAX_RETRIES {
        // Wait before each retry to give on-chain settlement time to catch up.
        // This is critical: FAK orders fill immediately on CLOB but the on-chain
        // CTF balance can lag by several seconds.
        tokio::time::sleep(tokio::time::Duration::from_millis(RETRY_DELAY_MS)).await;

        // Pre-check: if previous order(s) already settled, skip retry
        let existing_balance = chain_reader::get_ctf_balance(env, unfilled_token_id).await.unwrap_or(0.0);
        if existing_balance >= fill_threshold {
            println!(
                "{}",
                format!(
                    "✓ [RETRY] {} leg already filled before retry {} ({:.2} tokens on-chain)",
                    label, attempt, existing_balance
                ).green()
            );
            return true;
        }

        // Only retry for the remaining unfilled amount
        let remaining = floor_to_decimals(token_amount - existing_balance, TOKEN_DECIMALS);
        if remaining < MIN_TOKEN_AMOUNT {
            println!(
                "{}",
                format!(
                    "✓ [RETRY] {} leg mostly filled ({:.2}/{:.2}), remaining {:.2} below minimum — treating as filled",
                    label, existing_balance, token_amount, remaining
                ).green()
            );
            return true;
        }

        println!(
            "{}",
            format!(
                "[RETRY {}/{}] Re-submitting {} leg ({:.2} tokens @ ${:.4}, existing: {:.2})...",
                attempt, MAX_RETRIES, label, remaining, original_price, existing_balance
            ).yellow()
        );

        let retry_result = execute_buy_order(
            clob_client, unfilled_token_id, label, remaining, original_price,
        ).await;

        if retry_result.success {
            // Verify on-chain with longer wait for settlement
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            let balance = chain_reader::get_ctf_balance(env, unfilled_token_id).await.unwrap_or(0.0);
            if balance >= fill_threshold {
                println!(
                    "{}",
                    format!(
                        "✓ [RETRY] {} leg filled on retry {} ({:.2} tokens on-chain)",
                        label, attempt, balance
                    ).green()
                );
                send_telegram_alert(&format!(
                    "🔄 {} leg filled on retry {} ({:.2} tokens)\n📊 {} | {}", label, attempt, balance, coin, market_slug
                )).await;
                return true;
            }
        }
    }

    println!(
        "{}",
        format!("[RETRY] {} leg failed after {} attempts — proceeding to unwind", label, MAX_RETRIES).red()
    );
    false
}

/// Attempt to unwind a single filled leg when the other side didn't fill.
/// First tries to retry the unfilled leg; only unwinds if retries fail.
async fn attempt_unwind_after_partial_fill(
    clob_client: &Arc<ClobClient>,
    env: &Env,
    up_token_id: &str,
    down_token_id: &str,
    up_result: &ArbitrageOrderResult,
    down_result: &ArbitrageOrderResult,
    up_filled: bool,
    down_filled: bool,
    up_actual: f64,
    down_actual: f64,
    market_slug: &str,
    coin: &str,
) {
    if up_filled && !down_filled {
        // UP filled, DOWN didn't — retry DOWN first
        let down_size = floor_to_decimals(up_actual, TOKEN_DECIMALS); // match UP's fill
        let retry_ok = retry_unfilled_leg(
            clob_client, env, down_token_id, "DOWN", down_size, down_result.price,
            market_slug, coin,
        ).await;

        if !retry_ok {
            // Final safety check before unwinding: re-read DOWN balance one more time.
            // FAK orders can settle on-chain after the retry window closed.
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            let final_down_bal = chain_reader::get_ctf_balance(env, down_token_id).await.unwrap_or(0.0);
            if final_down_bal >= down_size * 0.5 {
                println!(
                    "{}",
                    format!(
                        "✓ [UNWIND ABORTED] DOWN leg settled late ({:.2} tokens on-chain) — both legs filled",
                        final_down_bal
                    ).green()
                );
            } else {
                // Retry failed and DOWN truly unfilled — unwind UP using UP's buy price
                let size = floor_to_decimals(up_actual, TOKEN_DECIMALS);
                if size >= MIN_TOKEN_AMOUNT {
                    try_unwind_sell(
                        clob_client, up_token_id, "UP", size,
                        up_result.price, env.max_unwind_slippage,
                        market_slug, coin,
                    ).await;
                }
            }
        }
    } else if down_filled && !up_filled {
        // DOWN filled, UP didn't — retry UP first
        let up_size = floor_to_decimals(down_actual, TOKEN_DECIMALS); // match DOWN's fill
        let retry_ok = retry_unfilled_leg(
            clob_client, env, up_token_id, "UP", up_size, up_result.price,
            market_slug, coin,
        ).await;

        if !retry_ok {
            // Final safety check before unwinding: re-read UP balance one more time.
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            let final_up_bal = chain_reader::get_ctf_balance(env, up_token_id).await.unwrap_or(0.0);
            if final_up_bal >= up_size * 0.5 {
                println!(
                    "{}",
                    format!(
                        "✓ [UNWIND ABORTED] UP leg settled late ({:.2} tokens on-chain) — both legs filled",
                        final_up_bal
                    ).green()
                );
            } else {
                // Retry failed and UP truly unfilled — unwind DOWN using DOWN's buy price
                let size = floor_to_decimals(down_actual, TOKEN_DECIMALS);
                if size >= MIN_TOKEN_AMOUNT {
                    try_unwind_sell(
                        clob_client, down_token_id, "DOWN", size,
                        down_result.price, env.max_unwind_slippage,
                        market_slug, coin,
                    ).await;
                }
            }
        }
    }

    // Log post-reconciliation USDC balance
    if let Ok(bal) = chain_reader::get_usdc_balance(env).await {
        println!(
            "{}",
            format!("[RECONCILE] Post-unwind USDC.e balance: ${:.2}", bal).bright_black()
        );
    }
}
