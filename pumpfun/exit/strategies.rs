use crate::types::*;
use crate::config::ExitProfileConfig;

pub fn build_strategy(label: &str, config: &ExitProfileConfig, moonbag_pct: f64) -> ExitStrategy {
    let tiers: Vec<ExitTier> = config
        .tiers
        .iter()
        .map(|t| ExitTier {
            sell_pct: t.sell_pct,
            target_multiplier: t.target_multiplier,
            sold: false,
        })
        .collect();

    ExitStrategy {
        label: label.to_string(),
        tiers,
        moonbag_pct,
    }
}
