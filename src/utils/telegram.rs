use std::env;

lazy_static::lazy_static! {
    static ref HTTP_CLIENT: reqwest::Client = reqwest::Client::new();
}

/// Send a Telegram notification. Fire-and-forget — won't slow down the bot.
/// Silently skips if TELEGRAM_BOT_TOKEN or TELEGRAM_CHAT_ID are not set.
pub async fn send_telegram_alert(message: &str) {
    let token = env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
    let chat_id = env::var("TELEGRAM_CHAT_ID").unwrap_or_default();

    if token.is_empty() || chat_id.is_empty() {
        return;
    }

    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);

    let _ = HTTP_CLIENT
        .post(&url)
        .form(&[("chat_id", &chat_id), ("text", &message.to_string())])
        .send()
        .await;
}
