use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ═══════════════════════════════════════
// Token Data
// ═══════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenData {
    pub mint: String,
    pub symbol: String,
    pub name: String,
    pub creator: String,
    pub created_at: DateTime<Utc>,
    pub metadata_uri: String,
}

// ═══════════════════════════════════════
// Creator
// ═══════════════════════════════════════

#[derive(Debug, Clone, PartialEq)]
pub enum CreatorStatus {
    FirstTime,
    Whitelisted,
    Blacklisted,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct CreatorCheck {
    pub wallet: String,
    pub status: CreatorStatus,
    pub score: u8,
    pub previous_tokens: u32,
    pub avg_mcap: f64,
    pub rug_rate: f64,
    pub passed: bool,
}

// ═══════════════════════════════════════
// Funding
// ═══════════════════════════════════════

#[derive(Debug, Clone, PartialEq)]
pub enum FundingSource {
    CEX,
    NormalWallet,
    SerialRugger,
    FreshWallet,
    Mixer,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct FundingCheck {
    pub source: FundingSource,
    pub funder_wallet: String,
    pub hops_checked: u8,
    pub passed: bool,
}

// ═══════════════════════════════════════
// Holders
// ═══════════════════════════════════════

#[derive(Debug, Clone)]
pub struct HolderData {
    pub unique_count: u32,
    pub top_holder_pct: f64,
    pub passed: bool,
}

// ═══════════════════════════════════════
// Market Cap
// ═══════════════════════════════════════

#[derive(Debug, Clone)]
pub struct MarketCapData {
    pub current_mcap: f64,
    pub velocity: f64,
    pub is_hot: bool,
    pub passed: bool,
}

// ═══════════════════════════════════════
// Liquidity
// ═══════════════════════════════════════

#[derive(Debug, Clone)]
pub struct LiquidityData {
    pub sol_in_curve: f64,
    pub unique_buyers: u32,
    pub top_buyer_pct: f64,
    pub passed: bool,
}

// ═══════════════════════════════════════
// Cache
// ═══════════════════════════════════════



// ═══════════════════════════════════════
// Filter Results (all parallel)
// ═══════════════════════════════════════

#[derive(Debug, Clone)]
pub struct FilterResults {
    pub creator: CreatorCheck,
    pub funding: FundingCheck,
    pub liquidity: LiquidityData,
    pub holders: HolderData,
    pub market_cap: MarketCapData,
    pub token_age_secs: u64,
    pub creator_db_hit: bool,
    pub final_pass: bool,
}

// ═══════════════════════════════════════
// Filter Result (individual)
// ═══════════════════════════════════════

#[derive(Debug, Clone)]
pub struct FilterResult {
    pub passed: bool,
    pub name: String,
    pub details: String,
}

// ═══════════════════════════════════════
// Holder Info (for RPC queries)
// ═══════════════════════════════════════

#[derive(Debug, Clone)]
pub struct HolderInfo {
    pub owner: String,
    pub amount: f64,
}

// ═══════════════════════════════════════
// Position
// ═══════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub id: i64,
    pub mint: String,
    pub symbol: String,
    pub entry_price: f64,
    pub entry_mcap: f64,
    pub original_sol: f64,
    pub remaining_sol: f64,
    pub strategy_label: String,
    pub is_moonbag: bool,
    pub stop_loss_triggered: bool,
    pub created_at: DateTime<Utc>,
}

// ═══════════════════════════════════════
// Exit Strategy
// ═══════════════════════════════════════

#[derive(Debug, Clone)]
pub struct ExitTier {
    pub sell_pct: f64,
    pub target_multiplier: f64,
    pub sold: bool,
}

#[derive(Debug, Clone)]
pub struct ExitStrategy {
    pub label: String,
    pub tiers: Vec<ExitTier>,
    pub moonbag_pct: f64,
}

// ═══════════════════════════════════════
// Trade Log
// ═══════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeLog {
    pub id: i64,
    pub mint: String,
    pub symbol: String,
    pub side: String,
    pub sol_amount: f64,
    pub price: f64,
    pub mcap: f64,
    pub multiplier: f64,
    pub strategy_label: String,
    pub created_at: DateTime<Utc>,
}

// ═══════════════════════════════════════
// Creator History (from DB)
// ═══════════════════════════════════════

#[derive(Debug, Clone)]
pub struct CreatorHistory {
    pub wallet: String,
    pub total_tokens: u32,
    pub avg_mcap: f64,
    pub rug_count: u32,
    pub graduated_count: u32,
}

// ═══════════════════════════════════════
// WebSocket
// ═══════════════════════════════════════

#[derive(Debug, Serialize, Deserialize)]
pub struct WsSubscription {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WsNotification {
    pub jsonrpc: String,
    pub method: Option<String>,
    pub params: Option<serde_json::Value>,
}
