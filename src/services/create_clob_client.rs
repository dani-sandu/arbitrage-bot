use crate::config::Env;
use anyhow::{anyhow, Result};
use colored::*;
use polymarket_client_sdk::clob::{Client as SdkClobClient, Config as ClobConfig};
use polymarket_client_sdk::clob::types::{Side, SignatureType};
use polymarket_client_sdk::types::Decimal;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{Normal, Signer};
use polymarket_client_sdk::POLYGON;
use alloy_signer_local::LocalSigner;
use k256::ecdsa::SigningKey;
use std::str::FromStr;

pub struct ClobClient {
    client: SdkClobClient<Authenticated<Normal>>,
    signer: LocalSigner<SigningKey>,
}

impl ClobClient {
    /// Build and submit a limit order (FAK = Fill-And-Kill, partial fills allowed, rest cancelled)
    pub async fn submit_order(
        &self,
        token_id: &str,
        side: OrderSide,
        price: f64,
        size: f64,
    ) -> Result<OrderResponse> {
        let price_str = format!("{:.2}", price);
        let size_str = format!("{:.2}", size);
        let price_dec = Decimal::from_str(&price_str)
            .map_err(|e| anyhow!("Invalid price decimal: {}", e))?;
        let size_dec = Decimal::from_str(&size_str)
            .map_err(|e| anyhow!("Invalid size decimal: {}", e))?;

        if price_dec <= Decimal::ZERO || size_dec <= Decimal::ZERO {
            return Err(anyhow!("Price and size must be positive"));
        }

        let sdk_side = match side {
            OrderSide::Buy => Side::Buy,
            OrderSide::Sell => Side::Sell,
        };

        let order = self.client.limit_order()
            .token_id(token_id)
            .price(price_dec)
            .size(size_dec)
            .side(sdk_side)
            .build()
            .await
            .map_err(|e| anyhow!("Order build failed: {}", e))?;

        let signed_order = self.client.sign(&self.signer, order)
            .await
            .map_err(|e| anyhow!("Order signing failed: {}", e))?;

        let resp = self.client.post_order(signed_order)
            .await
            .map_err(|e| anyhow!("Order submission failed: {}", e))?;

        Ok(OrderResponse {
            success: true,
            order_id: Some(resp.order_id.to_string()),
            error: None,
        })
    }
}

pub async fn create_clob_client(env: &Env) -> Result<ClobClient> {
    let private_key = env
        .private_key
        .as_ref()
        .ok_or_else(|| anyhow!("PRIVATE_KEY is required for trading"))?;

    let trimmed_key = private_key.trim();
    let signer = LocalSigner::from_str(trimmed_key)
        .map_err(|e| anyhow!("Invalid PRIVATE_KEY: {}. Must be a valid 64-char hex string.", e))?
        .with_chain_id(Some(POLYGON));

    // Determine wallet type from SIGNATURE_TYPE env var (EOA or GNOSIS_SAFE)
    let sig_type = env.signature_type.to_uppercase();
    let is_proxy_safe = match sig_type.as_str() {
        "EOA" | "" => false,
        "GNOSIS_SAFE" | "POLY_GNOSIS_SAFE" => true,
        other => return Err(anyhow!("Invalid SIGNATURE_TYPE '{}'. Use 'EOA' or 'GNOSIS_SAFE'.", other)),
    };

    println!(
        "{}",
        format!(
            "Wallet type detected: {}",
            if is_proxy_safe { "Gnosis Safe" } else { "EOA" }
        )
        .cyan()
    );

    let client_builder = SdkClobClient::new(
        &env.clob_http_url,
        ClobConfig::default(),
    )
    .map_err(|e| anyhow!("Failed to create CLOB client: {}", e))?;

    let mut auth_builder = client_builder.authentication_builder(&signer);

    if is_proxy_safe {
        if let Some(ref proxy_wallet) = env.proxy_wallet {
            let proxy_addr = polymarket_client_sdk::types::Address::from_str(proxy_wallet)
                .map_err(|e| anyhow!("Invalid PROXY_WALLET address: {}", e))?;
            auth_builder = auth_builder
                .funder(proxy_addr)
                .signature_type(SignatureType::GnosisSafe);
        }
    }

    let authenticated_client = auth_builder
        .authenticate()
        .await
        .map_err(|e| anyhow!("CLOB authentication failed: {}", e))?;

    println!("{}", "✓ CLOB client authenticated".green());

    Ok(ClobClient {
        client: authenticated_client,
        signer,
    })
}

#[derive(Debug, Clone)]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone)]
pub struct OrderResponse {
    pub success: bool,
    pub order_id: Option<String>,
    pub error: Option<String>,
}


