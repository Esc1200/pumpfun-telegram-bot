use std::sync::Arc;
use std::str::FromStr;
use tokio::sync::Mutex as TokioMutex;
use tracing::{info, warn};

use crate::config::AppConfig;
use crate::types::*;
use crate::executor::TradeExecutor;
use crate::tracker::db::Database;
use crate::utils::alerts::AlertManager;
use solana_sdk::signer::Signer;

pub struct ExitManager {
    position: Position,
    strategy: ExitStrategy,
    config: AppConfig,
    executor: Arc<TradeExecutor>,
    alerts: Arc<AlertManager>,
    db: Database,
    moonbag_tracker: Arc<TokioMutex<crate::exit::moonbag::MoonbagTracker>>,
    current_price: f64,
    check_interval: std::time::Duration,
}

impl ExitManager {
    pub fn new(
        position: Position,
        strategy: ExitStrategy,
        config: AppConfig,
        executor: Arc<TradeExecutor>,
        alerts: Arc<AlertManager>,
        db: Arc<Database>,
        moonbag_tracker: Arc<TokioMutex<crate::exit::moonbag::MoonbagTracker>>,
    ) -> Self {
        let check_interval_ms = config.exit.stop_loss.check_interval_ms;
        Self {
            position,
            strategy,
            config,
            executor,
            alerts,
            db: (*db).clone(),
            moonbag_tracker,
            current_price: 0.0,
            check_interval: std::time::Duration::from_millis(check_interval_ms),
        }
    }

    pub async fn monitor(&mut self) {
        let mut check_count: u64 = 0;

        loop {
            tokio::time::sleep(self.check_interval).await;
            check_count += 1;

            // Fetch current price
            match self.fetch_price().await {
                Ok(price) => {
                    self.current_price = price;
                }
                Err(e) => {
                    if check_count % 10 == 0 {
                        warn!("{} | Price fetch failed: {}", self.position.symbol, e);
                    }
                    continue;
                }
            }

            // Set entry_price from first successful price fetch if not set
            if self.position.entry_price <= 0.0 {
                self.position.entry_price = self.current_price;
                info!(
                    "{} | Entry price set from first price fetch: {:.8}",
                    self.position.symbol, self.current_price
                );
                let _ = self.db.update_position(&self.position).await;
            }

            let multiplier = self.current_price / self.position.entry_price;
            let pnl_pct = (multiplier - 1.0) * 100.0;

            // Log every 5 checks
            if check_count % 5 == 0 {
                info!(
                    "{} | {:.2}x | PnL: {:.1}% | Remaining: {:.4} SOL",
                    self.position.symbol, multiplier, pnl_pct, self.position.remaining_sol
                );
            }

            // Check stop loss
            let trigger = self.config.exit.stop_loss.trigger_pct;
            if pnl_pct <= trigger * 100.0 {
                warn!(
                    "{} | STOP LOSS TRIGGERED at {:.2}x ({:.1}%)",
                    self.position.symbol, multiplier, pnl_pct
                );
                self.execute_stop_loss(self.config.exit.moonbag_pct / 100.0).await;
                return;
            }

            // Check take profit tiers
            self.check_take_profit_tiers(multiplier).await;

            // Check if all tiers sold — position becomes moonbag
            let all_sold = self.strategy.tiers.iter().all(|t| t.sold);
            if all_sold {
                info!("{} | All tiers sold — entering moonbag mode", self.position.symbol);
                self.position.is_moonbag = true;
                let _ = self.db.update_position(&self.position).await;

                // The highest tier that was sold is the LAST tier in the strategy
                // (we only reach this branch after every tier was sold). The
                // moonbag tracker's first alert fires at 2x of this multiplier.
                let highest_sold_tier_multiplier = self.strategy.tiers
                    .iter()
                    .map(|t| t.target_multiplier)
                    .fold(0.0_f64, f64::max);

                // Hand off to the shared moonbag tracker. We need to know the
                // actual token balance to display in alerts and to size the
                // user's manual sells. Read it from the wallet's ATA.
                let token_balance = self.fetch_token_balance().await;
                let symbol = self.position.symbol.clone();
                let mint = self.position.mint.clone();
                let entry_price = self.position.entry_price;

                {
                    let mut tracker = self.moonbag_tracker.lock().await;
                    tracker.add(&mint, &symbol, token_balance, entry_price, highest_sold_tier_multiplier);
                }

                // Cache is no longer needed — moonbag sells refetch curve state
                // (and from the frontend API, not the on-chain curve).
                crate::executor::pumpfun::curve_cache_remove(&mint);

                info!(
                    "{} | Moonbag handoff complete | first alert at {:.1}x from entry",
                    self.position.symbol, highest_sold_tier_multiplier * 2.0
                );

                break;
            }
        }

        // Save final position state
        let _ = self.db.update_position(&self.position).await;
    }

