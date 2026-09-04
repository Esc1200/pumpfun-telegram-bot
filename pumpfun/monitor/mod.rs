use anyhow::Result;
use tokio::sync::mpsc;
use tracing::info;

use crate::config::AppConfig;
use crate::types::TokenData;
use crate::monitor::websocket::WebSocketMonitor;
use crate::monitor::parser::TransactionParser;

pub mod websocket;
pub mod parser;

pub async fn start_monitoring(
    config: &AppConfig,
    token_tx: mpsc::Sender<TokenData>,
) -> Result<()> {
    let monitor = WebSocketMonitor::new(config).await?;
    let parser = TransactionParser::new(&config.pumpfun.program_id);

    info!("Starting WebSocket monitor...");
    monitor.listen(token_tx, parser).await
}
