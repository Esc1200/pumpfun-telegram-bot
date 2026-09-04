use anyhow::Result;
use futures::{SinkExt, StreamExt};
use reqwest::Client;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Mutex;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn, error, debug};

use crate::config::AppConfig;
use crate::types::TokenData;

pub struct WebSocketMonitor {
    client: Client,
    rpc_url: String,
    ws_url: String,
    program_id: String,
    seen: Mutex<HashSet<String>>,
}

impl WebSocketMonitor {
    pub async fn new(config: &AppConfig) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()?,
            rpc_url: config.solana.rpc_url.clone(),
            ws_url: config.solana.ws_url.clone(),
            program_id: config.pumpfun.program_id.clone(),
            seen: Mutex::new(HashSet::new()),
        })
    }

    pub async fn listen(
        &self,
        token_tx: mpsc::Sender<TokenData>,
        _parser: crate::monitor::parser::TransactionParser,
    ) -> Result<()> {
        info!("Starting hybrid monitor: WS logsSubscribe + 1s API poll");

        // Run WS and poller concurrently
        let token_tx_ws = token_tx.clone();
        let client = self.client.clone();
        let rpc_url = self.rpc_url.clone();
        let ws_url = self.ws_url.clone();
        let program_id = self.program_id.clone();

        let ws_handle = tokio::spawn(Self::run_ws_listener(
            token_tx_ws, client, rpc_url, ws_url, program_id,
        ));

        // Poller runs in current task
        self.run_poller(&token_tx).await;

        ws_handle.abort();
        Ok(())
    }

    /// Real-time WebSocket: subscribe to logs mentioning pump.fun program
    async fn run_ws_listener(
        token_tx: mpsc::Sender<TokenData>,
        client: Client,
        rpc_url: String,
        ws_url: String,
        program_id: String,
    ) {
        let mut backoff = 2u64;

        loop {
            match Self::ws_session(&token_tx, &client, &rpc_url, &ws_url, &program_id).await {
                Ok(()) => {
                    info!("WS session ended, reconnecting...");
                    backoff = 2;
                }
                Err(e) => {
                    warn!("WS error: {}. Reconnecting in {}s...", e, backoff);
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(30);
                }
            }
        }
    }

    async fn ws_session(
        token_tx: &mpsc::Sender<TokenData>,
        client: &Client,
        rpc_url: &str,
        ws_url: &str,
        program_id: &str,
    ) -> Result<()> {
        info!("Connecting WS to {}...", ws_url);
        let (mut ws_stream, _) = connect_async(ws_url).await?;
        info!("WS connected! Subscribing to pump.fun logs...");

        // Subscribe to logs mentioning the pump.fun program
        let subscribe = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "logsSubscribe",
            "params": [
                { "mentions": [program_id] },
                { "commitment": "confirmed" }
            ]
        });

        ws_stream.send(Message::Text(subscribe.to_string())).await?;

        // Wait for subscription confirmation
        if let Some(msg) = ws_stream.next().await {
            let text = msg?.into_text()?;
            let val: serde_json::Value = serde_json::from_str(&text)?;
            if val.get("result").is_some() {
                info!("logsSubscribe confirmed (sub_id: {})", val["result"]);
            } else {
                // The subscribe failed (e.g., Alchemy doesn't support
                // logsSubscribe, or method returned an error). Return Err
                // so the outer loop reconnects — otherwise the WS task
                // would sit idle forever consuming a Tokio task slot.
                return Err(anyhow::anyhow!("Subscribe failed: {}", text));
            }
        }

        let mut seen: HashSet<String> = HashSet::new();

        // Process incoming log notifications
        // Bug 9: Periodically clear the seen set to prevent unbounded growth
        while let Some(msg) = ws_stream.next().await {
            let text = match msg {
                Ok(Message::Text(t)) => t,
                Ok(Message::Ping(d)) => {
                    let _ = ws_stream.send(Message::Pong(d)).await;
                    continue;
                }
                Ok(Message::Close(_)) => {
                    warn!("WS closed by server");
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
                _ => continue,
            };

            let val: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Extract notification
            let params = match val.get("params") {
                Some(p) => p,
                None => continue,
            };

            let result = match params.get("result") {
                Some(r) => r,
                None => continue,
            };

            let sig = match result.get("value")
                .and_then(|v| v.get("signature"))
                .and_then(|v| v.as_str())
            {
                Some(s) => s.to_string(),
                None => continue,
            };

            // Skip failed transactions
            let err = result.get("value").and_then(|v| v.get("err"));
            if err.is_some() && !err.unwrap().is_null() {
                continue;
            }

            // Check logs for token creation patterns
            let logs = match result.get("value")
                .and_then(|v| v.get("logs"))
                .and_then(|l| l.as_array())
            {
                Some(l) => l,
                None => continue,
            };

            // Look for pump.fun token creation signatures in logs
            // FIX 14: Require pump.fun program invocation to avoid false positives
            // from generic SPL token instructions
            let pump_invoke = format!("Program {} invoke", program_id);
            let pump_invoked = logs.iter().any(|l| {
                l.as_str().map(|s| s.contains(&pump_invoke)).unwrap_or(false)
            });
            let is_token_creation = pump_invoked && logs.iter().any(|l| {
                l.as_str().map(|s|
                    s.contains("Instruction: Create")
                    || s.contains("initialize_mint")
                    || s.contains("InitializeMint")
                ).unwrap_or(false)
            });

            if !is_token_creation {
                continue;
            }

            // Dedup by signature
            if !seen.insert(sig.clone()) {
                continue;
            }
            // Bug 9: Evict old entries if seen set grows too large (dedup cache only)
            if seen.len() > 10_000 {
                seen.clear();
                debug!("Cleared WS seen set (exceeded 10,000 entries)");
            }

            debug!("Token creation detected in logs: {}", &sig[..12.min(sig.len())]);

            // Fetch full transaction to get mint address
            match Self::fetch_mint_from_tx(client, rpc_url, &sig).await {
                Ok(Some((mint, creator))) => {
                    // Dedup by mint
                    {
                        // Use seen set - we share nothing with the poller's seen set
                        // but within WS session we track our own
                        if seen.contains(&mint) {
                            continue;
                        }
                        seen.insert(mint.clone());
                    }

                    // Enrich with pump.fun API
                    match Self::fetch_pumpfun_data(client, &mint).await {
                        Ok(token) => {
                            info!(
                                "WS HIT: {} ({}) by {} | {} | sig {}",
                                token.symbol, token.name,
                                &token.creator[..8.min(token.creator.len())],
                                &mint[..8.min(mint.len())],
                                &sig[..12.min(sig.len())]
                            );
                            let _ = token_tx.send(token).await;
                        }
                        Err(e) => {
                            debug!("Pump.fun enrichment failed for {}: {}", &mint[..8.min(mint.len())], e);
                            let token = TokenData {
                                mint: mint.clone(),
                                symbol: "???".to_string(),
                                name: "Unknown".to_string(),
                                creator,
                                created_at: chrono::Utc::now(),
                                metadata_uri: String::new(),
                            };
                            let _ = token_tx.send(token).await;
                        }
                    }
                }
                Ok(None) => {
                    debug!("No mint found in tx {}", &sig[..12.min(sig.len())]);
                }
                Err(e) => {
                    debug!("Failed to fetch tx {}: {}", &sig[..12.min(sig.len())], e);
                }
            }
        }

        Ok(())
    }

    /// Fetch transaction and extract the mint address from account keys
    async fn fetch_mint_from_tx(
        client: &Client,
        rpc_url: &str,
        sig: &str,
    ) -> Result<Option<(String, String)>> {
        let resp = client.post(rpc_url)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getTransaction",
                "params": [sig, { "encoding": "json", "commitment": "confirmed" }]
            }))
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        let tx = match resp.get("result") {
            Some(t) if !t.is_null() => t,
            _ => return Ok(None),
        };

        let account_keys = match tx.get("transaction")
            .and_then(|t| t.get("message"))
            .and_then(|m| m.get("accountKeys"))
            .and_then(|k| k.as_array())
        {
            Some(keys) => keys,
            None => return Ok(None),
        };

        // Fee payer (creator) is the first account key
        let creator = account_keys.first()
            .and_then(|k| k.as_str().or_else(|| k.get("pubkey").and_then(|p| p.as_str())))
            .unwrap_or("unknown")
            .to_string();

        // Known program/system accounts to skip when looking for the mint
        let known_programs: HashSet<&str> = [
            "11111111111111111111111111111111",
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
            "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
            "SysvarRent111111111111111111111111111111111",
            "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",
            "ComputeBudget111111111111111111111111111111",
        ].iter().copied().collect();

        // Find the first account after fee payer that isn't a known program
        let mint = account_keys.iter()
            .skip(1) // Skip fee payer (creator)
            .find_map(|k| {
                let key = k.as_str().or_else(|| k.get("pubkey").and_then(|p| p.as_str()))?;
                if known_programs.contains(key) {
                    None
                } else {
                    Some(key.to_string())
                }
            });

        match mint {
            Some(m) => Ok(Some((m, creator))),
            None => Ok(None),
        }
    }

    /// Fetch token data from pump.fun API
    async fn fetch_pumpfun_data(client: &Client, mint: &str) -> Result<TokenData> {
        let url = format!("https://frontend-api-v3.pump.fun/coins/{}", mint);
        let resp = client.get(&url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        Ok(TokenData {
            mint: mint.to_string(),
            symbol: resp.get("symbol").and_then(|v| v.as_str()).unwrap_or("???").to_string(),
            name: resp.get("name").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string(),
            creator: resp.get("creator").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
            created_at: chrono::Utc::now(),
            metadata_uri: resp.get("metadata_uri").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        })
    }

    /// Fallback: 1-second polling from pump.fun API
    async fn run_poller(&self, token_tx: &mpsc::Sender<TokenData>) {
        info!("Starting 1s API poller (fallback)");

        loop {
            if let Err(e) = self.poll_and_process(token_tx).await {
                warn!("Poll error: {}", e);
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
        }
    }

    async fn poll_and_process(
        &self,
        token_tx: &mpsc::Sender<TokenData>,
    ) -> Result<()> {
        let resp = self.client
            .get("https://frontend-api-v3.pump.fun/coins")
            .query(&[
                ("limit", "20"),
                ("offset", "0"),
                ("sort", "created_timestamp"),
                ("order", "DESC"),
            ])
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        let coins = match resp.as_array() {
            Some(c) => c,
            None => {
                debug!("No coins array in response");
                return Ok(());
            }
        };

        for coin in coins {
            let mint = match coin.get("mint").and_then(|v| v.as_str()) {
                Some(m) => m.to_string(),
                None => continue,
            };

            // Skip if we've already seen this mint
            {
                let mut seen = self.seen.lock().unwrap();
                // Bug 9: Evict old entries if seen set grows too large
                if seen.len() > 10_000 {
                    seen.clear();
                    debug!("Cleared poller seen set (exceeded 10,000 entries)");
                }
                if !seen.insert(mint.clone()) {
                    continue;
                }
            }

            let name = coin.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string();

            let symbol = coin.get("symbol")
                .and_then(|v| v.as_str())
                .unwrap_or("???")
                .to_string();

            let creator = coin.get("creator")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let is_initialized = coin.get("initialized")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if !is_initialized {
                debug!("Skipping uninitialized token: {}", symbol);
                continue;
            }

            if name.is_empty() || symbol.is_empty() {
                continue;
            }

            info!(
                "POLL: {} ({}) by {} | {}",
                symbol, name,
                &creator[..8.min(creator.len())],
                &mint[..8.min(mint.len())]
            );

            let token = TokenData {
                mint,
                symbol,
                name,
                creator,
                created_at: chrono::Utc::now(),
                metadata_uri: coin.get("metadata_uri")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            };

            if let Err(e) = token_tx.send(token).await {
                error!("Failed to send token: {}", e);
            }
        }

        Ok(())
    }
}
