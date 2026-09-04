use chrono::Utc;
use tracing::warn;

use crate::config::TrackerConfig;
use crate::tracker::db::Database;

pub struct PnlTracker {
    config: TrackerConfig,
    db: Database,
}

impl PnlTracker {
    pub fn new(config: TrackerConfig, db: Database) -> Self {
        Self { config, db }
    }

    pub async fn check_daily_loss(&self) -> bool {
        let today = Utc::now()
            .format("%Y-%m-%d")
            .to_string();

        let trades = match self.db.get_trades_since(&today).await {
            Ok(t) => t,
            Err(_) => return true, // Allow trading on error
        };

        let total_loss: f64 = trades
            .iter()
            .filter(|t| t.side == "STOP_LOSS" || t.side == "SELL")
            .map(|t| {
                if t.multiplier < 1.0 {
                    t.sol_amount * (1.0 - t.multiplier).abs()
                } else {
                    0.0
                }
            })
            .sum();

        if total_loss >= self.config.max_loss_sol {
            warn!(
                "Daily loss limit reached: {:.4} SOL (max: {:.4} SOL) - STOPPING",
                total_loss, self.config.max_loss_sol
            );
            return false;
        }

        true
    }

    pub async fn get_daily_summary(&self) -> String {
        let today = Utc::now()
            .format("%Y-%m-%d")
            .to_string();

        let trades = match self.db.get_trades_since(&today).await {
            Ok(t) => t,
            Err(e) => return format!("Error fetching trades: {}", e),
        };

        let total_trades = trades.len();
        let buys = trades.iter().filter(|t| t.side == "BUY").count();
        let sells = trades.iter().filter(|t| t.side == "SELL").count();
        let stop_losses = trades.iter().filter(|t| t.side == "STOP_LOSS").count();

        let total_spent: f64 = trades
            .iter()
            .filter(|t| t.side == "BUY")
            .map(|t| t.sol_amount)
            .sum();

        let total_received: f64 = trades
            .iter()
            .filter(|t| t.side == "SELL" || t.side == "STOP_LOSS")
            .map(|t| t.sol_amount * t.multiplier)
            .sum();

        let pnl = total_received - total_spent;

        format!(
            "Daily Summary ({})\n\
             Trades: {} (B: {} S: {} SL: {})\n\
             Spent: {:.4} SOL\n\
             Received: {:.4} SOL\n\
             PnL: {:.4} SOL ({:+.1}%)",
            today,
            total_trades, buys, sells, stop_losses,
            total_spent,
            total_received,
            pnl,
            if total_spent > 0.0 { pnl / total_spent * 100.0 } else { 0.0 }
        )
    }
}
