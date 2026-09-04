use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex as TokioMutex;

use anyhow::Result;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use chrono::TimeZone;

use pumpfun_sniper_bot::blockhash_cache::{spawn_refresh_loop, BlockhashCache};
use pumpfun_sniper_bot::config::AppConfig;
use pumpfun_sniper_bot::daily_activity::{DailyActivityEntry, DailyActivityLog};
use pumpfun_sniper_bot::executor::TradeExecutor;
use pumpfun_sniper_bot::exit::build_exit_strategy;
use pumpfun_sniper_bot::exit::stop_loss::ExitManager;
use pumpfun_sniper_bot::exit::moonbag::MoonbagTracker;
use pumpfun_sniper_bot::filters::first_timer::{check_first_timer_buy, FirstTimerResult};
// use pumpfun_sniper_bot::filters::FilterEngine;  // DISABLED -- whitelist-only mode
use pumpfun_sniper_bot::monitor::start_monitoring;
use pumpfun_sniper_bot::tracker::db::Database;
use pumpfun_sniper_bot::tracker::pnl::PnlTracker;
use pumpfun_sniper_bot::types::*;
use pumpfun_sniper_bot::utils::alerts::AlertManager;
use pumpfun_sniper_bot::utils::rpc::SolanaRpc;
use pumpfun_sniper_bot::moonbag_callbacks::poll_moonbag_callbacks;

/// Mass-launcher threshold: a whitelisted creator that launches more than this
/// many tokens in 24h is removed from the whitelist permanently.
const MAX_LAUNCHES_24H: u64 = 2;

