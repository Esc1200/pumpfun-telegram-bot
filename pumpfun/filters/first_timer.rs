//! First-Timer Buy Path
//!
//! Catches brand-new pump.fun creators with strong "first launch" signals:
//! CEX-funded, fresh wallet, actually has capital to deploy.
//!
//! Buy criteria (conditions 1, 2, 3 must ALL be met):
//!   1. First-timer: 0 prior tokens in our `creator_cache` table
//!   2. Wallet age < `first_timer_max_wallet_age_hours` (default 48h)
//!   3. Funded by a top CEX (cached in `funding_cache` or traced on-chain)
//!   4. SOL balance >= `first_timer_min_sol_balance` (default 1.0) [tracked, not gating]
//!
//! On signal: instant buy at `first_timer_cex_sol` (default 0.02 SOL),
//! add creator to whitelist with `first_timer_score` (default 30) for future launches.

use std::time::Instant;

use tracing::{info, warn};

use crate::config::AppConfig;
use crate::filters::funding;
use crate::tracker::db::Database;
use crate::types::{FundingSource, TokenData};
use crate::utils::rpc::SolanaRpc;
/// Result of a first-timer check.
#[derive(Debug)]
pub enum FirstTimerResult {
    /// All gating conditions met — buy this token.
    Buy {
        amount_sol: f64,
        score: u8,
        /// Seconds since the creator wallet's first transaction.
        wallet_age_secs: u64,
        /// Current SOL balance of the creator wallet.
        sol_balance: f64,
    },
    /// Not eligible — reasons logged.
    Skip {
        reason: &'static str,
    },
}

/// Run the first-timer check against a new token.
///
/// Returns `FirstTimerResult::Buy` if all gating conditions are satisfied,
/// `FirstTimerResult::Skip` otherwise. Caller is responsible for executing the
/// buy and adding the creator to the whitelist on a `Buy` result.
pub async fn check_first_timer_buy(
    token: &TokenData,
    config: &AppConfig,
    db: &Database,
    rpc: &SolanaRpc,
) -> FirstTimerResult {
    let wallet = &token.creator;
    let short = &wallet[..wallet.len().min(8)];
    let started = Instant::now();

    if !config.trading.first_timer_enabled {
        return FirstTimerResult::Skip { reason: "first-timer path disabled" };
    }

    // ──────────────────────────────────────────────────────────────────
    // CONDITION 1: First-timer (no prior tokens in our DB)
    // ──────────────────────────────────────────────────────────────────
    let history = db.get_creator_history(wallet).await;
    if let Some(ref h) = history {
        if h.total_tokens > 0 {
            return FirstTimerResult::Skip { reason: "creator has prior launches" };
        }
    }
    info!("FIRST-TIMER check {} ({}): creator history empty OK", token.symbol, short);

    // ──────────────────────────────────────────────────────────────────
    // CONDITION 2 + 3: Wallet age < N hours AND CEX-funded
    // Both come from the same `trace_wallet_funder` RPC call — do it ONCE.
    // ──────────────────────────────────────────────────────────────────
    let max_age_secs = config.trading.first_timer_max_wallet_age_hours * 3600;
    // (wallet_age_secs, is_cex) — single trace serves both
    let (wallet_age_secs, is_cex) = {
        let cached_age = db.get_wallet_age_secs(wallet).await;
        let cached_funding = db.get_funding_cache(wallet).await;

        // Fast path: both cached, no RPC needed
        if cached_age > 0 && cached_funding.is_some() {
            info!("FIRST-TIMER {}: full cache hit (age={}s, source={:?})", short, cached_age, cached_funding.as_ref().unwrap().source);
            (
                cached_age,
                cached_funding.unwrap().source == FundingSource::CEX,
            )
        } else {
            // Slow path: trace (1 RPC). Use the result for BOTH age and funder.
            match rpc.trace_wallet_funder(wallet).await {
                Ok((funder, tx_count, age)) => {
                    info!(
                        "FIRST-TIMER {}: traced on-chain: funder={} tx_count={} age={}s",
                        short,
                        &funder[..funder.len().min(8)],
                        tx_count,
                        age
                    );
                    let _ = db.save_wallet_funder(wallet, &funder, tx_count, age).await;
                    let is_cex_funder = funding::is_cex_wallet(&funder);
                    let source = if is_cex_funder {
                        FundingSource::CEX
                    } else {
                        FundingSource::NormalWallet
                    };
                    let _ = db.cache_funding(wallet, &source, &funder).await;
                    (age, is_cex_funder)
                }
                Err(e) => {
                    warn!("FIRST-TIMER {}: single trace failed: {}", short, e);
                    // If we have cached age, use it (skip CEX check this round)
                    if cached_age > 0 {
                        (cached_age, false)
                    } else {
                        return FirstTimerResult::Skip { reason: "trace failed" };
                    }
                }
            }
        }
    };

    if wallet_age_secs >= max_age_secs {
        return FirstTimerResult::Skip { reason: "wallet too old" };
    }
    info!(
        "FIRST-TIMER {}: wallet age {}s < {}h OK",
        short, wallet_age_secs, config.trading.first_timer_max_wallet_age_hours
    );

    if !is_cex {
        return FirstTimerResult::Skip { reason: "not CEX-funded" };
    }
    info!("FIRST-TIMER {}: CEX-funded OK", short);

    // ──────────────────────────────────────────────────────────────────
    // CONDITION 4: SOL balance >= N (tracked, not gating per spec)
    // ──────────────────────────────────────────────────────────────────
    let sol_balance = match rpc.get_sol_balance(wallet).await {
        Ok(b) => b,
        Err(e) => {
            warn!("FIRST-TIMER {}: SOL balance lookup failed: {}", short, e);
            0.0
        }
    };
    let min_bal = config.trading.first_timer_min_sol_balance;
    if sol_balance >= min_bal {
        info!(
            "FIRST-TIMER {}: SOL balance {:.2} >= {:.2} OK",
            short, sol_balance, min_bal
        );
    } else {
        info!(
            "FIRST-TIMER {}: SOL balance {:.2} < {:.2} (note: not gating per spec)",
            short, sol_balance, min_bal
        );
    }

    let elapsed_ms = started.elapsed().as_millis();
    info!(
        "FIRST-TIMER BUY SIGNAL: {} | {} | age={}s bal={:.2}SOL | elapsed={}ms",
        token.symbol, short, wallet_age_secs, sol_balance, elapsed_ms
    );

    FirstTimerResult::Buy {
        amount_sol: config.trading.first_timer_cex_sol,
        score: config.trading.first_timer_score,
        wallet_age_secs,
        sol_balance,
    }
}
