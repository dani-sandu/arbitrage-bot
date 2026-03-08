use crate::utils::logger::get_data_dir;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Persistent state that survives bot restarts.
/// Tracks which markets have been traded and the last trade result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotPersistentState {
    /// Market slugs already traded (won't re-trade)
    pub traded_slugs: Vec<String>,
    /// Cumulative PnL from all sessions
    pub cumulative_pnl: f64,
    /// Total trades executed
    pub total_trades: u64,
    /// Last known USDC balance
    pub last_usdc_balance: f64,
    /// Recent trade records for post-mortem analysis
    #[serde(default)]
    pub recent_trades: Vec<TradeRecord>,
}

/// A single trade record for audit and reconciliation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    pub timestamp: String,
    pub market_slug: String,
    pub both_filled: bool,
    pub up_order_id: Option<String>,
    pub down_order_id: Option<String>,
    pub estimated_pnl: f64,
    pub unwind_attempted: bool,
}

impl Default for BotPersistentState {
    fn default() -> Self {
        Self {
            traded_slugs: Vec::new(),
            cumulative_pnl: 0.0,
            total_trades: 0,
            last_usdc_balance: 0.0,
            recent_trades: Vec::new(),
        }
    }
}

impl BotPersistentState {
    fn state_path() -> PathBuf {
        PathBuf::from(get_data_dir()).join("state.json")
    }

    pub fn load() -> Self {
        let path = Self::state_path();
        if path.exists() {
            if let Ok(data) = std::fs::read_to_string(&path) {
                if let Ok(state) = serde_json::from_str(&data) {
                    println!("[STATE] Loaded persistent state ({} previous trades)", {
                        let s: &BotPersistentState = &state;
                        s.total_trades
                    });
                    return state;
                }
            }
        }
        Self::default()
    }

    pub fn save(&self) {
        let path = Self::state_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            if let Err(e) = std::fs::write(&path, json) {
                eprintln!("[STATE] Failed to save state.json: {}", e);
            }
        }
    }

    pub fn record_trade(&mut self, slug: &str, pnl: f64) {
        if !self.traded_slugs.contains(&slug.to_string()) {
            self.traded_slugs.push(slug.to_string());
        }
        // Keep only last 200 slugs to bound memory
        if self.traded_slugs.len() > 200 {
            self.traded_slugs.drain(0..100);
        }
        self.cumulative_pnl += pnl;
        self.total_trades += 1;
        self.save();
    }

    /// Record a richer trade entry for reconciliation.
    pub fn record_trade_detail(&mut self, record: TradeRecord) {
        self.recent_trades.push(record);
        // Keep only last 50 detailed records
        if self.recent_trades.len() > 50 {
            self.recent_trades.drain(0..25);
        }
        self.save();
    }

    pub fn was_traded(&self, slug: &str) -> bool {
        self.traded_slugs.contains(&slug.to_string())
    }
}
