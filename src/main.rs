mod config;
mod services;
mod utils;

use crate::config::Env;
use crate::services::market_discovery::{find_15_min_market, CoinMarket};
use crate::services::price_monitor::{create_price_data, display_coin_details, PriceMonitor};
use crate::services::arbitrage_executor::check_profitability;
use crate::services::persistent_state::{BotPersistentState, TradeRecord};
use crate::services::velocity::VelocityLockout;
use crate::services::websocket_client::MarketWebSocket;
use crate::utils::logger::{clear_log_files, init_monitor_log};
use crate::utils::telegram::send_telegram_alert;
use colored::*;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};

/// Mask credentials in a proxy URL for safe logging.
fn mask_proxy_url(url: &str) -> String {
    if let Ok(parsed) = url::Url::parse(url) {
        if parsed.username() != "" || parsed.password().is_some() {
            let host_port = if let Some(port) = parsed.port() {
                format!("{}:{}", parsed.host_str().unwrap_or("?"), port)
            } else {
                parsed.host_str().unwrap_or("?").to_string()
            };
            return format!("{}://***:***@{}", parsed.scheme(), host_port);
        }
    }
    url.to_string()
}

/// Test SOCKS5 proxy connectivity by making a simple GET through it.
async fn test_socks5_proxy(proxy_url: &str) -> anyhow::Result<()> {
    let proxy = reqwest::Proxy::all(proxy_url)
        .map_err(|e| anyhow::anyhow!("Invalid proxy URL: {}", e))?;
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build proxy client: {}", e))?;
    client
        .get("https://clob.polymarket.com/time")
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let env = Env::load();

    if let Some(ref proxy_url) = env.socks5_proxy_url {
        println!("{}", format!("🌐 SOCKS5 proxy configured for CLOB orders: {}", mask_proxy_url(proxy_url)).cyan());
        // Quick connectivity test through the proxy
        match test_socks5_proxy(proxy_url).await {
            Ok(_) => println!("{}", "  ✓ Proxy connectivity OK".green()),
            Err(e) => println!("{}", format!("  ⚠️  Proxy test failed: {}. CLOB client may fail to initialize.", e).yellow()),
        }
    } else if std::env::var("HTTPS_PROXY").ok().filter(|s| !s.is_empty()).is_some() {
        let proxy_val = std::env::var("HTTPS_PROXY").unwrap();
        println!("{}", format!("🌐 HTTPS_PROXY detected for CLOB orders: {}", mask_proxy_url(&proxy_val)).cyan());
        match test_socks5_proxy(&proxy_val).await {
            Ok(_) => println!("{}", "  ✓ Proxy connectivity OK".green()),
            Err(e) => println!("{}", format!("  ⚠️  Proxy test failed: {}. CLOB client may fail to initialize.", e).yellow()),
        }
    }
    
    println!("{}", "\n╔════════════════════════════════════════════════════════════════╗".cyan().bold());
    println!("{}", "║     Polymarket Arbitrage Bot - 15-Minute Market Monitor       ║".cyan().bold());
    println!("{}", "╚════════════════════════════════════════════════════════════════╝\n".cyan().bold());

    clear_log_files();
    init_monitor_log();
    println!("{}", "Log files cleared (monitor.log, error.log)\n".bright_black());

    let selected_coin = env.market_asset.clone();

    let startup_msg = format!(
        "🚀 Arbitrage Bot Started!\nAsset: {}\nThreshold: {}\nToken Amount: {}\nProxy: {}",
        selected_coin, env.arbitrage_threshold, env.token_amount,
        if env.socks5_proxy_url.is_some() { "SOCKS5" } else { "none" }
    );
    send_telegram_alert(&startup_msg).await;

    // Ensure USDC.e and CTF approvals are set before trading
    if env.private_key.is_some() {
        if let Err(e) = services::approvals::ensure_approvals(&env).await {
            eprintln!("{}", format!("⚠️  Approval check failed: {}. Trading may fail.", e).yellow());
        }
    }

    // Load persistent state (survives restarts)
    let persistent_state = Arc::new(Mutex::new(BotPersistentState::load()));
    {
        let state = persistent_state.lock().await;
        if state.total_trades > 0 {
            println!(
                "{}",
                format!(
                    "📊 Persistent state: {} previous trades, cumulative PnL: ${:.4}",
                    state.total_trades, state.cumulative_pnl
                )
                .bright_black()
            );
        }
    }

    // Check USDC balance and cap token_amount
    let mut effective_env = env.clone();
    match services::chain_reader::get_usdc_balance(&env).await {
        Ok(balance) => {
            println!(
                "{}",
                format!("💰 USDC.e balance: ${:.2}", balance).bright_black()
            );
            // Need enough for both sides: token_amount * max_ask_price per side
            // Worst case each side costs token_amount * 1.0, so need 2x token_amount
            let affordable = balance / 2.0;
            if affordable < effective_env.token_amount {
                let old = effective_env.token_amount;
                effective_env.token_amount = affordable;
                println!(
                    "{}",
                    format!(
                        "⚠️  Capping token_amount from {:.2} to {:.2} based on wallet balance",
                        old, affordable
                    )
                    .yellow()
                );
            }
            persistent_state.lock().await.last_usdc_balance = balance;
        }
        Err(e) => {
            println!(
                "{}",
                format!("⚠️  Could not check USDC balance: {}. Using configured token_amount.", e).yellow()
            );
        }
    }
    let env = effective_env;

    println!(
        "{}",
        format!(
            "\n✓ Coin selected: {}\n  Bot will automatically switch to next market when current market closes.\n  Press Ctrl+C to stop.\n\n",
            selected_coin
        )
        .green()
        .bold()
    );

    tokio::select! {
        result = monitor_market_loop(&selected_coin, &env, &persistent_state) => {
            if let Err(e) = result {
                eprintln!("{}", format!("Fatal error: {}", e).red());
            }
        }
        _ = tokio::signal::ctrl_c() => {
            println!("\n{}", "Shutting down gracefully...".yellow());
            send_telegram_alert("\u{1f6d1} Arbitrage Bot stopped.").await;
        }
    }

    Ok(())
}

