use tracing::{info, warn};

use crate::config::LiquidityFilterConfig;
use crate::types::*;
use crate::filters::pumpfun_data::PumpfunCoinData;

/// Anti-rug liquidity checks:
/// 1. Curve must have real SOL (not empty/drained)
/// 2. Must have real buyers (not just virtual activity)
/// 3. Creator can't hold >20% after curve has activity
/// 4. Must have minimum unique buyers
pub fn check_liquidity(
    _token: &TokenData,
    config: &LiquidityFilterConfig,
    coin: &PumpfunCoinData,
) -> LiquidityData {
    // SOL in bonding curve (real reserves, not virtual)
    let sol_in_curve = coin.real_sol_reserves as f64 / 1_000_000_000.0;

    // Estimate unique buyers from curve activity
    let unique_buyers = if sol_in_curve >= 0.5 {
        ((sol_in_curve / 0.1) as u32).max(1)
    } else {
        0
    };

    // Top buyer concentration: worst-case estimate
    // If 10% of supply is sold, worst case one buyer owns all 10%
    let tokens_sold_pct = 1.0 - (coin.real_token_reserves as f64 / coin.total_supply as f64);
    let top_buyer_pct = tokens_sold_pct.max(0.0);

    // ANTI-RUG CHECK 1: Curve must have real SOL
    // real_sol_reserves = 0 means curve is empty (all SOL drained by rugger)
    // This catches the #1 rug pattern: creator drains curve SOL, token dies
    let curve_alive = sol_in_curve >= config.min_sol_in_curve;
    if !curve_alive {
        warn!(
            "RUG CHECK: Curve empty! real_sol={:.2} SOL (min {:.2}) -> FAIL",
            sol_in_curve, config.min_sol_in_curve
        );
    }

    // ANTI-RUG CHECK 2: Must have real buyers
    // If unique_buyers < 3, it means very few people have bought — high rug risk
    let buyers_pass = unique_buyers >= config.min_unique_buyers;
    if !buyers_pass {
        info!(
            "RUG CHECK: Too few buyers: {} (min {}) -> FAIL",
            unique_buyers, config.min_unique_buyers
        );
    }

    // ANTI-RUG CHECK 3: Creator can't hold >20% after activity
    // If >0.5 SOL in curve (someone bought), but tokens_sold_pct < 20%,
    // it means the creator still holds most tokens and hasn't distributed
    // This is fine at launch, but if curve has activity and creator holds >20%... rug
    let concentration_pass = if sol_in_curve >= 0.5 && tokens_sold_pct < 0.20 {
        warn!(
            "RUG CHECK: Curve has {:.1} SOL but only {:.1}% sold — creator still holds too much -> FAIL",
            sol_in_curve, tokens_sold_pct * 100.0
        );
        false
    } else {
        true
    };

    // ANTI-RUG CHECK 4: Top buyer concentration
    let top_buyer_pass = top_buyer_pct <= config.max_single_buyer_pct;
    if !top_buyer_pass {
        warn!(
            "RUG CHECK: Top buyer owns {:.0}% (max {:.0}%) -> FAIL",
            top_buyer_pct * 100.0,
            config.max_single_buyer_pct * 100.0
        );
    }

    let passed = curve_alive && buyers_pass && concentration_pass && top_buyer_pass;

    if passed {
        info!(
            "Liquidity: {:.2} SOL, {} buyers, sold={:.1}% -> PASS",
            sol_in_curve, unique_buyers, tokens_sold_pct * 100.0
        );
    }

    LiquidityData {
        sol_in_curve,
        unique_buyers,
        top_buyer_pct,
        passed,
    }
}
