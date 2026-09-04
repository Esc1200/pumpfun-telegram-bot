use anyhow::Result;
use reqwest::Client;
use std::sync::Arc;
use tracing::info;

use crate::blockhash_cache::BlockhashCache;
use crate::config::AppConfig;
use crate::types::*;

pub mod pumpfun;
pub mod pumpswap;

pub struct TradeExecutor {
    config: AppConfig,
    private_key: Vec<u8>,
    client: Client,
    blockhash_cache: Arc<BlockhashCache>,
}

use crate::executor::pumpfun::{BuyError, SellError};

impl TradeExecutor {
    pub fn new(config: AppConfig, private_key: Vec<u8>, blockhash_cache: Arc<BlockhashCache>) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");
        Self { config, private_key, client, blockhash_cache }
    }

    /// Execute a buy — tries pump.fun bonding curve first.
    /// If the token has graduated, routes to PumpSwap AMM.
    pub async fn execute_buy(&self, token: &TokenData, amount_sol: f64) -> Result<String> {
        info!("Executing BUY: {} | {:.4} SOL", token.symbol, amount_sol);

        match pumpfun::buy_on_bonding_curve(
            &self.config, &self.private_key, token, amount_sol, &self.client, &self.blockhash_cache,
        ).await {
            Ok(tx) => return Ok(tx),
            Err(e) => {
                if e.downcast_ref::<BuyError>().map_or(false, |b| matches!(b, BuyError::Graduated { .. })) {
                    info!("Token {} graduated, routing to PumpSwap", token.symbol);
                } else if e.to_string().contains("3007") {
                    return Err(anyhow::anyhow!(
                        "Bonding curve error 3007 — token is broken, skipping"
                    ));
                } else {
                    // Simulation failure, RPC error, slippage, etc. — don't retry on PumpSwap
                    return Err(e);
                }
            }
        }

        // Graduated — try PumpSwap
        pumpswap::buy_on_pumpswap(
            &self.config, &self.private_key, token, amount_sol, &self.client,
        ).await
    }

    /// Execute a sell — tries pump.fun bonding curve first.
    /// If the token has graduated, routes to PumpSwap AMM.
    pub async fn execute_sell(
        &self,
        token: &TokenData,
        token_amount: f64,
        token_decimals: u8,
    ) -> Result<String> {
        info!("Executing SELL: {} | {:.4} tokens", token.symbol, token_amount);

        match pumpfun::sell_on_bonding_curve(
            &self.config, &self.private_key, token, token_amount, &self.client, &self.blockhash_cache,
        ).await {
            Ok(tx) => return Ok(tx),
            Err(e) => {
                if e.downcast_ref::<SellError>().map_or(false, |s| matches!(s, SellError::Graduated { .. })) {
                    info!("Token {} graduated, routing to PumpSwap for sell", token.symbol);
                } else {
                    return Err(e);
                }
            }
        }

        pumpswap::sell_on_pumpswap(
            &self.config, &self.private_key, token, token_amount, token_decimals, &self.client,
        ).await
    }

    pub fn get_client(&self) -> &Client {
        &self.client
    }

    /// Public accessor for the private key bytes (used by the exit manager
    /// to derive the wallet pubkey for token-balance queries). Returns None
    /// if the bot is running in detection-only mode (no private key loaded).
    pub fn private_key(&self) -> Option<&[u8]> {
        if self.private_key.is_empty() {
            None
        } else {
            Some(&self.private_key)
        }
    }

    /// Public accessor for the RPC URL (used by the exit manager to query
    /// token balances directly).
    pub fn rpc_url(&self) -> &str {
        &self.config.solana.rpc_url
    }
}