    async fn execute_stop_loss(&mut self, moonbag_pct: f64) {
        let multiplier = if self.position.entry_price > 0.0 {
            self.current_price / self.position.entry_price
        } else {
            0.0
        };
        let pnl_pct = (multiplier - 1.0) * 100.0;
        let sell_sol_value = self.position.remaining_sol - (self.position.remaining_sol * moonbag_pct);

        let sell_token_amount = if self.current_price > 0.0 {
            (sell_sol_value * 1_000_000_000.0 / self.current_price) / 1_000_000.0
        } else {
            0.0
        };

        if sell_token_amount <= 0.0 {
            warn!("{} | Stop loss sell amount too small", self.position.symbol);
            return;
        }

        let token_data = TokenData {
            mint: self.position.mint.clone(),
            symbol: self.position.symbol.clone(),
            name: String::new(),
            creator: String::new(),
            created_at: chrono::Utc::now(),
            metadata_uri: String::new(),
        };

        match self.executor.execute_sell(&token_data, sell_token_amount, 6).await {
            Ok(_tx) => {
                let moonbag_amount = self.position.remaining_sol * moonbag_pct;
                self.position.remaining_sol = moonbag_amount;
                self.position.stop_loss_triggered = true;
                self.position.is_moonbag = true;
                let _ = self.db.update_position(&self.position).await;

                // Log stop loss trade
                let trade = TradeLog {
                    id: 0,
                    mint: self.position.mint.clone(),
                    symbol: self.position.symbol.clone(),
                    side: "STOP_LOSS".to_string(),
                    sol_amount: sell_sol_value,
                    price: self.current_price,
                    mcap: 0.0,
                    multiplier,
                    strategy_label: self.strategy.label.clone(),
                    created_at: chrono::Utc::now(),
                };
                let _ = self.db.log_trade(&trade).await;

                info!(
                    "{} | STOP LOSS SOLD | moonbag: {:.4} SOL",
                    self.position.symbol, moonbag_amount
                );

                let _ = self.alerts.send_stop_loss_alert(
                    &self.position.symbol,
                    multiplier,
                    pnl_pct,
                    moonbag_amount,
                    self.config.exit.moonbag_pct,
                    &self.position.mint,
                ).await;
            }
            Err(e) => {
                warn!("{} | Stop loss sell failed: {}", self.position.symbol, e);
            }
        }
    }

    async fn check_take_profit_tiers(&mut self, multiplier: f64) {
        for idx in 0..self.strategy.tiers.len() {
            if self.strategy.tiers[idx].sold {
                continue;
            }

            let target_mult = self.strategy.tiers[idx].target_multiplier;
            if multiplier < target_mult {
                continue;
            }

            let sell_pct = self.strategy.tiers[idx].sell_pct;
            let sell_sol_value = self.position.original_sol * (sell_pct / 100.0);

            // Convert SOL value to human-readable token amount
            let sell_token_amount = if self.current_price > 0.0 {
                (sell_sol_value * 1_000_000_000.0 / self.current_price) / 1_000_000.0
            } else {
                0.0
            };

            if sell_token_amount <= 0.0 {
                warn!("Sell amount too small at {}x tier", target_mult);
                continue;
            }

            let token_data = TokenData {
                mint: self.position.mint.clone(),
                symbol: self.position.symbol.clone(),
                name: String::new(),
                creator: String::new(),
                created_at: chrono::Utc::now(),
                metadata_uri: String::new(),
            };

            match self.executor.execute_sell(&token_data, sell_token_amount, 6).await {
                Ok(_tx) => {
                    self.strategy.tiers[idx].sold = true;
                    self.position.remaining_sol -= sell_sol_value;

                    // Log sell trade
                    let trade = TradeLog {
                        id: 0,
                        mint: self.position.mint.clone(),
                        symbol: self.position.symbol.clone(),
                        side: "SELL".to_string(),
                        sol_amount: sell_sol_value,
                        price: self.current_price,
                        mcap: 0.0,
                        multiplier,
                        strategy_label: self.strategy.label.clone(),
                        created_at: chrono::Utc::now(),
                    };
                    let _ = self.db.log_trade(&trade).await;

                    let total_sold_pct: f64 = self.strategy.tiers.iter()
                        .filter(|t| t.sold)
                        .map(|t| t.sell_pct)
                        .sum();

                    info!(
                        "{} SOLD {:.0}% at {:.1}x | Total sold: {:.0}% | Remaining: {:.4} SOL",
                        self.position.symbol,
                        sell_pct,
                        target_mult,
                        total_sold_pct,
                        self.position.remaining_sol
                    );

                    let _ = self.alerts.send_sell_alert(
                        &self.position.symbol,
                        sell_pct,
                        multiplier,
                        &self.strategy.label,
                        &self.position.mint,
                    ).await;

                    let _ = self.db.update_position(&self.position).await;
                }
                Err(e) => {
                    warn!("{} | Sell failed at {}x: {}", self.position.symbol, target_mult, e);
                }
            }
        }
    }

