// PumpSwap — pump.fun's constant-product AMM that tokens migrate to after graduating.
//
// Tokens that graduate from the pump.fun bonding curve (curve.complete == true) move
// their liquidity to a PumpSwap pool. The whitelist is designed to avoid graduated
// tokens, so this path is rarely hit in practice.
//
// This module is currently a STUB — buy/sell return an explicit error and the
// graduated token is skipped with a clear log line. To enable real PumpSwap swaps,
// implement the swap instruction against the PumpSwap program.
//
// References for implementation:
//   Program: pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMgkX
//   IDL/docs: https://github.com/pump-fun/pump-swap-sdk

use anyhow::{Result, anyhow};
use reqwest::Client;
use tracing::warn;

use crate::config::AppConfig;
use crate::types::TokenData;

/// Buy a token that has graduated from pump.fun bonding curve to PumpSwap AMM.
pub async fn buy_on_pumpswap(
    _config: &AppConfig,
    _private_key: &[u8],
    token: &TokenData,
    _amount_sol: f64,
    _client: &Client,
) -> Result<String> {
    warn!("PUMPSWAP buy skipped (not implemented): {} ({})", token.symbol, token.mint);
    Err(anyhow!(
        "PumpSwap buy not implemented. Token {} has graduated from pump.fun \
         bonding curve. Add swap instruction to executor/pumpswap.rs to enable.",
        token.symbol
    ))
}

/// Sell a token via PumpSwap AMM (for graduated tokens).
pub async fn sell_on_pumpswap(
    _config: &AppConfig,
    _private_key: &[u8],
    token: &TokenData,
    _token_amount: f64,
    _token_decimals: u8,
    _client: &Client,
) -> Result<String> {
    warn!("PUMPSWAP sell skipped (not implemented): {} ({})", token.symbol, token.mint);
    Err(anyhow!(
        "PumpSwap sell not implemented. Token {} has graduated from pump.fun \
         bonding curve. Add swap instruction to executor/pumpswap.rs to enable.",
        token.symbol
    ))
}
