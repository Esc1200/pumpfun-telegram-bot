use tracing::{info, warn};

use crate::config::CreatorFilterConfig;
use crate::types::*;
use crate::tracker::db::Database;

pub async fn check_creator(
    token: &TokenData,
    config: &CreatorFilterConfig,
    db: &Database,
) -> CreatorCheck {
    let wallet = &token.creator;

    // Look up creator history from database
    let history = db.get_creator_history(wallet).await;

    // First-time creator: pass to next stage
    if history.is_none() || history.as_ref().unwrap().total_tokens == 0 {
        info!("First-time creator: {} -> PASS to next stage", &wallet[..wallet.len().min(8)]);
        return CreatorCheck {
            wallet: wallet.clone(),
            status: CreatorStatus::FirstTime,
            score: 50,
            previous_tokens: 0,
            avg_mcap: 0.0,
            rug_rate: 0.0,
            passed: true,
        };
    }

    let h = history.unwrap();

    // Returning creator: full vetting
    let avg_mcap_pass = h.avg_mcap > config.min_avg_mcap_usd;
    let rug_rate = if h.total_tokens > 0 {
        h.rug_count as f64 / h.total_tokens as f64
    } else {
        0.0
    };
    let rug_rate_pass = rug_rate < config.max_rug_rate;
    let grad_rate = if h.total_tokens > 0 {
        h.graduated_count as f64 / h.total_tokens as f64
    } else {
        0.0
    };
    let grad_rate_pass = grad_rate > config.min_graduation_rate;
    let enough_data = h.total_tokens >= config.min_data_tokens;

    // Calculate score
    let mut score: u8 = 0;
    if avg_mcap_pass { score += 40; }
    if rug_rate_pass { score += 30; }
    if grad_rate_pass { score += 20; }
    if enough_data { score += 10; }

    let status = if score >= config.min_score_whitelist {
        info!(
            "Creator {} WHITELISTED (score: {}, avg_mcap: ${:.0}, rug_rate: {:.0}%)",
            &wallet[..wallet.len().min(8)], score, h.avg_mcap, rug_rate * 100.0
        );
        CreatorStatus::Whitelisted
    } else if !rug_rate_pass && h.total_tokens >= config.blacklist_threshold {
        warn!(
            "Creator {} BLACKLISTED (rug_rate: {:.0}%, tokens: {})",
            &wallet[..wallet.len().min(8)], rug_rate * 100.0, h.total_tokens
        );
        // Auto-blacklist
        let _ = db.blacklist_wallet(wallet).await;
        CreatorStatus::Blacklisted
    } else {
        CreatorStatus::Unknown
    };

    let passed = status != CreatorStatus::Blacklisted;

    CreatorCheck {
        wallet: wallet.clone(),
        status,
        score,
        previous_tokens: h.total_tokens,
        avg_mcap: h.avg_mcap,
        rug_rate,
        passed,
    }
}
