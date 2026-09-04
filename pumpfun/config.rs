use serde::Deserialize;
use anyhow::Result;
use std::fs;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub solana: SolanaConfig,
    pub pumpfun: PumpfunConfig,
    pub trading: TradingConfig,
    pub filters: FiltersConfig,
    pub exit: ExitConfig,
    pub parallel: ParallelConfig,
    pub tracker: TrackerConfig,
    pub alerts: AlertsConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SolanaConfig {
    pub rpc_url: String,
    pub ws_url: String,
    pub commitment: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PumpfunConfig {
    pub program_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TradingConfig {
    pub whitelisted_amount_sol: f64,
    pub first_timer_cex_sol: f64,
    pub first_timer_unknown_sol: f64,
    #[allow(dead_code)]
    pub mixer_amount_sol: f64,
    #[allow(dead_code)]
    pub fresh_wallet_amount_sol: f64,
    pub default_amount_sol: f64,
    pub min_sol_balance: f64,

    // First-timer buy path (instant-buy CEX-funded brand-new creators at launch)
    pub first_timer_enabled: bool,
    pub first_timer_max_wallet_age_hours: u64,
    pub first_timer_min_sol_balance: f64,
    pub first_timer_score: u8,

    pub priority_fee: PriorityFeeConfig,

    /// Skip preflight simulation before sendTransaction. Saves ~210ms per buy
    /// but means failed txs (slippage, insufficient funds) pay gas instead
    /// of being caught. Default true — at high token throughput the speed
    /// win outweighs the occasional wasted gas.
    #[serde(default = "default_skip_simulation")]
    pub skip_simulation: bool,
}

fn default_skip_simulation() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct PriorityFeeConfig {
    pub compute_unit_price: u64,
    pub compute_unit_limit: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FiltersConfig {
    pub market_cap: MarketCapFilterConfig,
    #[allow(dead_code)]
    pub velocity: VelocityFilterConfig,
    pub holders: HoldersFilterConfig,
    pub liquidity: LiquidityFilterConfig,
    pub creator: CreatorFilterConfig,
    pub funding: FundingFilterConfig,
    pub token_age: TokenAgeFilterConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketCapFilterConfig {
    pub min_mcap_usd: f64,
    pub max_mcap_usd: f64,
    #[allow(dead_code)]
    pub dynamic_max_enabled: bool,
    #[allow(dead_code)]
    pub velocity_multiplier: f64,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct VelocityFilterConfig {
    pub min_mcap_change_per_sec: f64,
    pub sample_interval_ms: u64,
    pub hot_threshold: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HoldersFilterConfig {
    pub max_top_holder_pct: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LiquidityFilterConfig {
    pub min_sol_in_curve: f64,
    pub min_unique_buyers: u32,
    pub max_single_buyer_pct: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreatorFilterConfig {
    pub max_previous_tokens: u32,
    pub min_avg_mcap_usd: f64,
    pub max_rug_rate: f64,
    pub min_graduation_rate: f64,
    pub blacklist_threshold: u32,
    pub min_data_tokens: u32,
    pub min_score_whitelist: u8,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FundingFilterConfig {
    pub max_hops: u8,
    pub fresh_wallet_age_secs: u64,
    pub min_funder_tx_count: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenAgeFilterConfig {
    pub delay_before_check_secs: u64,
    pub max_token_age_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExitConfig {
    pub tier1: ExitProfileConfig,
    pub tier2: ExitProfileConfig,
    pub tier3: ExitProfileConfig,
    pub stop_loss: StopLossConfig,
    pub moonbag: MoonbagConfig,
    pub moonbag_pct: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExitProfileConfig {
    pub tiers: Vec<ExitTierConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExitTierConfig {
    pub sell_pct: f64,
    pub target_multiplier: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StopLossConfig {
    pub trigger_pct: f64,
    pub check_interval_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MoonbagConfig {
    /// How often the moonbag monitor loop polls the price, in seconds.
    /// 300 = 5 minutes is a reasonable default.
    pub check_interval_secs: u64,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct ParallelConfig {
    pub max_concurrent_checks: usize,
    pub cache_enabled: bool,
    pub timeout_ms: u64,
    pub min_pass_count: u8,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrackerConfig {
    pub max_loss_sol: f64,
    #[allow(dead_code)]
    pub max_loss_streak: u32,
    #[allow(dead_code)]
    pub daily_reset_hour_utc: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AlertsConfig {
    pub enabled: bool,
    pub alert_on_buy: bool,
    pub alert_on_sell: bool,
    pub alert_on_stop_loss: bool,
    pub alert_on_moonbag_pump: bool,
    pub alert_on_first_timer_buy: bool,
}

impl AppConfig {
    pub fn load(path: &str) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let config: AppConfig = toml::from_str(&content)?;
        Ok(config)
    }
}