async fn monitor_market_loop(coin: &str, env: &Env, persistent_state: &Arc<Mutex<BotPersistentState>>) -> anyhow::Result<()> {
    let mut ws: Option<Arc<MarketWebSocket>> = None;
    let clob_client: Arc<Mutex<Option<Arc<services::create_clob_client::ClobClient>>>> =
        Arc::new(Mutex::new(None));
    let monitor = Arc::new(Mutex::new(PriceMonitor::new()));
    let recent_opportunities = Arc::new(Mutex::new(HashSet::<String>::new()));
    let is_executing_trade = Arc::new(Mutex::new(false));
    let velocity = Arc::new(Mutex::new(VelocityLockout::new(
        env.velocity_threshold,
        3000, // 3s window
        env.velocity_lockout_secs * 1000,
    )));
    let last_close_warn_ms: Arc<Mutex<i64>> = Arc::new(Mutex::new(0));

    loop {
        // Stop old WebSocket so a fresh one is created for the new market
        if let Some(ref old_ws) = ws {
            old_ws.stop().await;
        }
        ws = None;
        recent_opportunities.lock().await.clear();

        match discover_and_monitor(coin, &mut ws, &clob_client, &monitor, &recent_opportunities, &is_executing_trade, &velocity, persistent_state, &last_close_warn_ms, env).await {
            Ok(Some(market)) => {
                loop {
                    let end_date = chrono::DateTime::parse_from_rfc3339(&market.end_date)
                        .unwrap_or_else(|_| chrono::Utc::now().into())
                        .with_timezone(&chrono::Utc);
                    let now = chrono::Utc::now();
                    let time_until_end = (end_date - now).num_milliseconds();

                    if time_until_end <= 0 {
                        println!(
                            "{}",
                            format!(
                                "\n\n╔════════════════════════════════════════════════╗\n║              MARKET CLOSED                     ║\n╚════════════════════════════════════════════════╝\n  Market: {}\n  Coin: {}\n  Status: Searching for next market...\n",
                                market.slug, coin
                            )
                            .yellow()
                            .bold()
                        );
                        break;
                    }

                    sleep(Duration::from_secs(1)).await;
                }
            }
            Ok(None) => {
                println!("{}", "Waiting 10 seconds before retrying...\n".yellow());
                sleep(Duration::from_secs(10)).await;
            }
            Err(e) => {
                eprintln!("{}", format!("Error: {}", e).red());
                sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

async fn discover_and_monitor(
    coin: &str,
    ws: &mut Option<Arc<MarketWebSocket>>,
    clob_client: &Arc<Mutex<Option<Arc<services::create_clob_client::ClobClient>>>>,
    monitor: &Arc<Mutex<PriceMonitor>>,
    recent_opportunities: &Arc<Mutex<HashSet<String>>>,
    is_executing_trade: &Arc<Mutex<bool>>,
    velocity: &Arc<Mutex<VelocityLockout>>,
    persistent_state: &Arc<Mutex<BotPersistentState>>,
    last_close_warn_ms: &Arc<Mutex<i64>>,
    env: &Env,
) -> anyhow::Result<Option<Arc<CoinMarket>>> {
    println!("{}", format!("\n🔍 Discovering market for {}...\n", coin).cyan());

    // Initialize ClobClient if needed
    {
        let mut client_guard = clob_client.lock().await;
        if client_guard.is_none() {
            println!("{}", "Initializing ClobClient for trading...\n".bright_black());
            match services::create_clob_client::create_clob_client(env).await {
                Ok(client) => {
                    *client_guard = Some(Arc::new(client));
                    println!("{}", "✓ ClobClient initialized\n".green());
                }
                Err(e) => {
                    println!("{}", format!("⚠️  Warning: Failed to initialize ClobClient: {}\n", e).yellow());
                    println!("{}", "Arbitrage detection will work, but automatic trading is disabled.\n".yellow());
                }
            }
        }
    }

    let market = match find_15_min_market(coin).await? {
        Some(m) => Arc::new(m),
        None => {
            println!("{}", format!("⚠️  No active market found for {}. Will retry in 10 seconds...\n", coin).yellow());
            return Ok(None);
        }
    };

    println!("{}", format!("✓ Market found: {}\n", market.slug).green());

    // Fresh WebSocket for each market cycle
    println!("{}", "Initializing WebSocket connection...\n".bright_black());
    let ws_client = Arc::new(MarketWebSocket::new(env.clob_ws_url.clone()));

    // Subscribe before connecting so the WS sends subscriptions on connect
    ws_client.subscribe(vec![market.up_token_id.clone(), market.down_token_id.clone()]).await?;

    // Set up orderbook callback
    let monitor_clone = monitor.clone();
    let clob_client_clone = clob_client.clone();
    let recent_opps_clone = recent_opportunities.clone();
    let is_executing_clone = is_executing_trade.clone();
    let velocity_clone = velocity.clone();
    let persistent_state_clone = persistent_state.clone();
    let market_clone = market.clone();
    let coin_str = coin.to_string();
    let env_clone = env.clone();
    let ws_ref_clone = ws_client.clone();
    let last_close_warn_clone = last_close_warn_ms.clone();

    ws_client.set_on_book(move |snapshot| {
        let market = market_clone.clone();
        let coin = coin_str.clone();
        let monitor = monitor_clone.clone();
        let clob_client = clob_client_clone.clone();
        let recent_opps = recent_opps_clone.clone();
        let is_executing = is_executing_clone.clone();
        let velocity = velocity_clone.clone();
        let persistent_state = persistent_state_clone.clone();
        let env = env_clone.clone();
        let ws_ref = ws_ref_clone.clone();
        let last_close_warn = last_close_warn_clone.clone();

        tokio::spawn(async move {
            let end_date = chrono::DateTime::parse_from_rfc3339(&market.end_date)
                .unwrap_or_else(|_| chrono::Utc::now().into())
                .with_timezone(&chrono::Utc);
            let now = chrono::Utc::now();
            let time_until_end = (end_date - now).num_milliseconds();

            if time_until_end <= 0 {
                return;
            }

            let is_up_token = snapshot.asset_id == market.up_token_id;
            let is_down_token = snapshot.asset_id == market.down_token_id;
            if !is_up_token && !is_down_token {
                return;
            }

            let up_snapshot = ws_ref.get_orderbook(&market.up_token_id).await;
            let down_snapshot = ws_ref.get_orderbook(&market.down_token_id).await;

            if let (Some(up_snap), Some(down_snap)) = (up_snapshot, down_snapshot) {
                let price_data = create_price_data(&coin, Some(&up_snap), Some(&down_snap), &env);

                // Feed velocity tracker with every update (only when enabled)
                if env.velocity_enabled {
                    velocity.lock().await.update(price_data.ask_sum);
                }

                if time_until_end > 0 && time_until_end < 60000 {
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    let mut last_warn = last_close_warn.lock().await;
                    // Only emit once every 10 seconds to avoid log flooding
                    if now_ms - *last_warn >= 10_000 {
                        *last_warn = now_ms;
                        let secs = time_until_end / 1000;
                        println!(
                            "{}",
                            format!(
                                "\n⚠️  MARKET CLOSING SOON - {}\n   {} seconds remaining.\n",
                                coin, secs
                            )
                            .yellow()
                            .bold()
                        );
                    }
                }

                // Arbitrage detection with fee-aware check
                // Skip if either side has no asks (price 0.0 = empty orderbook, not a real opportunity)
                if price_data.up_ask > 0.0 && price_data.down_ask > 0.0 && price_data.ask_sum < env.arbitrage_threshold {
                    let (net_spread, profitable) = check_profitability(price_data.ask_sum, env.arbitrage_threshold, env.taker_fee_rate);

                    let mut monitor_guard = monitor.lock().await;
                    monitor_guard.record_arbitrage(&coin, &price_data);
                    drop(monitor_guard);

                    let gross_spread = (env.arbitrage_threshold - price_data.ask_sum) * 100.0;
                    let timestamp = chrono::Utc::now().format("%H:%M:%S");
                    println!(
                        "{}",
                        format!(
                            "\n⚡ [{}] ARBITRAGE DETECTED - {}\n   UP_ASK: {:.4} + DOWN_ASK: {:.4} = {:.4}\n   Gross: {:.2}% | Net after fees: {:.4}\n   {}",
                            timestamp, coin, price_data.up_ask, price_data.down_ask, price_data.ask_sum, gross_spread, net_spread,
                            if profitable { "✓ PROFITABLE" } else { "✗ Not profitable after fees" }
                        )
                        .green()
                        .bold()
                    );

                    // === GUARD 1: Profitability ===
                    if !profitable {
                        // skip
                    }
                    // === GUARD 2: Velocity lockout ===
                    else if env.velocity_enabled && velocity.lock().await.is_locked() {
                        let mut vel = velocity.lock().await;
                        vel.record_blocked(net_spread);
                        println!(
                            "{}",
                            format!(
                                "[GUARD] Skipping: velocity lockout active (blocked {} opps worth ~${:.4} total)",
                                vel.blocked_count, vel.blocked_spread_total
                            ).yellow()
                        );
                    }
                    // === GUARD 3: Spread guard — skip if either side's spread is too wide ===
                    else if price_data.up_spread > env.max_spread || price_data.down_spread > env.max_spread {
                        println!(
                            "{}",
                            format!(
                                "[GUARD] Skipping: spread too wide (UP: {:.4}, DOWN: {:.4}, max: {:.4})",
                                price_data.up_spread, price_data.down_spread, env.max_spread
                            )
                            .yellow()
                        );
                    }
                    // === GUARD 4: Liquidity check — need at least MIN_TOKEN_AMOUNT on each side ===
                    else if price_data.up_ask_size < 5.0 || price_data.down_ask_size < 5.0 {
                        println!(
                            "{}",
                            format!(
                                "[GUARD] Skipping: thin liquidity (UP: {:.2}, DOWN: {:.2} tokens available)",
                                price_data.up_ask_size, price_data.down_ask_size
                            )
                            .yellow()
                        );
                    }
                    // === GUARD 5: Book depth — require minimum ask levels to avoid phantom liquidity ===
                    else if price_data.up_ask_depth < env.min_book_depth || price_data.down_ask_depth < env.min_book_depth {
                        println!(
                            "{}",
                            format!(
                                "[GUARD] Skipping: shallow orderbook (UP: {} levels, DOWN: {} levels, min: {})",
                                price_data.up_ask_depth, price_data.down_ask_depth, env.min_book_depth
                            )
                            .yellow()
                        );
                    }
                    // === GUARD 6: Bid-side liquidity — need bids for unwind if one leg fails ===
                    else if price_data.up_bid_depth == 0 || price_data.down_bid_depth == 0 {
                        println!(
                            "{}",
                            format!(
                                "[GUARD] Skipping: no bid-side liquidity for unwind (UP bids: {} levels, DOWN bids: {} levels)",
                                price_data.up_bid_depth, price_data.down_bid_depth
                            )
                            .yellow()
                        );
                    }
                    // === GUARD 7: Total ask depth — require enough total size, not just top-of-book ===
                    else if price_data.up_total_ask_size < env.token_amount || price_data.down_total_ask_size < env.token_amount {
                        println!(
                            "{}",
                            format!(
                                "[GUARD] Skipping: insufficient total ask depth (UP: {:.2}, DOWN: {:.2}, need: {:.2})",
                                price_data.up_total_ask_size, price_data.down_total_ask_size, env.token_amount
                            )
                            .yellow()
                        );
                    }
                    else {
                        // All guards passed — attempt trade
                        let opportunity_key = market.slug.clone();
                        let is_market_open = time_until_end > 90_000;

                        // === GUARD 8: Persistent state — check if already traded this market ===
                        let already_traded = persistent_state.lock().await.was_traded(&opportunity_key);

                        let client_arc = {
                            let guard = clob_client.lock().await;
                            guard.clone()
                        };

                        if let Some(client) = client_arc {
                            let should_trade = {
                                let mut is_exec = is_executing.lock().await;
                                let opps = recent_opps.lock().await;
                                if !*is_exec && !opps.contains(&opportunity_key) && !already_traded && is_market_open {
                                    *is_exec = true;
                                    true
                                } else {
                                    false
                                }
                            };

                            if should_trade {
                                let is_exec_clone = is_executing.clone();
                                let recent_opps_for_trade = recent_opps.clone();
                                let persistent_state_for_trade = persistent_state.clone();
                                let env_trade = env.clone();
                                let opp_key = opportunity_key.clone();
                                let available_up = price_data.up_ask_size;
                                let available_down = price_data.down_ask_size;

                                let market_trade = market.clone();
                                let ws_for_trade = ws_ref.clone();
                                let market_end_epoch_ms = end_date.timestamp_millis();

                                tokio::spawn(async move {
                                    // Re-check prices to avoid trading on stale data
                                    let up_ob = ws_for_trade.get_orderbook(&market_trade.up_token_id).await;
                                    let down_ob = ws_for_trade.get_orderbook(&market_trade.down_token_id).await;

                                    let mut traded = false;
                                    if let (Some(up_snap), Some(down_snap)) = (up_ob, down_ob) {
                                        let fresh_up = up_snap.asks.first().map(|a| a.price).unwrap_or(0.0);
                                        let fresh_down = down_snap.asks.first().map(|a| a.price).unwrap_or(0.0);
                                        let fresh_up_size = up_snap.asks.first().map(|a| a.size).unwrap_or(0.0);
                                        let fresh_down_size = down_snap.asks.first().map(|a| a.size).unwrap_or(0.0);

                                        if fresh_up > 0.0 && fresh_down > 0.0 {
                                            let (net_spread, still_profitable) = check_profitability(
                                                fresh_up + fresh_down,
                                                env_trade.arbitrage_threshold,
                                                env_trade.taker_fee_rate,
                                            );
                                            if still_profitable {
                                                match services::arbitrage_executor::execute_arbitrage_trade(
                                                    &client,
                                                    &ws_for_trade,
                                                    &market_trade.up_token_id,
                                                    &market_trade.down_token_id,
                                                    fresh_up,
                                                    fresh_down,
                                                    fresh_up_size.min(available_up),
                                                    fresh_down_size.min(available_down),
                                                    &env_trade,
                                                    &market_trade.slug,
                                                    &market_trade.coin,
                                                    market_end_epoch_ms,
                                                ).await {
                                                    Ok((up_res, down_res, both_ok)) => {
                                                        traded = both_ok || up_res.success || down_res.success;
                                                        // On-chain reconciliation (best-effort)
                                                        let token_amount = env_trade.token_amount
                                                            .min(fresh_up_size.min(available_up))
                                                            .min(fresh_down_size.min(available_down));
                                                        let estimated_pnl = net_spread * token_amount;
                                                        let unwind_attempted = !both_ok && (up_res.success || down_res.success);

                                                        let mut ps = persistent_state_for_trade.lock().await;
                                                        if both_ok {
                                                            ps.record_trade(&opp_key, estimated_pnl);
                                                        } else if unwind_attempted {
                                                            // Mark as traded even on partial fill to prevent re-entry
                                                            ps.record_trade(&opp_key, 0.0);
                                                        }
                                                        ps.record_trade_detail(TradeRecord {
                                                            timestamp: chrono::Utc::now().to_rfc3339(),
                                                            market_slug: opp_key.clone(),
                                                            both_filled: both_ok,
                                                            up_order_id: up_res.order_id.clone(),
                                                            down_order_id: down_res.order_id.clone(),
                                                            estimated_pnl,
                                                            unwind_attempted,
                                                        });
                                                        drop(ps);

                                                        if both_ok {
                                                            // Best-effort on-chain balance check
                                                            if let Ok(bal) = services::chain_reader::get_usdc_balance(&env_trade).await {
                                                                println!(
                                                                    "{}",
                                                                    format!("[RECONCILE] Post-trade USDC.e balance: ${:.2}", bal)
                                                                        .bright_black()
                                                                );
                                                            }
                                                        }
                                                    }
                                                    Err(e) => {
                                                        println!("{}", format!("Trade error: {}", e).red());
                                                    }
                                                }
                                            } else {
                                                println!("{}", format!(
                                                    "Opportunity vanished before execution (UP: {:.4}, DOWN: {:.4}, sum: {:.4})",
                                                    fresh_up, fresh_down, fresh_up + fresh_down
                                                ).yellow());
                                            }
                                        } else {
                                            println!("{}", format!(
                                                "Trade skipped: empty orderbook side (UP: {:.4}, DOWN: {:.4})",
                                                fresh_up, fresh_down
                                            ).yellow());
                                        }
                                    }

                                    // Mark this market as traded so we never re-trade it
                                    if traded {
                                        recent_opps_for_trade.lock().await.insert(opp_key);
                                    }
                                    // Keep is_executing true for a cooldown to let orderbook settle
                                    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                                    *is_exec_clone.lock().await = false;
                                });
                            }
                        }
                    }
                }

                let mut monitor_guard = monitor.lock().await;
                monitor_guard.add_to_history(&coin, price_data.clone(), &env);
                display_coin_details(&coin, &price_data, &market, &monitor_guard, &env);
            }
        });
    }).await;

    // Start WebSocket connection
    let ws_run_clone = ws_client.clone();
    tokio::spawn(async move {
        if let Err(e) = ws_run_clone.run(true).await {
            eprintln!("WebSocket error: {}", e);
        }
    });

    sleep(Duration::from_secs(2)).await;
    *ws = Some(ws_client);

    Ok(Some(market))
}

