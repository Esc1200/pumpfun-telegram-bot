use tracing::info;

use crate::config::MarketCapFilterConfig;
use crate::types::*;
use crate::filters::pumpfun_data::PumpfunCoinData;

pub fn check_market_cap(
    token: &TokenData,
    config: &MarketCapFilterConfig,
    coin: &PumpfunCoinData,
) -> MarketCapData {
    let current_mcap = coin.usd_market_cap;
    let velocity = 0.0;
    let is_hot = false;
    let effective_max = config.max_mcap_usd;

    let mcap_pass = current_mcap >= config.min_mcap_usd && current_mcap <= effective_max;

    if !mcap_pass {
        info!("Market cap: ${:.0} (range ${:.0}-${:.0}) -> FAIL", current_mcap, config.min_mcap_usd, effective_max);
    } else {
        info!("Market cap: ${:.0} (range ${:.0}-${:.0}) -> PASS", current_mcap, config.min_mcap_usd, effective_max);
    }

    MarketCapData { current_mcap, velocity, is_hot, passed: mcap_pass }
}
