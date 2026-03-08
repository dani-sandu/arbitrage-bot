use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Debug, Clone)]
pub struct OrderbookLevel {
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone)]
pub struct OrderbookSnapshot {
    pub asset_id: String,
    pub market: String,
    pub timestamp: i64,
    pub bids: Vec<OrderbookLevel>,
    pub asks: Vec<OrderbookLevel>,
    pub hash: Option<String>,
}

pub type BookCallback = Arc<dyn Fn(OrderbookSnapshot) + Send + Sync>;

pub struct MarketWebSocket {
    url: String,
    subscribed_assets: Mutex<Vec<String>>,
    orderbooks: RwLock<HashMap<String, OrderbookSnapshot>>,
    on_book_callback: Mutex<Option<BookCallback>>,
    is_running: Mutex<bool>,
}

impl MarketWebSocket {
    pub fn new(url: String) -> Self {
        Self {
            url,
            subscribed_assets: Mutex::new(Vec::new()),
            orderbooks: RwLock::new(HashMap::new()),
            on_book_callback: Mutex::new(None),
            is_running: Mutex::new(false),
        }
    }

    pub async fn set_on_book<F>(&self, callback: F)
    where
        F: Fn(OrderbookSnapshot) + Send + Sync + 'static,
    {
        *self.on_book_callback.lock().await = Some(Arc::new(callback));
    }

    pub async fn get_orderbook(&self, asset_id: &str) -> Option<OrderbookSnapshot> {
        self.orderbooks.read().await.get(asset_id).cloned()
    }

    fn parse_orderbook_snapshot(data: &serde_json::Value) -> Result<OrderbookSnapshot> {
        let mut bids: Vec<OrderbookLevel> = data
            .get("bids")
            .and_then(|v| v.as_array())
            .unwrap_or(&Vec::new())
            .iter()
            .filter_map(|b| {
                Some(OrderbookLevel {
                    price: b.get("price")?.as_str()?.parse().ok()?,
                    size: b.get("size")?.as_str()?.parse().ok()?,
                })
            })
            .collect();
        bids.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));

        let mut asks: Vec<OrderbookLevel> = data
            .get("asks")
            .and_then(|v| v.as_array())
            .unwrap_or(&Vec::new())
            .iter()
            .filter_map(|a| {
                Some(OrderbookLevel {
                    price: a.get("price")?.as_str()?.parse().ok()?,
                    size: a.get("size")?.as_str()?.parse().ok()?,
                })
            })
            .collect();
        asks.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));

        Ok(OrderbookSnapshot {
            asset_id: data.get("asset_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            market: data.get("market").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            timestamp: data.get("timestamp").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0),
            bids,
            asks,
            hash: data.get("hash").and_then(|v| v.as_str()).map(|s| s.to_string()),
        })
    }

    async fn handle_message(&self, message: &str) -> Result<()> {
        let data: serde_json::Value = serde_json::from_str(message)?;

        let messages = if let Some(arr) = data.as_array() {
            arr.clone()
        } else {
            vec![data]
        };

        for msg in messages {
            let event_type = msg
                .get("event_type")
                .or_else(|| msg.get("type"))
                .and_then(|v| v.as_str());

            if event_type == Some("book") {
                let snapshot = Self::parse_orderbook_snapshot(&msg)?;
                let asset_id = snapshot.asset_id.clone();

                {
                    self.orderbooks.write().await.insert(asset_id.clone(), snapshot.clone());
                }

                let callback_guard = self.on_book_callback.lock().await;
                if let Some(ref callback) = *callback_guard {
                    callback(snapshot);
                }
            }
        }

        Ok(())
    }

    pub async fn subscribe(&self, asset_ids: Vec<String>) -> Result<()> {
        if asset_ids.is_empty() {
            return Err(anyhow!("No asset IDs provided"));
        }
        *self.subscribed_assets.lock().await = asset_ids;
        Ok(())
    }

    pub async fn run(&self, auto_reconnect: bool) -> Result<()> {
        *self.is_running.lock().await = true;

        loop {
            if !*self.is_running.lock().await {
                break;
            }

            match self.connect().await {
                Ok((mut ws_stream, _)) => {
                    {
                        let subscribed = self.subscribed_assets.lock().await;
                        if !subscribed.is_empty() {
                            let subscribe_msg = json!({
                                "assets_ids": subscribed.clone(),
                                "type": "MARKET"
                            });
                            let _ = ws_stream.send(Message::Text(subscribe_msg.to_string())).await;
                        }
                    }

                    loop {
                        if !*self.is_running.lock().await {
                            break;
                        }
                        match ws_stream.next().await {
                            Some(Ok(Message::Text(text))) => {
                                if let Err(e) = self.handle_message(&text).await {
                                    eprintln!("Error handling message: {}", e);
                                }
                            }
                            Some(Ok(Message::Ping(data))) => {
                                let _ = ws_stream.send(Message::Pong(data)).await;
                            }
                            Some(Ok(Message::Close(_))) => break,
                            Some(Err(e)) => {
                                eprintln!("WebSocket error: {}", e);
                                break;
                            }
                            None => break,
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Connection error: {}", e);
                }
            }

            if !auto_reconnect || !*self.is_running.lock().await {
                break;
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }

        Ok(())
    }

    async fn connect(&self) -> Result<(tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, tokio_tungstenite::tungstenite::handshake::client::Response)> {
        let result = connect_async(&self.url).await?;
        Ok(result)
    }

    pub async fn stop(&self) {
        *self.is_running.lock().await = false;
    }
}
