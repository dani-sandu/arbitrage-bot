use dotenv::dotenv;
use std::env;

// Config struct for env vars (FYI: all optional fields can be None if not set)
#[derive(Debug, Clone)]
pub struct Env {
    pub clob_http_url: String,
    pub clob_ws_url: String,
    pub private_key: Option<String>,
    pub proxy_wallet: Option<String>,
    pub rpc_url: String,
    pub token_amount: f64,
    pub arbitrage_threshold: f64,
    pub taker_fee_rate: f64,
    pub market_asset: String,
    pub display_ui: bool,
    pub data_dir: String,
    pub signature_type: String,
    pub max_spread: f64,
    pub velocity_enabled: bool,
    pub velocity_threshold: f64,
    pub velocity_lockout_secs: i64,
    pub max_unwind_slippage: f64,
    pub buy_price_buffer: f64,
    pub min_book_depth: usize,
    pub sequential_execution: bool,
    pub socks5_proxy_url: Option<String>,
    /// REDEEM_ENABLED: when true this instance runs the background redeemer sweep.
    pub redeem_enabled: bool,
    /// DRY_RUN: when true the redeemer logs what it would do but sends no transactions.
    pub dry_run: bool,
    /// REDEEM_INTERVAL_SECS: how often the background redeemer sweeps for resolved positions.
    pub redeem_interval_secs: u64,
}

impl Env {
    // Load env vars from .env file (AFAIK: falls back to defaults if missing)
    pub fn load() -> Self {
        dotenv().ok(); // Load .env, ignore errors if file doesn't exist

        Self {
            clob_http_url: env::var("CLOB_HTTP_URL")
                .unwrap_or_else(|_| "https://clob.polymarket.com".to_string()),
            clob_ws_url: env::var("CLOB_WS_URL")
                .unwrap_or_else(|_| "wss://ws-subscriptions-clob.polymarket.com/ws/market".to_string()),
            private_key: env::var("PRIVATE_KEY").ok(),
            proxy_wallet: env::var("PROXY_WALLET").ok(),
            rpc_url: env::var("RPC_URL")
                .unwrap_or_else(|_| "https://polygon-rpc.com".to_string()),
            token_amount: env::var("TOKEN_AMOUNT")
                .unwrap_or_else(|_| "5.0".to_string())
                .parse()
                .unwrap_or(5.0),
            arbitrage_threshold: env::var("ARBITRAGE_THRESHOLD")
                .unwrap_or_else(|_| "1.0".to_string())
                .parse()
                .unwrap_or(1.0),
            taker_fee_rate: env::var("TAKER_FEE_RATE")
                .unwrap_or_else(|_| "0.02".to_string())
                .parse()
                .unwrap_or(0.02),
            market_asset: env::var("MARKET_ASSET")
                .unwrap_or_else(|_| "BTC".to_string()),
            display_ui: env::var("DISPLAY_UI")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),
            data_dir: env::var("DATA_DIR")
                .unwrap_or_else(|_| "./data".to_string()),
            signature_type: env::var("SIGNATURE_TYPE")
                .unwrap_or_else(|_| "EOA".to_string()),
            max_spread: env::var("MAX_SPREAD")
                .unwrap_or_else(|_| "0.10".to_string())
                .parse()
                .unwrap_or(0.10),
            velocity_enabled: env::var("VELOCITY_ENABLED")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),
            velocity_threshold: env::var("VELOCITY_THRESHOLD")
                .unwrap_or_else(|_| "0.15".to_string())
                .parse()
                .unwrap_or(0.15),
            velocity_lockout_secs: env::var("VELOCITY_LOCKOUT_SECS")
                .unwrap_or_else(|_| "5".to_string())
                .parse()
                .unwrap_or(5),
            max_unwind_slippage: env::var("MAX_UNWIND_SLIPPAGE")
                .unwrap_or_else(|_| "0.50".to_string())
                .parse()
                .unwrap_or(0.50),
            buy_price_buffer: env::var("BUY_PRICE_BUFFER")
                .unwrap_or_else(|_| "0.01".to_string())
                .parse()
                .unwrap_or(0.01),
            min_book_depth: env::var("MIN_BOOK_DEPTH")
                .unwrap_or_else(|_| "2".to_string())
                .parse()
                .unwrap_or(2),
            sequential_execution: env::var("SEQUENTIAL_EXECUTION")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),
            socks5_proxy_url: env::var("SOCKS5_PROXY_URL").ok().filter(|s| !s.is_empty()),
            redeem_enabled: env::var("REDEEM_ENABLED")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),
            dry_run: env::var("DRY_RUN")
                .unwrap_or_else(|_| "true".to_string())
                .parse()
                .unwrap_or(true),
            redeem_interval_secs: env::var("REDEEM_INTERVAL_SECS")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .unwrap_or(300),
        }
    }
}

