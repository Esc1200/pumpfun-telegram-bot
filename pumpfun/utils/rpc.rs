use anyhow::{Result, anyhow};
use reqwest::Client;
use serde_json::json;

use crate::config::AppConfig;
use crate::types::HolderInfo;

/// Async Solana RPC wrapper using reqwest for HTTP JSON-RPC calls.
/// Falls back to standard Solana RPC methods, enhanced by Jupiter price API.
#[derive(Clone)]
pub struct SolanaRpc {
    client: Client,
    rpc_url: String,
}

impl SolanaRpc {
    pub fn new(config: &AppConfig) -> Self {
        Self {
            client: Client::new(),
            rpc_url: config.solana.rpc_url.clone(),
        }
    }

    pub fn get_client(&self) -> &Client {
        &self.client
    }

    /// Fetch the spot price for a mint from pump.fun's frontend API. Used
    /// by the moonbag callback listener to compute the multiplier for the
    /// sell confirmation message. Returns price in SOL per token, or 0.0
    /// if the request fails.
    pub async fn fetch_spot_price_static(mint: &str) -> f64 {
        let url = format!("https://frontend-api-v3.pump.fun/coins/{}", mint);
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(_) => return 0.0,
        };
        let resp = match client
            .get(&url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => return 0.0,
        };
        let json: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => return 0.0,
        };
        let virtual_sol = json["virtual_sol_reserves"].as_f64().unwrap_or(0.0);
        let virtual_token = json["virtual_token_reserves"].as_f64().unwrap_or(1.0);
        if virtual_token <= 0.0 { return 0.0; }
        virtual_sol / virtual_token
    }

    /// Make a raw JSON-RPC call to the Solana cluster
    async fn rpc_call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params
        });

        let resp = self.client
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("RPC HTTP error: {}", status));
        }

        let json: serde_json::Value = resp.json().await?;

        if let Some(error) = json.get("error") {
            return Err(anyhow!("RPC error: {}", error));
        }

        json.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("RPC response missing 'result' field"))
    }

    /// Get SOL balance for a wallet address (in SOL, not lamports)
    pub async fn get_sol_balance(&self, wallet: &str) -> Result<f64> {
        let result = self.rpc_call("getBalance", json!([wallet])).await?;
        let lamports = result["value"]
            .as_u64()
            .ok_or_else(|| anyhow!("Invalid balance response"))?;
        Ok(lamports as f64 / 1_000_000_000.0)
    }

    /// Get token supply for a mint (UI amount, already decimal-adjusted)
    pub async fn get_token_supply(&self, mint: &str) -> Result<f64> {
        let result = self.rpc_call("getTokenSupply", json!([mint])).await?;
        let amount_str = result["value"]["uiAmountString"]
            .as_str()
            .or_else(|| result["value"]["uiAmount"].as_f64().map(|_| ""))
            .ok_or_else(|| anyhow!("Invalid token supply response"))?;

        // Try uiAmountString first (more precise), then uiAmount
        if !amount_str.is_empty() {
            return amount_str.parse::<f64>().map_err(|e| anyhow!("Parse error: {}", e));
        }

        result["value"]["uiAmount"]
            .as_f64()
            .ok_or_else(|| anyhow!("Cannot parse token supply"))
    }

    /// Get the top token holders via getTokenLargestAccounts
    /// Returns the token account addresses (ATAs), not the owner wallets.
    /// To get actual owners, we need to fetch account info for each.
    pub async fn get_token_holders(&self, mint: &str) -> Result<Vec<HolderInfo>> {
        let result = self.rpc_call(
            "getTokenLargestAccounts",
            json!([mint, {"commitment": "confirmed"}])
        ).await?;

        let accounts = result["value"]
            .as_array()
            .ok_or_else(|| anyhow!("Invalid token largest accounts response"))?;

        let mut holders = Vec::new();
        for account in accounts {
            let address = account["address"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let amount = account["uiAmount"]
                .as_f64()
                .or_else(|| {
                    account["uiAmountString"]
                        .as_str()
                        .and_then(|s| s.parse::<f64>().ok())
                })
                .unwrap_or(0.0);

            // For holder concentration check, we use the token account address
            // (not the owner wallet). The bonding curve's ATA will match
            // associated_bonding_curve from the API.
            holders.push(HolderInfo { owner: address, amount });
        }

        Ok(holders)
    }

    /// Get market cap in USD: token supply * token price in SOL * SOL price in USD
    pub async fn get_market_cap(&self, mint: &str) -> Result<f64> {
        let (supply, token_price_sol, sol_price_usd) = tokio::try_join!(
            self.get_token_supply(mint),
            self.get_token_price_sol(mint),
            Self::get_sol_price_usd_static(&self.client),
        )?;

        let mcap = supply * token_price_sol * sol_price_usd;
        Ok(mcap)
    }

    /// Get token price in SOL from pump.fun bonding curve
    pub async fn get_token_price_sol(&self, mint: &str) -> Result<f64> {
        let url = format!("https://frontend-api-v3.pump.fun/coins/{}", mint);
        let resp = self.client.get(&url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
            .send().await?
            .json::<serde_json::Value>().await?;

        let virtual_sol = resp["virtual_sol_reserves"].as_f64().unwrap_or(0.0);
        let virtual_token = resp["virtual_token_reserves"].as_f64().unwrap_or(1.0);

        if virtual_token <= 0.0 {
            return Err(anyhow!("Invalid bonding curve state for {}", mint));
        }

        Ok(virtual_sol / virtual_token)
    }

    /// Trace who funded a wallet by looking at its earliest transactions
    /// Returns (funder_wallet, tx_count, wallet_age_secs)
    pub async fn trace_wallet_funder(&self, wallet: &str) -> Result<(String, u64, u64)> {
        // Get recent signatures (limit 10 to find the earliest)
        let sigs = self.rpc_call(
            "getSignaturesForAddress",
            json!([wallet, {"limit": 10}])
        ).await?;

        let signatures = sigs.as_array()
            .ok_or_else(|| anyhow!("Invalid signatures response"))?;

        if signatures.is_empty() {
            return Err(anyhow!("No transactions found for wallet {}", wallet));
        }

        let tx_count = signatures.len() as u64;

        // The last entry is the earliest transaction
        let earliest = signatures.last()
            .ok_or_else(|| anyhow!("No signatures"))?;

        let sig = earliest["signature"].as_str()
            .ok_or_else(|| anyhow!("No signature string"))?;

        // Calculate wallet age from earliest transaction
        let earliest_time = earliest["blockTime"].as_u64().unwrap_or(0);
        let now = chrono::Utc::now().timestamp() as u64;
        let age_secs = if earliest_time > 0 { now - earliest_time } else { 0 };

        // Fetch the earliest transaction to find the sender
        let tx = self.rpc_call(
            "getTransaction",
            json!([sig, {"encoding": "jsonParsed", "commitment": "confirmed", "maxSupportedTransactionVersion": 0}])
        ).await?;

        // Extract the funder from the transaction
        // In a SOL transfer, the first account key (fee payer / signer) is the sender
        let funder = self.extract_funder_from_tx(&tx, wallet)?;

        Ok((funder, tx_count, age_secs))
    }

    /// Extract the funder (sender) from a transaction
    fn extract_funder_from_tx(&self, tx: &serde_json::Value, target_wallet: &str) -> Result<String> {
        // Try jsonParsed format first
        if let Some(message) = tx.get("transaction").and_then(|t| t.get("message")) {
            // Check parsed instructions for SystemProgram transfer
            if let Some(instructions) = message.get("instructions").and_then(|i| i.as_array()) {
                for ix in instructions {
                    if let Some(parsed) = ix.get("parsed") {
                        let program = ix.get("program").and_then(|p| p.as_str()).unwrap_or("");
                        let ix_type = parsed.get("type").and_then(|t| t.as_str()).unwrap_or("");

                        if program == "system" && ix_type == "transfer" {
                            let info = parsed.get("info").ok_or_else(|| anyhow!("No transfer info"))?;
                            let source = info.get("source").and_then(|s| s.as_str()).unwrap_or("");
                            let dest = info.get("destination").and_then(|d| d.as_str()).unwrap_or("");

                            // The funder is the source of the transfer TO our target wallet
                            if dest == target_wallet {
                                return Ok(source.to_string());
                            }
                        }
                    }
                }
            }

            // Fallback: the first account key (fee payer) is likely the funder
            if let Some(account_keys) = message.get("accountKeys").and_then(|k| k.as_array()) {
                if let Some(first) = account_keys.first() {
                    let key = first.get("pubkey").and_then(|p| p.as_str())
                        .or_else(|| first.as_str());
                    if let Some(k) = key {
                        if k != target_wallet {
                            return Ok(k.to_string());
                        }
                    }
                }
            }
        }

        Err(anyhow!("Could not extract funder from transaction"))
    }

    /// Get wallet transaction count (uses getSignaturesForAddress with limit)
    pub async fn get_wallet_tx_count_rpc(&self, wallet: &str) -> Result<u64> {
        let sigs = self.rpc_call(
            "getSignaturesForAddress",
            json!([wallet, {"limit": 1000}])
        ).await?;

        let count = sigs.as_array()
            .map(|a| a.len() as u64)
            .unwrap_or(0);

        Ok(count)
    }

    /// Get current SOL price in USD from Jupiter Price API
    /// Uses So11111111111111111111111111111111111111112 (native SOL mint)
    pub async fn get_sol_price_usd(&self) -> Result<f64> {
        Self::get_sol_price_usd_static(&self.client).await
    }

    /// Static version for use in concurrent joins
    async fn get_sol_price_usd_static(client: &Client) -> Result<f64> {
        // Use CoinGecko directly (Jupiter DNS may not resolve on some VPS)
        Self::get_sol_price_coingecko(client).await
    }

    /// Fallback: get SOL price from CoinGecko
    async fn get_sol_price_coingecko(client: &Client) -> Result<f64> {
        let url = "https://api.coingecko.com/api/v3/simple/price?ids=solana&vs_currencies=usd";
        let resp = client.get(url).send().await?;

        if !resp.status().is_success() {
            return Err(anyhow!("CoinGecko HTTP error: {}", resp.status()));
        }

        let json: serde_json::Value = resp.json().await?;
        json["solana"]["usd"]
            .as_f64()
            .ok_or_else(|| anyhow!("No SOL price in CoinGecko response"))
    }

    /// Helper for parsing SOL price from Jupiter response as fallback
    fn get_sol_price_coingecko_sync(_json: &serde_json::Value) -> Result<f64> {
        Err(anyhow!("SOL price not available from Jupiter"))
    }
}
