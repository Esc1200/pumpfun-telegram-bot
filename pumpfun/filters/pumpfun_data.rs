use anyhow::Result;
use reqwest::Client;
use tracing::{info, warn, debug};

use crate::config::AppConfig;
use crate::types::TokenData;

/// Pump.fun coin data fetched from their public API (free, no rate limits)
#[derive(Debug, Clone)]
pub struct PumpfunCoinData {
    pub mint: String,
    pub usd_market_cap: f64,
    pub real_sol_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub real_token_reserves: u64,
    pub total_supply: u64,
    pub complete: bool,
    pub creator: String,
    pub associated_bonding_curve: String,
    pub created_timestamp: i64,
}

/// Fetch coin data from pump.fun API
pub async fn fetch_coin_data(client: &Client, mint: &str) -> Result<PumpfunCoinData> {
    let url = format!("https://frontend-api-v3.pump.fun/coins/{}", mint);
    let resp = client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    Ok(PumpfunCoinData {
        mint: mint.to_string(),
        usd_market_cap: resp.get("usd_market_cap").and_then(|v| v.as_f64()).unwrap_or(0.0),
        real_sol_reserves: resp.get("real_sol_reserves").and_then(|v| v.as_u64()).unwrap_or(0),
        virtual_sol_reserves: resp.get("virtual_sol_reserves").and_then(|v| v.as_u64()).unwrap_or(30_000_000_000),
        virtual_token_reserves: resp.get("virtual_token_reserves").and_then(|v| v.as_u64()).unwrap_or(1_073_000_000_000_000),
        real_token_reserves: resp.get("real_token_reserves").and_then(|v| v.as_u64()).unwrap_or(793_000_000_000_000),
        total_supply: resp.get("total_supply").and_then(|v| v.as_u64()).unwrap_or(1_000_000_000_000_000),
        complete: resp.get("complete").and_then(|v| v.as_bool()).unwrap_or(false),
        creator: resp.get("creator").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
        associated_bonding_curve: resp.get("associated_bonding_curve")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        created_timestamp: resp.get("created_timestamp")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
    })
}
