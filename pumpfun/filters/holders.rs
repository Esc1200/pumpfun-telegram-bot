use tracing::{info, warn};

use crate::config::HoldersFilterConfig;
use crate::types::*;
use crate::filters::pumpfun_data::PumpfunCoinData;
use crate::utils::rpc::SolanaRpc;

/// Check holder concentration using real on-chain data from getTokenLargestAccounts.
/// The bonding curve (liquidity pool) is excluded — we only care about individual wallets.
/// Rule: if any single wallet holds >30% of supply (excluding curve), skip.
pub async fn check_holders(
    _token: &TokenData,
    config: &HoldersFilterConfig,
    coin: &PumpfunCoinData,
    rpc: &SolanaRpc,
) -> HolderData {
    // Get top holders from on-chain data (returns token accounts, not owners)
    let holders = match rpc.get_token_holders(&coin.mint).await {
        Ok(h) => h,
        Err(e) => {
            // Token-2022 mints fail getTokenLargestAccounts on most RPCs
            // Use bonding curve data as estimate: worst case one buyer owns all sold tokens
            let sold_pct = 1.0 - (coin.real_token_reserves as f64 / coin.total_supply as f64);
            let passed = sold_pct <= config.max_top_holder_pct;
            warn!(
                "Holder RPC failed ({}), using sold_pct={:.1}% as upper bound: {}",
                e, sold_pct * 100.0, if passed { "PASS" } else { "FAIL" }
            );
            return HolderData {
                unique_count: 0,
                top_holder_pct: sold_pct,
                passed,
            };
        }
    };

    let total_supply = coin.total_supply as f64;
    let curve_account = &coin.associated_bonding_curve;

    // Find the largest holder excluding the bonding curve's associated token account
    let mut max_pct: f64 = 0.0;
    let mut max_address = String::new();

    for holder in &holders {
        // Skip the bonding curve's token account (that's the liquidity pool)
        if holder.owner == *curve_account {
            continue;
        }

        let pct = holder.amount / total_supply;
        if pct > max_pct {
            max_pct = pct;
            max_address = holder.owner.clone();
        }
    }

    // Estimate unique holders from bonding curve activity (for logging)
    let sol_in_curve = coin.real_sol_reserves as f64 / 1_000_000_000.0;
    let tokens_sold = coin.total_supply.saturating_sub(coin.real_token_reserves);
    let sold_pct = tokens_sold as f64 / total_supply;
    let unique_count = if sol_in_curve >= 0.5 {
        ((sol_in_curve / 0.05) as u32).max(1)
    } else {
        0
    };

    let concentration_pass = max_pct <= config.max_top_holder_pct;
    let passed = concentration_pass;

    if !concentration_pass {
        warn!(
            "Top holder: {} holds {:.1}% (max {:.0}%) -> FAIL",
            &max_address[..8.min(max_address.len())],
            max_pct * 100.0,
            config.max_top_holder_pct * 100.0
        );
    } else {
        info!(
            "Top holder (excl. curve): {:.1}% (max {:.0}%), sold={:.1}% -> PASS",
            max_pct * 100.0,
            config.max_top_holder_pct * 100.0,
            sold_pct * 100.0,
        );
    }

    HolderData {
        unique_count,
        top_holder_pct: max_pct,
        passed,
    }
}
