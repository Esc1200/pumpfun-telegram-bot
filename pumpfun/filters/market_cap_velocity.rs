use crate::types::*;
use crate::utils::rpc::SolanaRpc;
use tracing::info;

/// NOTE: This filter is currently dead code — it is declared in filters/mod.rs
/// but never called from the filter pipeline. Left here for potential future use.
pub async fn check(
    mint: &str,
    rpc: &SolanaRpc,
    min_velocity: f64,
) -> FilterResult {
    // Get current market cap
    let mcap_now = rpc.get_market_cap(mint).await.unwrap_or(0.0);

    // Wait a short interval
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Get market cap again
    let mcap_later = rpc.get_market_cap(mint).await.unwrap_or(0.0);

    // Calculate velocity (mcap change per second)
    let velocity = if mcap_now > 0.0 {
        (mcap_later - mcap_now) / 0.2 // 200ms interval = 0.2 seconds
    } else {
        0.0
    };

    if velocity >= min_velocity {
        info!("Velocity check passed: ${:.0}/sec", velocity);
        FilterResult {
            passed: true,
            name: "velocity".to_string(),
            details: format!("${:.0}/sec growth", velocity),
        }
    } else {
        info!("Velocity check: ${:.0}/sec (min: ${:.0}/sec)", velocity, min_velocity);
        // Velocity is informational — always pass, just report it
        FilterResult {
            passed: true,
            name: "velocity".to_string(),
            details: format!("${:.0}/sec — below threshold but passed", velocity),
        }
    }
}
