use std::sync::Arc;
use tracing::{info, warn};

use crate::config::MoonbagConfig;
use crate::types::*;
use crate::executor::TradeExecutor;
use crate::utils::alerts::AlertManager;

/// A moonbag position the bot is tracking. The bot never auto-sells these —
/// it just watches the price and sends alerts with sell buttons. The user
/// decides when (and how much) to sell.
pub struct MoonbagPosition {
    pub mint: String,
    pub symbol: String,
    /// Actual token balance, in human units. Sourced from the RPC at the
    /// time the position entered moonbag mode (NOT a SOL-value proxy, which
    /// is what the old code did and got wrong).
    pub token_balance: f64,
    /// Buy entry price, in SOL per token. Used to compute the multiplier
    /// shown in alerts ("at 1000x from buy").
    pub entry_price: f64,
    /// The price at which the last take-profit tier sold. The first moonbag
    /// alert fires when `current_price >= 2 * highest_sold_tier_price`.
    /// Stored as `entry_price * highest_sold_tier_multiplier` so the
    /// comparison `current_price >= last_alert_price * 2` works for both
    /// first and subsequent alerts.
    pub last_alert_price: f64,
    /// The multiplier (in x from buy entry) at which the most recent alert
    /// fired. Tracked for logging. None = no alert fired yet.
    pub last_alert_multiplier: Option<f64>,
}

pub struct MoonbagTracker {
    positions: Vec<MoonbagPosition>,
    config: MoonbagConfig,
    executor: Arc<TradeExecutor>,
    alerts: Arc<AlertManager>,
}

impl MoonbagTracker {
    pub fn new(config: MoonbagConfig, executor: Arc<TradeExecutor>, alerts: Arc<AlertManager>) -> Self {
        Self { positions: Vec::new(), config, executor, alerts }
    }

    /// Add a position to the moonbag tracker. Called from ExitManager when
    /// all take-profit tiers have sold, OR at startup for restored positions.
    ///
    /// `highest_sold_tier_multiplier` is the multiplier of the LAST take-profit
    /// tier that was sold (e.g., 100.0 if the 100x tier is what just sold).
    /// The first alert threshold is `2 * highest_sold_tier_multiplier * entry_price`.
    pub fn add(
        &mut self,
        mint: &str,
        symbol: &str,
        token_balance: f64,
        entry_price: f64,
        highest_sold_tier_multiplier: f64,
    ) {
        // Dedupe: if we already track this mint, don't add a second copy
        if self.positions.iter().any(|b| b.mint == mint) {
            info!("Moonbag already tracked for {} — skipping add", symbol);
            return;
        }

        let last_alert_price = entry_price * highest_sold_tier_multiplier;

        self.positions.push(MoonbagPosition {
            mint: mint.to_string(),
            symbol: symbol.to_string(),
            token_balance,
            entry_price,
            last_alert_price,
            last_alert_multiplier: None,
        });

        info!(
            "Moonbag tracking started: {} | balance={:.0} tokens | first alert at {:.1}x (entry: {:.8} SOL/token)",
            symbol, token_balance, highest_sold_tier_multiplier * 2.0, entry_price
        );
    }

    /// Remove a mint from tracking. Called after the user sells a moonbag.
    pub fn remove(&mut self, mint: &str) {
        let before = self.positions.len();
        self.positions.retain(|b| b.mint != mint);
        if self.positions.len() < before {
            info!("Moonbag removed from tracking: {}", mint);
        }
    }

    /// Update the stored token balance for a mint (called after a user sell
    /// reduces the balance).
    pub fn update_balance(&mut self, mint: &str, new_balance: f64) {
        if let Some(bag) = self.positions.iter_mut().find(|b| b.mint == mint) {
            bag.token_balance = new_balance;
        }
    }

    /// Snapshot of currently tracked mints — used by the callback listener to
    /// validate that a sell callback refers to a known position.
    pub fn tracked_mints(&self) -> Vec<String> {
        self.positions.iter().map(|b| b.mint.clone()).collect()
    }

    /// Read-only access to a moonbag's balance (for the callback listener's
    /// 50% / 100% sell math).
    pub fn balance_of(&self, mint: &str) -> Option<f64> {
        self.positions.iter().find(|b| b.mint == mint).map(|b| b.token_balance)
    }