    /// Fetch current token price from pump.fun bonding curve
    /// Returns price in SOL per token (lamports/raw_token)
    async fn fetch_price(&self) -> anyhow::Result<f64> {
        let url = format!(
            "https://frontend-api-v3.pump.fun/coins/{}",
            self.position.mint
        );

        // Try pump.fun API with retry (handles rate limiting)
        for attempt in 0..3 {
            let resp = self.executor.get_client()
                .get(&url)
                .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    if let Ok(json) = r.json::<serde_json::Value>().await {
                        let virtual_sol = json["virtual_sol_reserves"].as_f64().unwrap_or(0.0);
                        let virtual_token = json["virtual_token_reserves"].as_f64().unwrap_or(1.0);

                        if virtual_token > 0.0 {
                            return Ok(virtual_sol / virtual_token);
                        }
                    }
                }
                Ok(r) if r.status().as_u16() == 429 => {
                    // Rate limited — back off
                    let delay = std::time::Duration::from_millis(500 * (attempt + 1));
                    tokio::time::sleep(delay).await;
                    continue;
                }
                _ => {}
            }

            // Brief delay before retry
            if attempt < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }

        // Fallback: use entry price as rough estimate
        if self.position.entry_price > 0.0 {
            return Ok(self.position.entry_price);
        }

        Err(anyhow::anyhow!("Price fetch failed for {} after retries", self.position.mint))
    }

    /// Read the actual token balance for this position from the wallet's ATA.
    /// Returns the human-readable balance (UI amount) — what we'll show in
    /// moonbag alerts and what the user's 50% / 100% sell math is based on.
    ///
    /// Falls back to 0.0 if the RPC query fails (e.g., position already
    /// closed). The bot will then show "0 tokens" in alerts — not ideal but
    /// better than crashing.
    async fn fetch_token_balance(&self) -> f64 {
        // We need the wallet's pubkey to query its ATA. Pull it from the
        // private key (32 bytes) — this is a quick way to get it without
        // changing the constructor signature.
        let private_key = match self.executor.private_key() {
            Some(pk) => pk,
            None => return 0.0,
        };
        if private_key.len() != 64 {
            return 0.0;
        }
        let keypair = solana_sdk::signer::keypair::Keypair::from_bytes(private_key)
            .expect("valid keypair");
        let wallet = keypair.pubkey();

        // Derive the ATA for the wallet + mint manually (no extra dep).
        // ATA = find_program_address([wallet, token_prog, mint], ata_prog)
        let mint = match solana_sdk::pubkey::Pubkey::from_str(&self.position.mint) {
            Ok(m) => m,
            Err(_) => return 0.0,
        };

        let legacy_token_prog = solana_sdk::pubkey::Pubkey::from_str(
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        ).unwrap();
        let ata_prog = solana_sdk::pubkey::Pubkey::from_str(
            "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
        ).unwrap();

        let (ata, _) = solana_sdk::pubkey::Pubkey::find_program_address(
            &[wallet.as_ref(), legacy_token_prog.as_ref(), mint.as_ref()],
            &ata_prog,
        );

        // Query via getTokenAccountBalance
        let rpc_url = self.executor.rpc_url().to_string();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getTokenAccountBalance",
            "params": [ata.to_string()]
        });

        match self.executor.get_client()
            .post(&rpc_url)
            .json(&body)
            .send().await
        {
            Ok(r) if r.status().is_success() => {
                if let Ok(json) = r.json::<serde_json::Value>().await {
                    return json["value"]["uiAmount"].as_f64().unwrap_or(0.0);
                }
            }
            _ => {}
        }
        0.0
    }
}