/// Per-creator async lock map. The outer StdMutex is held briefly to
/// get/create the inner tokio Mutex. The inner Mutex serializes all
/// operations for a single creator (record_launch, mass-launcher check,
/// recent-buy check, buy execution) so concurrent detections of tokens
/// from the same creator can't race past the 24h cap or cull threshold.
/// Different creators proceed in parallel.
type CreatorLocks = Arc<StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Load environment variables
    dotenv::dotenv().ok();

    info!("=== PumpFun Sniper Bot Starting ===");

    // Parse CLI args
    let args: Vec<String> = std::env::args().collect();

    // Check for --test-buy mode (one-shot real buy, no WebSocket loop)
    if args.len() > 1 && args[1] == "--test-buy" {
        return run_test_buy(args).await;
    }
    // Check for --test-sell mode (one-shot real sell, no WebSocket loop)
    if args.len() > 1 && args[1] == "--test-sell" {
        return run_test_sell(args).await;
    }
    // Check for --test-buy-and-sell mode (round-trip in one process — proves curve cache)
    if args.len() > 1 && args[1] == "--test-buy-and-sell" {
        return run_test_buy_and_sell(args).await;
    }

    // Load config
    let config = AppConfig::load("config.toml")?;
    info!("Config loaded successfully");

    // Initialize database
    let db = Database::new("data/sniper.db")?;
    info!("Database initialized");

    // Import whitelisted creators from daily Dune scraper
    let whitelist_path = "data/dev_whitelist.json";
    match db.import_whitelist_from_json(whitelist_path).await {
        Ok(count) => info!("Loaded {} whitelisted creators from {}", count, whitelist_path),
        Err(e) => warn!("Could not load whitelist from {}: {}", whitelist_path, e),
    }

    // Initialize RPC
    let rpc = SolanaRpc::new(&config);

    // Initialize blockhash cache (saves ~250ms per buy by avoiding RPC roundtrip)
    let blockhash_cache = BlockhashCache::new(rpc.get_client().clone(), config.solana.rpc_url.clone());
    spawn_refresh_loop(blockhash_cache.clone());
    info!("Blockhash cache: pre-warming");

    // Initialize trade executor (optional — bot can detect without trading)
    let private_key: Vec<u8> = match std::env::var("SOLANA_PRIVATE_KEY") {
        Ok(pk) => {
            let decoded = bs58::decode(&pk).into_vec()
                .expect("Failed to decode SOLANA_PRIVATE_KEY (must be base58)");
            info!("Private key loaded — trading ENABLED");
            decoded
        }
        Err(_) => {
            warn!("SOLANA_PRIVATE_KEY not set — running in DETECTION-ONLY mode (no trades)");
            Vec::new()
        }
    };

    let executor = Arc::new(TradeExecutor::new(config.clone(), private_key, blockhash_cache));

    // Initialize alerts
    let alerts = Arc::new(AlertManager::new(config.alerts.clone()));

    // Initialize daily activity log (whitelisted creator launch tracking)
    let daily_log = Arc::new(DailyActivityLog::new("data/daily_activity.json"));
    info!("Daily activity log: data/daily_activity.json");

    // Initialize per-creator lock map for race-safe whitelist path
    let creator_locks: CreatorLocks = Arc::new(StdMutex::new(HashMap::new()));

    // Initialize PnL tracker
    let pnl_tracker = PnlTracker::new(config.tracker.clone(), db.clone());

    // MoonbagTracker is shared between ExitManager (which adds new positions
    // when all tiers sell) and the main moonbag monitor task (which polls
    // prices and sends alerts). It is also shared with the callback listener
    // (which looks up balances when the user taps Sell 50% / Sell 100%).
    let moonbag_tracker = Arc::new(TokioMutex::new(
        MoonbagTracker::new(
            config.exit.moonbag.clone(),
            executor.clone(),
            alerts.clone(),
        )
    ));

    // Restore moonbag positions from DB (in case the bot restarted while
    // positions were already in moonbag mode).
    {
        let moonbag_positions = db.get_moonbag_positions().await;
        let mut tracker = moonbag_tracker.lock().await;
        for pos in &moonbag_positions {
            // For restored positions we don't have a "highest sold tier"
            // snapshot, so use 100x as a reasonable default (matches
            // the current tier1 config). The first alert will fire at
            // 200x, which is the conservative choice.
            // The actual token balance is unknown at restart — set to 0
            // and let the next price check update from the alert message.
            tracker.add(
                &pos.mint,
                &pos.symbol,
                0.0,                              // unknown at restart
                pos.entry_price,
                100.0,                            // assume tier1's 100x
            );
        }
        if !moonbag_positions.is_empty() {
            info!("Restored {} moonbag position(s) — first alerts at 200x", moonbag_positions.len());
        }
    }

    // Create channel for new tokens from WebSocket
    let (token_tx, mut token_rx) = tokio::sync::mpsc::channel::<TokenData>(64);

    // Spawn WebSocket monitor
    let ws_config = config.clone();
    tokio::spawn(async move {
        if let Err(e) = start_monitoring(&ws_config, token_tx).await {
            tracing::error!("WebSocket monitor error: {}", e);
        }
    });

    // Spawn moonbag monitor — shares the Arc<Mutex<MoonbagTracker>> with
    // the exit manager and the callback listener.
    let monitor_tracker = moonbag_tracker.clone();
    tokio::spawn(async move {
        let mut tracker = monitor_tracker.lock().await;
        tracker.monitor_loop().await;
    });

    // Spawn the Telegram callback listener. It polls getUpdates for
    // callback_query updates matching "moonbag_sell:<mint>:<pct>" and
    // executes user-triggered sells. The listener needs:
    //   - The AlertManager (to call answerCallbackQuery + edit messages)
    //   - The TradeExecutor (to actually execute the sell)
    //   - The MoonbagTracker (to look up balance / remove after sell)
    //   - A Solana RPC client (for token balance after sell)
    if !alerts.bot_token().is_empty() {
        let cb_tracker = moonbag_tracker.clone();
        let cb_alerts = alerts.clone();
        let cb_executor = executor.clone();
        let cb_rpc = rpc.clone();
        tokio::spawn(async move {
            poll_moonbag_callbacks(cb_alerts, cb_executor, cb_tracker, cb_rpc).await;
        });
        info!("Moonbag callback listener started");
    } else {
        warn!("TELEGRAM_BOT_TOKEN not set — moonbag callback listener disabled");
    }

    // Spawn periodic whitelist reload (twice daily).
    // Reload slots: 03:10 and 15:10 server local time (= 09:10 and 21:10 WAT).
    // The 09:10 run picks up the output of the daily Dune scraper which runs at 09:00 WAT.
    let db_reload = db.clone();
    tokio::spawn(async move {
        whitelist_reload_loop(db_reload, "data/dev_whitelist.json").await;
    });

    info!("Listening for new pump.fun tokens...");

    // Main loop — process incoming tokens
    while let Some(token) = token_rx.recv().await {
        // Check daily loss limit
        if !pnl_tracker.check_daily_loss().await {
            warn!("Daily loss limit reached — skipping token {}", token.symbol);
            continue;
        }

        // Check if we already have a position
        if db.has_active_position(&token.mint).await {
            info!("Already have position for {} — skipping", token.symbol);
            continue;
        }

        // Spawn token processing task
        let config = config.clone();
        let db = db.clone();
        let executor = executor.clone();
        let alerts = alerts.clone();
        let rpc = rpc.clone();
        let daily_log = daily_log.clone();
        let creator_locks = creator_locks.clone();
        let moonbag_tracker = moonbag_tracker.clone();

        tokio::spawn(async move {
            if let Err(e) = process_token(token, config, db, executor, alerts, rpc, daily_log, creator_locks, moonbag_tracker).await {
                tracing::error!("Error processing token: {}", e);
            }
        });
    }

    Ok(())
}

