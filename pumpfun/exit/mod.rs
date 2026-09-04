use crate::types::*;
use crate::config::ExitConfig;

pub mod strategies;
pub mod stop_loss;
pub mod moonbag;

/// Build exit strategy based on whitelist score tier.
/// Tier 1 (score >= 75): aggressive targets
/// Tier 2 (score 50-74): moderate targets
/// Tier 3 (score < 50): conservative targets
pub fn build_exit_strategy(score: u8, config: &ExitConfig) -> ExitStrategy {
    if score >= 75 {
        strategies::build_strategy("TIER1", &config.tier1, config.moonbag_pct)
    } else if score >= 50 {
        strategies::build_strategy("TIER2", &config.tier2, config.moonbag_pct)
    } else {
        strategies::build_strategy("TIER3", &config.tier3, config.moonbag_pct)
    }
}
