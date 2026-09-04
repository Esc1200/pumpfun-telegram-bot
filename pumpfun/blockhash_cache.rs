//! Shared blockhash cache.
//!
//! Solana blockhashes are valid for ~60-90 slots (currently ~90s on mainnet).
//! A buy that fetches its own blockhash pays ~250ms per buy. This cache
//! refreshes one blockhash in the background every 10s, so buy-time blockhash
//! lookups are effectively free (just a mutex read).
//!
//! A stale blockhash is one that Solana will reject at submission time, so
//! we use a conservative 30s TTL — well inside the ~90s slot validity window.

use std::str::FromStr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use reqwest::Client;
use solana_sdk::hash::Hash;
use tracing::{info, warn};

const FRESH_TTL: Duration = Duration::from_secs(30);
const REFRESH_INTERVAL: Duration = Duration::from_secs(10);

struct CacheEntry {
    hash: Hash,
    fetched_at: Instant,
}

pub struct BlockhashCache {
    inner: StdMutex<Option<CacheEntry>>,
    client: Client,
    rpc_url: String,
}

impl BlockhashCache {
    pub fn new(client: Client, rpc_url: String) -> Arc<Self> {
        Arc::new(Self {
            inner: StdMutex::new(None),
            client,
            rpc_url,
        })
    }

    /// Returns a fresh blockhash from the cache, refreshing if stale or empty.
    /// First call after a long idle period may pay one RPC roundtrip.
    pub async fn get(&self) -> Result<Hash> {
        // Fast path: cached and fresh
        {
            let guard = self.inner.lock().unwrap();
            if let Some(entry) = guard.as_ref() {
                if entry.fetched_at.elapsed() < FRESH_TTL {
                    return Ok(entry.hash);
                }
            }
        }
        // Slow path: refresh
        self.refresh().await
    }

    async fn refresh(&self) -> Result<Hash> {
        let hash = fetch_blockhash(&self.client, &self.rpc_url).await?;
        let mut guard = self.inner.lock().unwrap();
        *guard = Some(CacheEntry {
            hash,
            fetched_at: Instant::now(),
        });
        Ok(hash)
    }
}

/// Spawn a background task that refreshes the blockhash every REFRESH_INTERVAL.
/// One task per bot — call once at startup.
pub fn spawn_refresh_loop(cache: Arc<BlockhashCache>) {
    tokio::spawn(async move {
        // Warm the cache immediately so first buy is fast
        if let Err(e) = cache.refresh().await {
            warn!("Blockhash cache initial warm failed: {}", e);
            return; // No point looping if the RPC is down
        }
        info!("Blockhash cache warmed");
        let mut interval = tokio::time::interval(REFRESH_INTERVAL);
        loop {
            interval.tick().await;
            if let Err(e) = cache.refresh().await {
                warn!("Blockhash cache refresh failed: {}", e);
                // Don't return — RPC may have been transiently down
            }
        }
    });
}

async fn fetch_blockhash(client: &Client, rpc_url: &str) -> Result<Hash> {
    let resp = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "getLatestBlockhash",
            "params": [{"commitment": "finalized"}]
        }))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    let blockhash_str = resp
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.get("blockhash"))
        .and_then(|b| b.as_str())
        .ok_or_else(|| anyhow!("Failed to get blockhash: {}", resp))?;

    Ok(Hash::from_str(blockhash_str)?)
}