/// Process a newly detected token: whitelist check → insta-buy or funding trace.
async fn process_token(
    token: TokenData,
    config: AppConfig,
    db: Database,
    executor: Arc<TradeExecutor>,
    alerts: Arc<AlertManager>,
    rpc: SolanaRpc,
    daily_log: Arc<DailyActivityLog>,
    creator_locks: CreatorLocks,
    moonbag_tracker: Arc<TokioMutex<MoonbagTracker>>,
) -> Result<()> {
    // Acquire per-creator lock FIRST to serialize all operations for this
    // creator (record_launch, mass-launcher check, recent-buy check, buy
    // execution). Without this, two tokens from the same creator detected
    // in the same second could both pass the cull and the 24h cap checks.
    let creator_lock = {
        let mut locks = creator_locks.lock().unwrap();
        locks
            .entry(token.creator.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _guard = creator_lock.lock().await;

    info!("Processing token: {} ({})", token.symbol, token.mint);

    // Record this launch for spam detection
    let _ = db.record_creator_launch(&token.creator, &token.mint).await;

    // ═══════════════════════════════════════════════════════
    // PATH 1: Creator is whitelisted → 24h buy-cap + mass-launcher cull
    // ═══════════════════════════════════════════════════════
    if let Some(score) = db.get_whitelist_score(&token.creator).await {
        // [FIRST] Mass launcher check: >MAX_LAUNCHES_24H in 24h → remove permanently
        let launch_count = db.creator_launch_count_24h(&token.creator).await;
        if launch_count > MAX_LAUNCHES_24H {
            let short = &token.creator[..token.creator.len().min(8)];
            warn!(
                "MASS LAUNCHER CULL: {} launched {} tokens in 24h (>{} threshold) — REMOVING from whitelist permanently",
                short, launch_count, MAX_LAUNCHES_24H
            );
            let _ = db.remove_from_whitelist(&token.creator).await;
            // Log to daily_activity so decay check 24h later sees it
            let _ = daily_log.append(&DailyActivityEntry {
                creator: token.creator.clone(),
                mint: token.mint.clone(),
                symbol: token.symbol.clone(),
                detected_at: chrono::Utc::now(),
                launch_block_time: token.created_at,
                action: "removed_mass_launcher".to_string(),
            });
            // Fall through — no buy, but no further whitelist check
        } else {
            // [SECOND] Already bought from this creator in last 24h?
            if db.recent_buy_from_creator_24h(&token.creator).await {
                let short = &token.creator[..token.creator.len().min(8)];
                info!(
                    "ALREADY BOUGHT: {} — skipping 2nd buy in 24h (creator stays on whitelist)",
                    short
                );
                // Log to daily_activity
                let _ = daily_log.append(&DailyActivityEntry {
                    creator: token.creator.clone(),
                    mint: token.mint.clone(),
                    symbol: token.symbol.clone(),
                    detected_at: chrono::Utc::now(),
                    launch_block_time: token.created_at,
                    action: "skipped_already_bought".to_string(),
                });
                return Ok(());
            }

            // First of the day — BUY
            let amount_sol = score_to_amount(score, &config);
            let short = &token.creator[..token.creator.len().min(8)];

            info!(
                "WHITELIST HIT (first of day): {} | creator {} | score {} | {:.4} SOL → INSTA-BUY",
                token.symbol, short, score, amount_sol
            );

            // Record the buy BEFORE the actual trade so a crash mid-buy still
            // counts as "already bought" and prevents a re-buy on retry.
            let _ = db.record_buy_from_creator(&token.creator).await;

            // Log to daily_activity
            let _ = daily_log.append(&DailyActivityEntry {
                creator: token.creator.clone(),
                mint: token.mint.clone(),
                symbol: token.symbol.clone(),
                detected_at: chrono::Utc::now(),
                launch_block_time: token.created_at,
                action: "bought".to_string(),
            });

            execute_buy_and_manage(
                &token, amount_sol, score, &config, &db, &executor, &alerts, None, &moonbag_tracker,
            ).await;
            return Ok(());
        }
    }

    // ═══════════════════════════════════════════════════════
    // PATH 1.5: FIRST-TIMER BUY — catches brand-new CEX-funded creators
    // Conditions: 0 prior launches + wallet age < 48h + CEX-funded
    //             (SOL balance tracked, not gating)
    // ═══════════════════════════════════════════════════════
    match check_first_timer_buy(&token, &config, &db, &rpc).await {
        FirstTimerResult::Buy { amount_sol, score, wallet_age_secs, sol_balance } => {
            info!(
                "FIRST-TIMER INSTA-BUY: {} | creator {} | age={}s bal={:.2}SOL | score={} | {:.4} SOL",
                token.symbol,
                &token.creator[..token.creator.len().min(8)],
                wallet_age_secs,
                sol_balance,
                score,
                amount_sol
            );

            let ft_ctx = FirstTimerBuyContext { wallet_age_secs, sol_balance };
            execute_buy_and_manage(
                &token, amount_sol, score, &config, &db, &executor, &alerts, Some(ft_ctx), &moonbag_tracker,
            ).await;

            // Add creator to whitelist so their next launches auto-buy
            // (spam filter at >5 launches/24h still applies)
            let _ = db.whitelist_wallet(&token.creator, score).await;
            info!(
                "Added first-timer creator {} to whitelist (score={})",
                &token.creator[..token.creator.len().min(8)],
                score
            );
            return Ok(());
        }
        FirstTimerResult::Skip { reason } => {
            // Not a first-timer OR conditions not met — fall through to existing paths
            // (silently skip Path 1.5 if creator already has history, which is most tokens)
            if reason != "creator has prior launches" {
                info!("First-timer path: {} -> {}", &token.creator[..token.creator.len().min(8)], reason);
            }
        }
    }

    // ═══════════════════════════════════════════════════════
    // PATH 2: Creator NOT whitelisted → trace funding
    // Check if the funding wallet is one of our whitelisted addresses.
    // If yes, buy with funder's score and add new creator to DB.
    // ═══════════════════════════════════════════════════════
    info!(
        "Creator {} NOT in whitelist — tracing funding source...",
        &token.creator[..token.creator.len().min(8)]
    );

    // Check funding cache or trace from blockchain
    let funder_wallet = match db.get_wallet_funder(&token.creator).await {
        Some(funder) => {
            info!("Funding cache hit: funder={}", &funder[..funder.len().min(8)]);
            Some(funder)
        }
        None => {
            // Trace from blockchain (1 RPC call)
            info!("Tracing funding from blockchain...");
            match rpc.trace_wallet_funder(&token.creator).await {
                Ok((funder, tx_count, age_secs)) => {
                    info!(
                        "Blockchain trace: funder={} tx_count={} age={}s",
                        &funder[..funder.len().min(8)], tx_count, age_secs
                    );
                    let _ = db.save_wallet_funder(&token.creator, &funder, tx_count, age_secs).await;
                    Some(funder)
                }
                Err(e) => {
                    warn!("Failed to trace funder for {}: {}", &token.creator[..token.creator.len().min(8)], e);
                    None
                }
            }
        }
    };

    // Check if the funder is whitelisted
    if let Some(ref funder) = funder_wallet {
        if let Some(funder_score) = db.get_whitelist_score(funder).await {
            let amount_sol = score_to_amount(funder_score, &config);

            info!(
                "FUNDING HIT: {} | funder {} (score {}) funded creator {} → {:.4} SOL BUY",
                token.symbol,
                &funder[..funder.len().min(8)],
                funder_score,
                &token.creator[..token.creator.len().min(8)],
                amount_sol
            );

            // Buy first, then add new creator to whitelist
            execute_buy_and_manage(
                &token, amount_sol, funder_score, &config, &db, &executor, &alerts, None, &moonbag_tracker,
            ).await;

            // Add this new creator to whitelist with inherited score
            info!(
                "Adding new creator {} to whitelist with inherited score {}",
                &token.creator[..token.creator.len().min(8)], funder_score
            );
            let _ = db.whitelist_wallet(&token.creator, funder_score).await;
            return Ok(());
        }
    }

    // Neither creator nor funder is whitelisted — skip
    info!(
        "SKIP: {} | creator {} not whitelisted, funder not whitelisted either",
        token.symbol,
        &token.creator[..token.creator.len().min(8)]
    );
    Ok(())
}

/// Periodically reload the whitelist from dev_whitelist.json.
/// Schedule: 03:10 and 15:10 server local time daily (= 09:10 and 21:10 WAT).
/// The 09:10 run picks up the output of the daily Dune scraper (runs at 09:00 WAT).
/// Preserves first-timer additions (source='firsttimer' rows are not touched).
async fn whitelist_reload_loop(db: Database, path: &'static str) {
    info!("Whitelist reload loop active (schedule: 03:10 & 15:10 server local / 09:10 & 21:10 WAT)");

    loop {
        let now = chrono::Local::now();
        let now_ts = now.timestamp();
        let today = now.date_naive();

        // Candidate reload times today (server local time)
        let morning = today
            .and_hms_opt(3, 10, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .unwrap()
            .timestamp();
        let evening = today
            .and_hms_opt(15, 10, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .unwrap()
            .timestamp();

        // Pick the next slot: morning if still ahead, else evening if still ahead, else tomorrow's morning.
        let next_ts = if now_ts < morning {
            morning
        } else if now_ts < evening {
            evening
        } else {
            (today + chrono::Duration::days(1))
                .and_hms_opt(3, 10, 0)
                .unwrap()
                .and_local_timezone(chrono::Local)
                .unwrap()
                .timestamp()
        };

        let sleep_secs = (next_ts - now_ts).max(60) as u64;
        let next_str = chrono::Local
            .timestamp_opt(next_ts, 0)
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        info!("Next whitelist reload: {} (in {}s)", next_str, sleep_secs);

        tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;

        info!("Reloading whitelist from {}", path);
        match db.reload_whitelist_from_json(path).await {
            Ok(n) => info!("Whitelist reloaded: {} entries from JSON", n),
            Err(e) => warn!("Whitelist reload failed: {}", e),
        }
    }
}

/// Map whitelist score to buy amount. Higher score = bigger bet.
fn score_to_amount(score: u8, config: &AppConfig) -> f64 {
    if score >= 75 {
        config.trading.whitelisted_amount_sol  // 0.02 SOL
    } else if score >= 50 {
        config.trading.whitelisted_amount_sol * 0.75  // 0.015 SOL
    } else {
        config.trading.whitelisted_amount_sol * 0.5  // 0.01 SOL
    }
}

/// Context for a first-timer buy — passed to `execute_buy_and_manage` so it
/// knows to send the specialized "first-timer" alert with the wallet-age/balance
/// fields. `None` for whitelist or funder-funded buys.
#[derive(Debug, Clone)]
pub struct FirstTimerBuyContext {
    pub wallet_age_secs: u64,
    pub sol_balance: f64,
}

/// Shared buy + position + alert + exit manager logic.
async fn execute_buy_and_manage(
    token: &TokenData,
    amount_sol: f64,
    score: u8,
    config: &AppConfig,
    db: &Database,
    executor: &Arc<TradeExecutor>,
    alerts: &Arc<AlertManager>,
    first_timer: Option<FirstTimerBuyContext>,
    moonbag_tracker: &Arc<TokioMutex<MoonbagTracker>>,
) {
    let strategy = build_exit_strategy(score, &config.exit);

    match executor.execute_buy(token, amount_sol).await {
        Ok(tx) => {
            // Log trade
            let trade = TradeLog {
                id: 0,
                mint: token.mint.clone(),
                symbol: token.symbol.clone(),
                side: "BUY".to_string(),
                sol_amount: amount_sol,
                price: 0.0,
                mcap: 0.0,
                multiplier: 1.0,
                strategy_label: strategy.label.clone(),
                created_at: chrono::Utc::now(),
            };
            let _ = db.log_trade(&trade).await;

            // Save position
            let position = Position {
                id: 0,
                mint: token.mint.clone(),
                symbol: token.symbol.clone(),
                entry_price: 0.0,
                entry_mcap: 0.0,
                original_sol: amount_sol,
                remaining_sol: amount_sol,
                strategy_label: strategy.label.clone(),
                is_moonbag: false,
                stop_loss_triggered: false,
                created_at: chrono::Utc::now(),
            };
            let _ = db.save_position(&position).await;

            // Send buy alert (first-timer variant if applicable)
            if let Some(ft) = &first_timer {
                alerts
                    .send_first_timer_buy_alert(
                        &token.symbol,
                        &token.creator,
                        amount_sol,
                        score,
                        ft.wallet_age_secs,
                        ft.sol_balance,
                        &token.mint,
                        &tx,
                    )
                    .await;
            } else {
                alerts
                    .send_buy_alert(
                        &token.symbol,
                        amount_sol,
                        0.0,
                        0.0,
                        &strategy.label,
                        score,
                        &token.mint,
                        &tx,
                    )
                    .await;
            }

            // Start exit manager in its own task. exit_mgr.monitor() blocks
            // for the life of the position (until all tiers sold or stop
            // loss hits), so wrapping it in tokio::spawn lets
            // execute_buy_and_manage return immediately and frees this
            // per-token task to do other work (e.g., wait for the next
            // token while this position is being managed in the background).
            let mut exit_mgr = ExitManager::new(
                position,
                strategy,
                config.clone(),
                executor.clone(),
                alerts.clone(),
                Arc::new(db.clone()),
                moonbag_tracker.clone(),
            );
            tokio::spawn(async move {
                exit_mgr.monitor().await;
            });
        }
        Err(e) => {
            warn!("Buy failed for {}: {}", token.symbol, e);
            let _ = db.cache_failed_mint(&token.mint, 300).await;
        }
    }
}

/*
/// Map creator status and funding source to the appropriate buy amount.
/// DISABLED — replaced by score_to_amount() for whitelist-only mode.
fn determine_amount(results: &FilterResults, config: &AppConfig) -> f64 {
    let score = results.creator.score;
    let funding = &results.funding.source;

    match results.creator.status {
        CreatorStatus::Whitelisted => config.trading.whitelisted_amount_sol,
        CreatorStatus::FirstTime => {
            // First-time creator — size by funding source
            match funding {
                FundingSource::CEX => config.trading.first_timer_cex_sol,
                FundingSource::NormalWallet => config.trading.default_amount_sol,
                FundingSource::FreshWallet => config.trading.fresh_wallet_amount_sol,
                FundingSource::Mixer => config.trading.mixer_amount_sol,
                FundingSource::Unknown => config.trading.first_timer_unknown_sol,
                FundingSource::SerialRugger => 0.0, // Should never reach here
            }
        }
        _ => {
            // Returning creator — size by score + funding
            match funding {
                FundingSource::CEX => {
                    if score >= 70 {
                        config.trading.whitelisted_amount_sol
                    } else {
                        config.trading.first_timer_cex_sol
                    }
                }
                FundingSource::NormalWallet => config.trading.default_amount_sol,
                FundingSource::FreshWallet => config.trading.fresh_wallet_amount_sol,
                FundingSource::Mixer => config.trading.mixer_amount_sol,
                FundingSource::Unknown => config.trading.first_timer_unknown_sol,
                FundingSource::SerialRugger => 0.0,
            }
        }
    }
}
*/

/// Test-buy mode: one-shot real buy through the production code path,
/// to validate end-to-end latency. Usage:
///   pumpfun-sniper-bot --test-buy <MINT> [--amount 0.005] [--count 1]
async fn run_test_buy(args: Vec<String>) -> Result<()> {
    use std::time::Instant;

    let mint = match args.get(2) {
        Some(m) if !m.is_empty() => m.clone(),
        _ => {
            eprintln!("Usage: --test-buy <MINT> [--amount 0.005] [--count 1]");
            eprintln!("  MINT:  pump.fun token address (must be on bonding curve)");
            eprintln!("  --amount: SOL to spend per buy (default 0.005)");
            eprintln!("  --count:  number of consecutive buys (default 1)");
            return Err(anyhow::anyhow!("missing MINT argument"));
        }
    };

    let amount_sol: f64 = args
        .iter()
        .position(|a| a == "--amount")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.005);

    let count: usize = args
        .iter()
        .position(|a| a == "--count")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    info!("=== TEST-BUY MODE ===");
    info!("  mint:      {}", mint);
    info!("  amount:    {} SOL", amount_sol);
    info!("  count:     {}", count);
    info!("  skip_sim:  {}", skip_simulation_flag());

    // Load config (same as production)
    let config = AppConfig::load("config.toml")?;

    // Load private key
    let pk_b58 = std::env::var("SOLANA_PRIVATE_KEY")
        .expect("SOLANA_PRIVATE_KEY must be set in .env");
    let private_key = bs58::decode(&pk_b58).into_vec()
        .expect("Failed to decode SOLANA_PRIVATE_KEY (must be base58)");

    // Build RPC client (same builder as TradeExecutor)
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    // Build blockhash cache (shared with executor)
    let blockhash_cache = BlockhashCache::new(client.clone(), config.solana.rpc_url.clone());
    spawn_refresh_loop(blockhash_cache.clone());

    // Warm the cache (waits for first refresh to complete)
    let warm_start = Instant::now();
    blockhash_cache.get().await?;
    info!("Blockhash cache warmed in {:?}", warm_start.elapsed());

    // Build executor (uses blockhash cache internally)
    let executor = Arc::new(TradeExecutor::new(
        config.clone(),
        private_key.clone(),
        blockhash_cache.clone(),
    ));

    // Build token data (creator/metadata not used by buy path)
    let token = TokenData {
        mint: mint.clone(),
        symbol: "TEST".to_string(),
        name: "Test Buy".to_string(),
        creator: "11111111111111111111111111111111".to_string(),
        created_at: chrono::Utc::now(),
        metadata_uri: String::new(),
    };

    // Run N consecutive buys
    let mut total = std::time::Duration::ZERO;
    for i in 0..count {
        let start = Instant::now();
        // Call buy_on_bonding_curve directly to see the raw error
        let result = pumpfun_sniper_bot::executor::pumpfun::buy_on_bonding_curve(
            &config,
            &private_key,
            &token,
            amount_sol,
            executor.get_client(),
            &blockhash_cache,
        )
        .await;
        let elapsed = start.elapsed();
        total += elapsed;
        match result {
            Ok(tx) => {
                info!(
                    "[TEST BUY #{}] ✅ {} | {:.4} SOL | {}ms | tx: {}",
                    i + 1, mint, amount_sol, elapsed.as_millis(), tx
                );
            }
            Err(e) => {
                let raw = e.to_string();
                warn!(
                    "[TEST BUY #{}] ❌ {} | {:.4} SOL | {}ms | raw error: {}",
                    i + 1, mint, amount_sol, elapsed.as_millis(), raw
                );
            }
        }
    }
    info!(
        "=== TEST-BUY COMPLETE: {}/{} successful, avg {}ms ===",
        count,
        count,
        if count > 0 { total.as_millis() / count as u128 } else { 0 }
    );
    Ok(())
}

/// Helper: read skip_simulation from config (replaces the inline cfg access in
/// run_test_buy so we don't have to load the full config for a single value).
fn skip_simulation_flag() -> bool {
    AppConfig::load("config.toml")
        .map(|c| c.trading.skip_simulation)
        .unwrap_or(true)
}


/// Test-sell mode: one-shot real sell through the production code path.
/// Usage:
///   pumpfun-sniper-bot --test-sell <MINT> [--amount 5000] [--count 1]
async fn run_test_sell(args: Vec<String>) -> Result<()> {
    use std::time::Instant;

    let mint = match args.get(2) {
        Some(m) if !m.is_empty() => m.clone(),
        _ => {
            eprintln!("Usage: --test-sell <MINT> [--amount 5000] [--count 1]");
            eprintln!("  MINT:  pump.fun token address (must be on bonding curve, in wallet)");
            eprintln!("  --amount: tokens to sell in UI units (default 5000)");
            eprintln!("  --count:  number of consecutive sells (default 1)");
            return Err(anyhow::anyhow!("missing MINT argument"));
        }
    };

    let amount_tokens: f64 = args
        .iter()
        .position(|a| a == "--amount")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(5000.0);

    let count: usize = args
        .iter()
        .position(|a| a == "--count")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    info!("=== TEST-SELL MODE ===");
    info!("  mint:    {}", mint);
    info!("  amount:  {} tokens", amount_tokens);
    info!("  count:   {}", count);
    info!("  skip_sim: {}", skip_simulation_flag());

    // Load config (same as production)
    let config = AppConfig::load("config.toml")?;

    // Load private key
    let pk_b58 = std::env::var("SOLANA_PRIVATE_KEY")
        .expect("SOLANA_PRIVATE_KEY must be set in .env");
    let private_key = bs58::decode(&pk_b58).into_vec()
        .expect("Failed to decode SOLANA_PRIVATE_KEY (must be base58)");

    // Build RPC client
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    // Build blockhash cache
    let blockhash_cache = BlockhashCache::new(client.clone(), config.solana.rpc_url.clone());
    spawn_refresh_loop(blockhash_cache.clone());
    let warm_start = Instant::now();
    blockhash_cache.get().await?;
    info!("Blockhash cache warmed in {:?}", warm_start.elapsed());

    // Build token data
    let token = TokenData {
        mint: mint.clone(),
        symbol: "TEST".to_string(),
        name: "Test Sell".to_string(),
        creator: "11111111111111111111111111111111".to_string(),
        created_at: chrono::Utc::now(),
        metadata_uri: String::new(),
    };

    // Run N consecutive sells
    let mut total = std::time::Duration::ZERO;
    for i in 0..count {
        let start = Instant::now();
        let result = pumpfun_sniper_bot::executor::pumpfun::sell_on_bonding_curve(
            &config,
            &private_key,
            &token,
            amount_tokens,
            &client,
            &blockhash_cache,
        )
        .await;
        let elapsed = start.elapsed();
        total += elapsed;
        match result {
            Ok(tx) => {
                info!(
                    "[TEST SELL #{}] ✅ {} | {:.4} tokens | {}ms | tx: {}",
                    i + 1, mint, amount_tokens, elapsed.as_millis(), tx
                );
            }
            Err(e) => {
                let raw = e.to_string();
                warn!(
                    "[TEST SELL #{}] ❌ {} | {:.4} tokens | {}ms | raw error: {}",
                    i + 1, mint, amount_tokens, elapsed.as_millis(), raw
                );
            }
        }
    }
    info!(
        "=== TEST-SELL COMPLETE: {}/{} attempts, avg {}ms ===",
        count,
        count,
        if count > 0 { total.as_millis() / count as u128 } else { 0 }
    );
    Ok(())
}




/// Test-buy-and-sell mode: in-process round-trip that proves the curve
/// cache works (the sell's fetch should HIT the buy's cached entry).
/// Reports P&L in tokens + SOL + USD.
/// Usage:
///   pumpfun-sniper-bot --test-buy-and-sell <MINT> [--buy 0.005] [--sell 5000] [--wait 2]
async fn run_test_buy_and_sell(args: Vec<String>) -> Result<()> {
    use std::time::{Duration, Instant};

    let mint = match args.get(2) {
        Some(m) if !m.is_empty() => m.clone(),
        _ => {
            eprintln!("Usage: --test-buy-and-sell <MINT> [--buy 0.005] [--sell 5000] [--wait 2]");
            eprintln!("  MINT:    pump.fun token address");
            eprintln!("  --buy:   SOL to spend (default 0.005)");
            eprintln!("  --sell:  tokens to sell back (default 5000)");
            eprintln!("  --wait:  seconds to wait between buy and sell (default 2)");
            return Err(anyhow::anyhow!("missing MINT argument"));
        }
    };

    let buy_sol: f64 = args
        .iter().position(|a| a == "--buy").and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok()).unwrap_or(0.005);
    let sell_tokens: f64 = args
        .iter().position(|a| a == "--sell").and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok()).unwrap_or(5000.0);
    let wait_secs: u64 = args
        .iter().position(|a| a == "--wait").and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok()).unwrap_or(2);

    info!("=== TEST-BUY-AND-SELL (round-trip, in-process) ===");
    info!("  mint:    {}", mint);
    info!("  buy:     {} SOL", buy_sol);
    info!("  sell:    {} tokens", sell_tokens);
    info!("  wait:    {}s between", wait_secs);

    let config = AppConfig::load("config.toml")?;
    let pk_b58 = std::env::var("SOLANA_PRIVATE_KEY").expect("SOLANA_PRIVATE_KEY must be set in .env");
    let private_key = bs58::decode(&pk_b58).into_vec().expect("Failed to decode SOLANA_PRIVATE_KEY");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30)).build()?;
    let blockhash_cache = BlockhashCache::new(client.clone(), config.solana.rpc_url.clone());
    spawn_refresh_loop(blockhash_cache.clone());
    let warm = Instant::now();
    blockhash_cache.get().await?;
    info!("Blockhash cache warmed in {:?}", warm.elapsed());

    let token = TokenData {
        mint: mint.clone(),
        symbol: "TEST".to_string(),
        name: "Round-Trip".to_string(),
        creator: "11111111111111111111111111111111".to_string(),
        created_at: chrono::Utc::now(),
        metadata_uri: String::new(),
    };

    // ── Phase 1: Buy ──────────────────────────────────────────────
    info!("─── Phase 1: BUY ───");
    let buy_start = Instant::now();
    let buy_tx = pumpfun_sniper_bot::executor::pumpfun::buy_on_bonding_curve(
        &config, &private_key, &token, buy_sol, &client, &blockhash_cache,
    ).await?;
    let buy_elapsed = buy_start.elapsed();
    info!("✅ Buy confirmed in {}ms: {}", buy_elapsed.as_millis(), buy_tx);

    // ── Phase 2: Wait ──────────────────────────────────────────────
    info!("─── Phase 2: WAIT {}s (cache should stay warm) ───", wait_secs);
    tokio::time::sleep(Duration::from_secs(wait_secs)).await;

    // ── Phase 3: Sell (should HIT the cache populated by the buy) ──
    info!("─── Phase 3: SELL (expecting 'Curve cache HIT') ───");
    let sell_start = Instant::now();
    let sell_tx = pumpfun_sniper_bot::executor::pumpfun::sell_on_bonding_curve(
        &config, &private_key, &token, sell_tokens, &client, &blockhash_cache,
    ).await?;
    let sell_elapsed = sell_start.elapsed();
    info!("✅ Sell confirmed in {}ms: {}", sell_elapsed.as_millis(), sell_tx);

    // ── Summary ────────────────────────────────────────────────────
    info!("");
    info!("══════════════ ROUND-TRIP SUMMARY ══════════════");
    info!("Buy:  {} SOL  | total {}ms | tx: {}", buy_sol, buy_elapsed.as_millis(), buy_tx);
    info!("Sell: {} tokens | total {}ms | tx: {}", sell_tokens, sell_elapsed.as_millis(), sell_tx);
    info!("  (totals include the on-chain 'processed' confirmation wait)");
    info!("  (look for 'TX submitted in Nms' log line for the actual submission time)");
    let total_ms = buy_elapsed.as_millis() + sell_elapsed.as_millis();
    info!("Total wall time: {}ms", total_ms);

    // Cache note: the buy populated the curve cache. After the sell, the
    // position is closed — clean up the cache to free memory.
    pumpfun_sniper_bot::executor::pumpfun::curve_cache_remove(&mint);

    info!("═══════════════════════════════════════════════");
    Ok(())
}
