use crate::config::{coin_slug, GAMMA_API_HOST};
use anyhow::{anyhow, Result};
use chrono::Timelike;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

lazy_static::lazy_static! {
    static ref HTTP_CLIENT: reqwest::Client = reqwest::Client::new();
}

// Market data structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoinMarket {
    pub coin: String, // Coin ticker (BTC, ETH, etc.)
    pub up_token_id: String, // UP token contract address
    pub down_token_id: String, // DOWN token contract address
    pub slug: String, // Market slug (e.g., "btc-updown-15m-1234567890")
    pub question: String, // Market question text
    pub end_date: String, // ISO 8601 end date
    pub accepting_orders: bool, // Whether market is still open
}

// Raw Gamma API response
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GammaMarket {
    slug: String,
    question: String,
    #[serde(alias = "endDate")]
    end_date: String,
    #[serde(alias = "acceptingOrders")]
    accepting_orders: bool,
    #[serde(alias = "clobTokenIds")]
    clob_token_ids: serde_json::Value, // Can be string or array
    outcomes: serde_json::Value, // Can be string or array
}

// Fetch market data from Gamma API (BTW: 10s timeout to avoid hanging)
async fn get_market_by_slug(slug: &str) -> Result<Option<GammaMarket>> {
    let url = format!("{}/markets/slug/{}", GAMMA_API_HOST, slug);
    let client = &*HTTP_CLIENT;
    
    match client.get(&url).timeout(std::time::Duration::from_secs(10)).send().await {
        Ok(response) => {
            if response.status().is_success() {
                Ok(response.json().await.ok())
            } else {
                Ok(None)
            }
        }
        Err(_) => Ok(None),
    }
}

// Parse JSON field that might be string or array (FYI: Polymarket API inconsistency)
fn parse_json_field<T: for<'de> Deserialize<'de>>(value: &serde_json::Value) -> Result<Vec<T>> {
    match value {
        serde_json::Value::String(s) => {
            // String format: parse as JSON string
            let parsed: Vec<T> = serde_json::from_str(s)?;
            Ok(parsed)
        }
        serde_json::Value::Array(arr) => {
            // Array format: parse directly
            let parsed: Vec<T> = serde_json::from_value(serde_json::Value::Array(arr.clone()))?;
            Ok(parsed)
        }
        _ => Err(anyhow!("Invalid JSON field format")),
    }
}

// Map outcomes to token IDs (AFAIK: creates lookup map like {"up": "0x123...", "down": "0x456..."})
fn parse_token_ids(market: &GammaMarket) -> Result<HashMap<String, String>> {
    let clob_token_ids: Vec<String> = parse_json_field(&market.clob_token_ids)?;
    let outcomes: Vec<String> = parse_json_field(&market.outcomes)?;
    
    let mut result = HashMap::new();
    for (i, outcome) in outcomes.iter().enumerate() {
        if i < clob_token_ids.len() {
            result.insert(outcome.to_lowercase(), clob_token_ids[i].clone()); // Lowercase for case-insensitive lookup
        }
    }
    Ok(result)
}

fn parse_market_data(coin: &str, market: GammaMarket) -> Result<CoinMarket> {
    let token_ids = parse_token_ids(&market)?;
    let up_token_id = token_ids
        .get("up")
        .or_else(|| token_ids.get("yes"))
        .ok_or_else(|| anyhow!("UP token ID not found"))?
        .clone();
    let down_token_id = token_ids
        .get("down")
        .or_else(|| token_ids.get("no"))
        .ok_or_else(|| anyhow!("DOWN token ID not found"))?
        .clone();

    Ok(CoinMarket {
        coin: coin.to_string(),
        up_token_id,
        down_token_id,
        slug: market.slug,
        question: market.question,
        end_date: market.end_date,
        accepting_orders: market.accepting_orders,
    })
}

// Find active 15-min market for coin. Fires all three window lookups in parallel
// to avoid the worst-case 30-second sequential wait (3 × 10s timeout).
pub async fn find_15_min_market(coin: &str) -> Result<Option<CoinMarket>> {
    let coin_upper = coin.to_uppercase();
    let prefix = coin_slug(&coin_upper)
        .ok_or_else(|| anyhow!("Unsupported coin: {}", coin_upper))?;

    // Calculate current 15-minute window timestamp (FYI: rounds down to nearest 15min)
    let now = chrono::Utc::now();
    let minute = (now.minute() / 15) * 15;
    let current_window = now
        .date_naive()
        .and_hms_opt(now.hour(), minute, 0)
        .unwrap();
    let current_ts = current_window.and_utc().timestamp();

    let current_slug = format!("{}-{}", prefix, current_ts);
    let next_slug    = format!("{}-{}", prefix, current_ts + 900);
    let prev_slug    = format!("{}-{}", prefix, current_ts - 900);

    // Fire all three requests simultaneously — total time = max(individual latencies)
    // instead of sum. Priority: current > next > previous.
    let (r_current, r_next, r_prev) = tokio::join!(
        get_market_by_slug(&current_slug),
        get_market_by_slug(&next_slug),
        get_market_by_slug(&prev_slug),
    );

    for result in [r_current, r_next, r_prev] {
        match result {
            Ok(Some(market)) if market.accepting_orders => {
                return Ok(Some(parse_market_data(&coin_upper, market)?));
            }
            Err(e) => {
                eprintln!("Market discovery error: {}", e);
            }
            _ => {}
        }
    }

    Ok(None)
}

