pub mod creator;
pub mod funding;
pub mod first_timer;
pub mod holders;
pub mod liquidity;
pub mod market_cap;
pub mod market_cap_velocity;
pub mod pumpfun_data;

use tracing::{info, warn};

use crate::config::AppConfig;
use crate::types::*;
use crate::tracker::db::Database;
use crate::utils::rpc::SolanaRpc;

pub struct FilterEngine {
    config: AppConfig,
    db: Database,
    rpc: SolanaRpc,
}

impl FilterEngine {
    pub fn new(config: AppConfig, db: Database, rpc: SolanaRpc) -> Self {
        Self { config, db, rpc }
    }

    /// Whitelist-only filter pipeline:
    /// 1. Creator whitelist check (not whitelisted → skip immediately)
    /// 2. Token-level checks (concentration + liquidity)
    /// 3. Market cap ($5K–$30K)
    /// 4. Token age (<60s)
    pub async fn run_all_filters(&self, token: &TokenData) -> FilterResults {
        let config = &self.config;
        let db = &self.db;
        let rpc = &self.rpc;

        // ═══════════════════════════════════════════════
        // STAGE 1: Whitelist-only — not whitelisted = skip
        // ═══════════════════════════════════════════════
        if !db.is_whitelisted(&token.creator).await {
            info!("Creator {} NOT in whitelist -> SKIP", &token.creator[..token.creator.len().min(8)]);
            return FilterResults {
                creator: CreatorCheck {
                    wallet: token.creator.clone(),
                    status: CreatorStatus::Unknown,
                    score: 0,
                    previous_tokens: 0,
                    avg_mcap: 0.0,
                    rug_rate: 0.0,
                    passed: false,
                },
                funding: FundingCheck {
                    source: FundingSource::Unknown,
                    funder_wallet: String::new(),
                    hops_checked: 0,
                    passed: false,
                },
                liquidity: LiquidityData { sol_in_curve: 0.0, unique_buyers: 0, top_buyer_pct: 0.0, passed: false },
                holders: HolderData { unique_count: 0, top_holder_pct: 1.0, passed: false },
                market_cap: MarketCapData { current_mcap: 0.0, velocity: 0.0, is_hot: false, passed: false },
                token_age_secs: 0,
                creator_db_hit: false,
                final_pass: false,
            };
        }

        info!("Creator {} is WHITELISTED -> checking token", &token.creator[..token.creator.len().min(8)]);
        let creator_db_hit = true;

        // Stub creator/funding results (no on-chain analysis for whitelisted creators)
        let creator_result = CreatorCheck {
            wallet: token.creator.clone(),
            status: CreatorStatus::Whitelisted,
            score: 99,
            previous_tokens: 0,
            avg_mcap: 0.0,
            rug_rate: 0.0,
            passed: true,
        };
        let funding_result = FundingCheck {
            source: FundingSource::Unknown,
            funder_wallet: String::new(),
            hops_checked: 0,
            passed: true,
        };

        // ═══════════════════════════════════════════════
        // STAGE 3: Token-level checks (parallel)
        // ═══════════════════════════════════════════════
        let coin_data = pumpfun_data::fetch_coin_data(rpc.get_client(), &token.mint).await;

        // Skip graduated tokens — bonding curve is empty, no point buying
        if let Ok(coin) = &coin_data {
            if coin.complete {
                info!("{}: Token has GRADUATED (complete=true) -> SKIP", token.symbol);
                return FilterResults {
                    creator: creator_result,
                    funding: funding_result,
                    liquidity: LiquidityData { sol_in_curve: 0.0, unique_buyers: 0, top_buyer_pct: 0.0, passed: false },
                    holders: HolderData { unique_count: 0, top_holder_pct: 1.0, passed: false },
                    market_cap: MarketCapData { current_mcap: 0.0, velocity: 0.0, is_hot: false, passed: false },
                    token_age_secs: 0,
                    creator_db_hit,
                    final_pass: false,
                };
            }
        }

        let (holders_result, liquidity_result) = match &coin_data {
            Ok(coin) => {
                let h = holders::check_holders(token, &config.filters.holders, coin, rpc).await;
                let l = liquidity::check_liquidity(token, &config.filters.liquidity, coin);
                (h, l)
            }
            Err(e) => {
                warn!("Failed to fetch pump.fun data for {}: {}", token.symbol, e);
                (
                    HolderData { unique_count: 0, top_holder_pct: 1.0, passed: false },
                    LiquidityData { sol_in_curve: 0.0, unique_buyers: 0, top_buyer_pct: 0.0, passed: false },
                )
            }
        };

        info!(
            "Token checks {}: holders(conc={:.0}%)={} liquidity={:.1}SOL={}",
            token.symbol,
            holders_result.top_holder_pct * 100.0,
            if holders_result.passed { "PASS" } else { "FAIL" },
            liquidity_result.sol_in_curve,
            if liquidity_result.passed { "PASS" } else { "FAIL" },
        );

        if !holders_result.passed || !liquidity_result.passed {
            info!("{}: FAILED token checks -> SKIP", token.symbol);
            return FilterResults {
                creator: creator_result,
                funding: funding_result,
                liquidity: liquidity_result,
                holders: holders_result,
                market_cap: MarketCapData { current_mcap: 0.0, velocity: 0.0, is_hot: false, passed: false },
                token_age_secs: 0,
                creator_db_hit,
                final_pass: false,
            };
        }

        // ═══════════════════════════════════════════════
        // STAGE 4: Market cap — LAST (needs time for price to stabilize)
        // ═══════════════════════════════════════════════
        let mcap_result = match &coin_data {
            Ok(coin) => market_cap::check_market_cap(token, &config.filters.market_cap, coin),
            Err(_) => MarketCapData { current_mcap: 0.0, velocity: 0.0, is_hot: false, passed: false },
        };

        if !mcap_result.passed {
            info!("{}: FAILED market cap (${:.0}) -> SKIP", token.symbol, mcap_result.current_mcap);
            return FilterResults {
                creator: creator_result,
                funding: funding_result,
                liquidity: liquidity_result,
                holders: holders_result,
                market_cap: mcap_result,
                token_age_secs: 0,
                creator_db_hit,
                final_pass: false,
            };
        }

        // ═══════════════════════════════════════════════
        // STAGE 5: Token age check (<60s)
        // Use real created_timestamp from pump.fun API (milliseconds since epoch)
        // ═══════════════════════════════════════════════
        let token_age_secs = match &coin_data {
            Ok(coin) if coin.created_timestamp > 0 => {
                let created_ms = coin.created_timestamp;
                let now_ms = chrono::Utc::now().timestamp_millis();
                ((now_ms - created_ms) / 1000).max(0) as u64
            }
            _ => {
                // Fallback: if no API data, use token.created_at (less accurate)
                (chrono::Utc::now() - token.created_at).num_seconds().max(0) as u64
            }
        };
        let max_age = config.filters.token_age.max_token_age_secs;

        if token_age_secs > max_age {
            info!("{}: Token too old ({}s > {}s max) -> SKIP", token.symbol, token_age_secs, max_age);
            return FilterResults {
                creator: creator_result,
                funding: funding_result,
                liquidity: liquidity_result,
                holders: holders_result,
                market_cap: mcap_result,
                token_age_secs,
                creator_db_hit,
                final_pass: false,
            };
        }

        // ═══════════════════════════════════════════════
        // ALL CHECKS PASSED — buy signal
        // ═══════════════════════════════════════════════
        info!(
            "✅ {} PASSED ALL FILTERS (creator:WL holders:{}% liquidity:{:.1}SOL mcap:${:.0} age:{}s) -> BUY",
            token.symbol,
            (holders_result.top_holder_pct * 100.0) as u32,
            liquidity_result.sol_in_curve,
            mcap_result.current_mcap,
            token_age_secs,
        );

        FilterResults {
            creator: creator_result,
            funding: funding_result,
            liquidity: liquidity_result,
            holders: holders_result,
            market_cap: mcap_result,
            token_age_secs,
            creator_db_hit,
            final_pass: true,
        }
    }
}
