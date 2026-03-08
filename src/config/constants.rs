pub const AVAILABLE_COINS: &[&str] = &["BTC", "ETH", "SOL", "XRP"];

pub fn coin_slug(coin: &str) -> Option<&str> {
    match coin.to_uppercase().as_str() {
        "BTC" => Some("btc-updown-15m"),
        "ETH" => Some("eth-updown-15m"),
        "SOL" => Some("sol-updown-15m"),
        "XRP" => Some("xrp-updown-15m"),
        _ => None,
    }
}

pub const GAMMA_API_HOST: &str = "https://gamma-api.polymarket.com";
pub const MIN_ORDER_SIZE_USD: f64 = 1.0;