    pub fn symbol_of(&self, mint: &str) -> Option<String> {
        self.positions.iter().find(|b| b.mint == mint).map(|b| b.symbol.clone())
    }

    /// Main monitor loop. Polls price every `check_interval_secs`. For each
    /// tracked position, checks if `current_price >= last_alert_price * 2`
    /// and sends an alert (with 50% / 100% sell buttons) if so.
    pub async fn monitor_loop(&mut self) {
        let interval = tokio::time::Duration::from_secs(self.config.check_interval_secs);
        info!(
            "Moonbag monitor loop started — check every {}s, tracking {} position(s)",
            self.config.check_interval_secs,
            self.positions.len()
        );

        loop {
            tokio::time::sleep(interval).await;

            // Collect mints first to avoid borrow conflict on positions
            let mints: Vec<String> = self.positions.iter().map(|b| b.mint.clone()).collect();
            let mut prices: Vec<(String, f64)> = Vec::new();
            for mint in &mints {
                let price = self.get_price(mint).await.unwrap_or(0.0);
                prices.push((mint.clone(), price));
            }

            for (mint, current_price) in prices {
                if current_price <= 0.0 {
                    continue;
                }

                // We need a mutable reference to positions — find by mint
                let bag = match self.positions.iter_mut().find(|b| b.mint == mint) {
                    Some(b) => b,
                    None => continue,
                };

                if bag.entry_price <= 0.0 {
                    // Legacy/placeholder entry price — set from first real fetch
                    bag.entry_price = current_price;
                    // Recompute the first-alert threshold too
                    bag.last_alert_price = current_price; // will be re-anchored below
                    warn!(
                        "{} | Moonbag entry_price was 0 — reset to {:.8} from first price fetch",
                        bag.symbol, current_price
                    );
                    continue;
                }

                // If first time and entry was just corrected, last_alert_price
                // was set to current_price above. Set it to entry * highest_tier
                // — but we don't know the highest tier now, so use entry * 100x
                // as a reasonable default (covers the typical tier1 config).
                if bag.last_alert_multiplier.is_none() && bag.last_alert_price < bag.entry_price * 2.0 {
                    // Probably just got a fresh entry — re-anchor
                    bag.last_alert_price = bag.entry_price * 100.0; // assume tier1's highest (100x)
                    info!(
                        "{} | First-alert threshold re-anchored to {:.1}x (entry={:.8})",
                        bag.symbol, 200.0, bag.entry_price
                    );
                }

                let multiplier = current_price / bag.entry_price;
                let threshold_price = bag.last_alert_price * 2.0;

                // Fire alert only when price has DOUBLED from the last alert
                if current_price < threshold_price {
                    continue;
                }

                info!(
                    "MOONBAG ALERT: {} at {:.1}x (last alert: {:.1}x) | threshold {:.8} → current {:.8}",
                    bag.symbol, multiplier,
                    bag.last_alert_multiplier.unwrap_or(0.0),
                    threshold_price, current_price
                );

                let pct_from_entry = (multiplier - 1.0) * 100.0;
                let estimated_sol = bag.token_balance * current_price;

                self.alerts.send_moonbag_alert(
                    &bag.symbol,
                    multiplier,
                    pct_from_entry,
                    bag.token_balance,
                    estimated_sol,
                    &bag.mint,
                ).await;

                bag.last_alert_price = current_price;
                bag.last_alert_multiplier = Some(multiplier);
            }
        }
    }

    /// Fetch the spot price for a mint from the pump.fun frontend API.
    /// Returns price in SOL per token (virtual_sol / virtual_token).
    async fn get_price(&self, mint: &str) -> anyhow::Result<f64> {
        let url = format!("https://frontend-api-v3.pump.fun/coins/{}", mint);
        let resp = self.executor.get_client()
            .get(&url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
            .send().await?
            .json::<serde_json::Value>().await?;

        let virtual_sol = resp["virtual_sol_reserves"].as_f64().unwrap_or(0.0);
        let virtual_token = resp["virtual_token_reserves"].as_f64().unwrap_or(1.0);

        if virtual_token <= 0.0 {
            return Err(anyhow::anyhow!("Invalid bonding curve state for {}", mint));
        }

        Ok(virtual_sol / virtual_token)
    }
}
