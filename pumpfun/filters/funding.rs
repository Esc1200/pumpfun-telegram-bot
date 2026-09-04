use tracing::{info, warn};

use crate::config::FundingFilterConfig;
use crate::types::*;
use crate::tracker::db::Database;
use crate::utils::rpc::SolanaRpc;

// Major CEX hot wallets
pub const CEX_WALLETS: &[&str] = &[
    "5tzFkiKscXHK5ZXCGbXZxdw7gTWeDvhyDqX2kA4u1hyE",
    "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM",
    "H8sMJSCQxfKiFTC5SR4H1LDohFNBjhDdQGmgVZ5PNf7R",
    "2AQdpHJ2JpcEgPiATUX8w8T9mWdRv2jzuHdMq5S7GJdR",
    "6FEVkH1Mk5m6FpzPkPpPHLboRw54XkzE5w8N5YpG3yEP",
    "9un5wqah3b7j6M5c3JPDv8zC3D6BwqD3pFCGZ3JGS4Po",
    "AC5RDfQFmDS1oeFhJPC4nRXGnJW84q7cm66Y4J6oYG9V",
    "Fpwg2JXwBv5avkZSbNzQBjYwVPjHWTjkwZ7DFp7GJz4z",
];

/// Public helper: is this wallet a known CEX hot wallet?
pub fn is_cex_wallet(wallet: &str) -> bool {
    CEX_WALLETS.contains(&wallet)
}

pub async fn check_funding(
    token: &TokenData,
    config: &FundingFilterConfig,
    db: &Database,
    rpc: &SolanaRpc,
) -> FundingCheck {
    let wallet = &token.creator;

    // STEP 1: Check DB cache first (fast path)
    if let Some(cached) = db.get_funding_cache(wallet).await {
        info!("Funding cache hit for {}: {:?}", &wallet[..wallet.len().min(8)], cached.source);
        let passed = cached.source != FundingSource::SerialRugger
            && cached.source != FundingSource::Mixer;
        return FundingCheck {
            source: cached.source,
            funder_wallet: cached.funder_wallet,
            hops_checked: 0,
            passed,
        };
    }

    // STEP 2: Check if we already traced this wallet (wallet_history table)
    if let Some(funder) = db.get_wallet_funder(wallet).await {
        info!("Wallet history hit for {}: funder={}", &wallet[..wallet.len().min(8)], &funder[..funder.len().min(8)]);

        // Classify the funder (wallet_tx_count=0 means we don't know yet)
        let source = classify_funder(&funder, db, config, 0).await;
        let passed = source != FundingSource::SerialRugger
            && source != FundingSource::Mixer;

        // Cache for faster lookup next time
        let _ = db.cache_funding(wallet, &source, &funder).await;

        return FundingCheck {
            source,
            funder_wallet: funder,
            hops_checked: 1,
            passed,
        };
    }

    // STEP 3: No DB data — trace from blockchain (slow path, 2 RPC calls)
    info!("No DB data for {}, tracing from blockchain...", &wallet[..wallet.len().min(8)]);

    let (funder, tx_count, age_secs) = match rpc.trace_wallet_funder(wallet).await {
        Ok(data) => data,
        Err(e) => {
            warn!("Failed to trace funder for {}: {}", &wallet[..wallet.len().min(8)], e);
            // Cache as Unknown so we don't retry immediately
            let _ = db.cache_funding(wallet, &FundingSource::Unknown, "").await;
            return FundingCheck {
                source: FundingSource::Unknown,
                funder_wallet: String::new(),
                hops_checked: 0,
                passed: true, // Unknown passes (not SerialRugger/Mixer)
            };
        }
    };

    info!(
        "Blockchain trace for {}: funder={} tx_count={} age={}s",
        &wallet[..wallet.len().min(8)], &funder[..funder.len().min(8)], tx_count, age_secs
    );

    // Save wallet data to DB (continuous learning)
    let _ = db.save_wallet_funder(wallet, &funder, tx_count, age_secs).await;

    // Classify the funder — use config fields for fresh wallet detection
    let source = if age_secs > 0 && age_secs < config.fresh_wallet_age_secs && tx_count < config.min_funder_tx_count {
        FundingSource::FreshWallet
    } else {
        classify_funder(&funder, db, config, tx_count).await
    };
    let passed = source != FundingSource::SerialRugger
        && source != FundingSource::Mixer;

    // Cache the classification
    let _ = db.cache_funding(wallet, &source, &funder).await;

    // Also check: if this wallet itself is a known rugger, blacklist it
    if let Some(history) = db.get_creator_history(wallet).await {
        if history.total_tokens >= 3 {
            let rug_rate = history.rug_count as f64 / history.total_tokens as f64;
            if rug_rate > 0.5 {
                warn!("Creator {} has rug_rate {:.0}% — blacklisting", &wallet[..wallet.len().min(8)], rug_rate * 100.0);
                let _ = db.blacklist_wallet(wallet).await;
            }
        }
    }

    FundingCheck {
        source,
        funder_wallet: funder,
        hops_checked: 1,
        passed,
    }
}

/// Classify a funder wallet into a FundingSource.
/// Uses config.min_funder_tx_count instead of hardcoded threshold.
async fn classify_funder(funder: &str, db: &Database, config: &FundingFilterConfig, wallet_tx_count: u64) -> FundingSource {
    // Check 1: Known CEX wallet
    if CEX_WALLETS.contains(&funder) {
        return FundingSource::CEX;
    }

    // Check 2: Blacklisted wallet
    if db.is_blacklisted(funder).await {
        return FundingSource::SerialRugger;
    }

    // Check 3: Known rugger (from creator history)
    if let Some(history) = db.get_creator_history(funder).await {
        if history.total_tokens >= 3 {
            let rug_rate = history.rug_count as f64 / history.total_tokens as f64;
            if rug_rate > 0.5 {
                return FundingSource::SerialRugger;
            }
        }
    }

    // Check 4: Wallet with history (using config threshold)
    let tx_count = if wallet_tx_count > 0 { wallet_tx_count } else { db.get_wallet_tx_count(funder).await };
    if tx_count >= config.min_funder_tx_count {
        return FundingSource::NormalWallet;
    }

    // Default: Unknown but not suspicious
    FundingSource::Unknown
}
