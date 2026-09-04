use anyhow::{Result, anyhow};
use reqwest::Client;
use base64::Engine;
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signer::keypair::Keypair,
    signer::Signer,
    system_program,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    transaction::Transaction,
};
use std::str::FromStr;

use crate::blockhash_cache::BlockhashCache;
use crate::config::AppConfig;
use crate::types::TokenData;

// ═══════════════════════════════════════
// Pump.fun Program Constants
// ═══════════════════════════════════════

/// Typed error returned by `buy_on_bonding_curve`. Callers match on
/// `is::<BuyError>()` instead of doing string matching on error messages,
/// which is brittle and breaks if the message is ever reworded.
#[derive(Debug, thiserror::Error)]
pub enum BuyError {
    /// The token's bonding curve has been completed — it has graduated to
    /// PumpSwap AMM. The buy path should route to PumpSwap in this case.
    #[error("Token {symbol} has graduated — route to PumpSwap")]
    Graduated { symbol: String },
}

/// Typed error returned by `sell_on_bonding_curve`.
#[derive(Debug, thiserror::Error)]
pub enum SellError {
    /// Same as BuyError::Graduated, but for the sell path. Caller should
    /// route to the PumpSwap AMM sell rather than failing.
    #[error("Token {symbol} has graduated — route to PumpSwap")]
    Graduated { symbol: String },
}

const PUMP_FUN_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
const PUMP_GLOBAL: &str = "4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf";
const PUMP_EVENT_AUTHORITY: &str = "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1";
const PUMP_FEE_RECIPIENT: &str = "9rPYyANsfQZw3DnDmKE3YCQF5E8oD89UXoHn9JFEhJUz";
const PUMP_FEE_PROGRAM: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";
const PUMP_NEW_ACCT_9: &str = "HBM7cQB7M2fvuzYVtq2SLXJYnozW3j8N17KCqVuCa8p1";
const PUMP_GLOBAL_VOL_ACCUMULATOR: &str = "Hq2wp8uJ9jCPsYgNHex8RtqdvMPfVGoYwjvF1ATiwn2Y";
const PUMP_NEW_ACCT_16: &str = "7ucEb33Dg8REbDUuF16eDerV9wS9zqMUffrwBQpKoiED";
const PUMP_NEW_ACCT_17: &str = "9M4giFFMxmFGXtc3feFzRai56WbBqehoSeRE5GK7gf7F";

// 8 buyback_fee_recipient vault accounts from Global.buyback_fee_recipients[0..8]
// (offset 741 in the Global account data, derived from the official pump.json IDL).
// These are the 8 BuybackVault PDAs on the fee program. The buy's internal
// CPI to sell.rs checks these are non-zero; passing 0 system-program accounts
// triggers error 6062 "BuybackFeeRecipientMissing".
// Source: read live from Global account on Jun 18 2026.
const BUYBACK_FEE_RECIPIENTS: [&str; 8] = [
    "5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD",
    "9M4giFFMxmFGXtc3feFzRai56WbBqehoSeRE5GK7gf7",
    "GXPFM2caqTtQYC2cJ5yJRi9VDkpsYZXzYdwYpGnLmtDL",
    "3BpXnfJaUTiwXnJNe7Ej1rcbzqTTQUvLShZaWazebsVR",
    "5cjcW9wExnJJiqgLjq7DEG75Pm6JBgE1hNv4B2vHXUW6",
    "EHAAiTxcdDwQ3U4bU6YcMsQGaekdzLS3B5SmYo46kJtL",
    "5eHhjP8JaYkz83CWwvGU2uMUXefd3AazWGx4gpcuEEYD",
    "A7hAgCzFw14fejgCp387JUJRMNyz4j89JKnhtKU8piqW",
];

const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ASSOC_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

// buy / sell instruction discriminators (first 8 bytes of sha256("global:<method>"))
const BUY_DISCRIMINATOR: [u8; 8] = [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea];
// buy_v2 is the new unified instruction (27 accounts). Per the official
// pump-public-docs/docs/instructions/BUY.md (committed 2025), buy_v2 supports
// both SOL-paired and non-SOL-paired coins. We use buy_v2 going forward.
const BUY_V2_DISCRIMINATOR: [u8; 8] = [0xb8, 0x17, 0xee, 0x61, 0x67, 0xc5, 0xd3, 0x3d];
// sell_v2 discriminator per official pump-fun/pump-public-docs/idl/pump.json
// hash = sha256("global:sell_v2")[0..8] = [93, 246, 130, 60, 231, 233, 64, 178]
const SELL_V2_DISCRIMINATOR: [u8; 8] = [0x5d, 0xf6, 0x82, 0x3c, 0xe7, 0xe9, 0x40, 0xb2];
// initUserVolumeAccumulator: required BEFORE buy for wallets that have never
// traded (Aug 2025 volume-tracker update). Without this, buy fails with
// Anchor 3012 "expected account to be already initialized".
const INIT_USER_VOLUME_ACCUMULATOR_DISCRIMINATOR: [u8; 8] =
    [0x5e, 0x06, 0xca, 0x73, 0xff, 0x60, 0xe8, 0xb7];

const SOL_DECIMALS: u64 = 1_000_000_000;
const TOKEN_DECIMALS: u64 = 1_000_000;

// Slippage cap on max_sol_cost. Without curve state we use u64::MAX for amount,
// so slippage here = how much extra SOL we're willing to pay vs the limit.
const SLIPPAGE_BPS: u16 = 5000;

// ═══════════════════════════════════════
// Bonding Curve State (RPC-direct, no frontend API)
// ═══════════════════════════════════════

/// On-chain layout of pump.fun BondingCurve (after 8-byte Anchor discriminator):
///   virtual_token_reserves: u64 (8 bytes)
///   virtual_sol_reserves:  u64 (8 bytes)
///   real_token_reserves:   u64 (8 bytes)
///   real_sol_reserves:     u64 (8 bytes)
///   token_total_supply:    u64 (8 bytes)
///   complete:              bool (1 byte)
///   creator:               Pubkey (32 bytes)  // added Dec 2024
/// Total: 73 bytes of state (81 with discriminator).
/// BondingCurve accounts must be extended to 150 bytes via extendAccount
/// if dataLen < 150 (do this before buy if needed).
const BONDING_CURVE_DATA_SIZE: usize = 49;
const BONDING_CURVE_DATA_SIZE_NEW: usize = 81;

// ═══════════════════════════════════════
// Buy on Bonding Curve
// ═══════════════════════════════════════

pub async fn buy_on_bonding_curve(
    config: &AppConfig,
    private_key: &[u8],
    token: &TokenData,
    amount_sol: f64,
    client: &Client,
    blockhash_cache: &Arc<BlockhashCache>,
) -> Result<String> {
    let keypair = Keypair::from_bytes(private_key)
        .map_err(|e| anyhow!("Invalid keypair: {}", e))?;

    info!(
        "Bonding curve BUY: {} | {:.4} SOL | mint: {}",
        token.symbol, amount_sol, token.mint.as_str()
    );

    let sol_lamports = (amount_sol * SOL_DECIMALS as f64) as u64;
    // Fetch real bonding curve state — needed for accurate token_program,
    // bonding_curve address, and expected_tokens (u64::MAX overflows the
    // curve math and produces on-chain error 3007).
    let curve = fetch_bonding_curve(client, &config.solana.rpc_url, &token.mint).await?;
    // Cache it for a quick sell later (saves ~300ms on the sell path).
    curve_cache_put(&token.mint, curve.clone());
    if curve.complete {
        return Err(BuyError::Graduated { symbol: token.symbol.clone() }.into());
    }
    // tokens_out ≈ (sol_in * virtual_token_reserves) / virtual_sol_reserves
    // Apply slippage to set a realistic minimum-acceptable amount.
    let expected_tokens = ((sol_lamports as u128) * (curve.virtual_token_reserves as u128)
        / (curve.virtual_sol_reserves as u128).max(1)) as u64;
    let min_tokens = expected_tokens.saturating_mul(10000 - SLIPPAGE_BPS as u64) / 10000;
    let max_sol_cost = sol_lamports + (sol_lamports * SLIPPAGE_BPS as u64 / 10000);

    info!(
        "Curve: vSOL={} vTokens={} | min_tokens={} ({:.0} estimated) | max_sol_cost={} lamports ({}bps slippage)",
        curve.virtual_sol_reserves, curve.virtual_token_reserves,
        min_tokens, expected_tokens, max_sol_cost, SLIPPAGE_BPS
    );

    let mint_pubkey = Pubkey::from_str(&token.mint)?;
    let bonding_curve_pubkey = Pubkey::from_str(&curve.bonding_curve)?;
    let assoc_bonding_curve_pubkey = Pubkey::from_str(&curve.associated_bonding_curve)?;
    let token_prog_pubkey = Pubkey::from_str(&curve.token_program)?;
    let creator_pubkey = Pubkey::from_str(&curve.creator)?;
    let user_ata = get_associated_token_address(
        &keypair.pubkey(), &mint_pubkey, &token_prog_pubkey,
    );

    let mut instructions = Vec::new();
    instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
        config.trading.priority_fee.compute_unit_price,
    ));
    instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
        config.trading.priority_fee.compute_unit_limit as u32,
    ));
    // Aug 2025 volume-tracker update: pre-create user_volume_accumulator for
    // new wallets. If we skip this, the buy fails with Anchor 3012.
    // Cost: ~0.00089 SOL rent. Idempotent? No — fails if account already
    // exists. For first-time wallets this is required; for returning wallets
    // the user_volume_accumulator already exists, and including this ix would
    // fail. We include it always for safety; if the wallet is known to have
    // traded before, this can be skipped. (Future: track in DB.)
    instructions.push(build_init_user_volume_accumulator(
        &keypair.pubkey(),
        &keypair.pubkey(),
    ));
    instructions.push(create_idempotent_ata_instruction(
        &keypair.pubkey(), &mint_pubkey, &token_prog_pubkey,
    ));
    instructions.push(build_buy_v2_instruction(
        &keypair.pubkey(),
        &mint_pubkey,
        &bonding_curve_pubkey,
        &assoc_bonding_curve_pubkey,
        &user_ata,
        min_tokens,
        max_sol_cost,
        &token_prog_pubkey,
        &creator_pubkey,
    ));

    let blockhash = blockhash_cache.get().await?;
    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&keypair.pubkey()),
        &[&keypair],
        blockhash,
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(bincode::serialize(&tx)?);

    // Skip simulation if configured (default true — saves ~210ms per buy)
    let tx_hash = if config.trading.skip_simulation {
        warn!("skip_simulation=true: sending tx WITHOUT preflight check (config.trading.skip_simulation)");
        send_transaction_rpc(client, &config.solana.rpc_url, &encoded).await?
    } else {
        simulate_and_send(client, &config.solana.rpc_url, &encoded).await?
    };

    info!("BUY confirmed: {} | {:.4} SOL | tx: {}", token.symbol, amount_sol, tx_hash);
    Ok(tx_hash)
}

// ═══════════════════════════════════════
// Sell on Bonding Curve
// ═══════════════════════════════════════

pub async fn sell_on_bonding_curve(
    config: &AppConfig,
    private_key: &[u8],
    token: &TokenData,
    token_amount: f64,
    client: &Client,
    blockhash_cache: &Arc<BlockhashCache>,
) -> Result<String> {
    let keypair = Keypair::from_bytes(private_key)
        .map_err(|e| anyhow!("Invalid keypair: {}", e))?;

    let raw_amount = (token_amount * TOKEN_DECIMALS as f64) as u64;
    if raw_amount == 0 {
        return Err(anyhow!("Sell amount too small (0 tokens)"));
    }

    info!("Bonding curve SELL: {} | {:.4} tokens | raw: {}", token.symbol, token_amount, raw_amount);

    // Check cache first — saves ~300ms when buy recently populated it.
    let curve = if let Some(c) = curve_cache_get(&token.mint) {
        info!("Curve cache HIT for {}", token.mint);
        c
    } else {
        info!("Curve cache MISS for {}, fetching...", token.mint);
        let c = fetch_bonding_curve(client, &config.solana.rpc_url, &token.mint).await?;
        curve_cache_put(&token.mint, c.clone());
        c
    };
    if curve.complete {
        return Err(SellError::Graduated { symbol: token.symbol.clone() }.into());
    }

    let expected_sol = calculate_sell_amount(
        raw_amount,
        curve.virtual_sol_reserves,
        curve.virtual_token_reserves,
    );
    let min_sol = expected_sol * (10000 - SLIPPAGE_BPS as u64) / 10000;

    info!(
        "Expected: {:.6} SOL (min: {:.6} | slippage: {}bps)",
        expected_sol as f64 / SOL_DECIMALS as f64,
        min_sol as f64 / SOL_DECIMALS as f64,
        SLIPPAGE_BPS
    );

    let mint_pubkey = Pubkey::from_str(&token.mint)?;
    let bonding_curve_pubkey = Pubkey::from_str(&curve.bonding_curve)?;
    let assoc_bonding_curve_pubkey = Pubkey::from_str(&curve.associated_bonding_curve)?;
    let token_prog_pubkey = Pubkey::from_str(&curve.token_program)?;
    let creator_pubkey = Pubkey::from_str(&curve.creator)?;
    let user_ata = get_associated_token_address(
        &keypair.pubkey(), &mint_pubkey, &token_prog_pubkey,
    );

    let mut instructions = Vec::new();
    instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
        config.trading.priority_fee.compute_unit_price,
    ));
    instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
        config.trading.priority_fee.compute_unit_limit as u32,
    ));
    instructions.push(build_sell_v2_instruction(
        &keypair.pubkey(),
        &mint_pubkey,
        &bonding_curve_pubkey,
        &assoc_bonding_curve_pubkey,
        &user_ata,
        raw_amount,
        min_sol,
        &token_prog_pubkey,
        &creator_pubkey,
    ));

    let blockhash = blockhash_cache.get().await?;
    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&keypair.pubkey()),
        &[&keypair],
        blockhash,
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(bincode::serialize(&tx)?);

    let tx_hash = if config.trading.skip_simulation {
        send_transaction_rpc(client, &config.solana.rpc_url, &encoded).await?
    } else {
        simulate_and_send(client, &config.solana.rpc_url, &encoded).await?
    };

    info!("SELL confirmed: {} | tx: {}", token.symbol, tx_hash);
    Ok(tx_hash)
}

// ═══════════════════════════════════════
// Simulate-then-Send Wrapper
// ═══════════════════════════════════════

/// Simulate a transaction. Returns Ok(units_consumed) on success,
/// Err if simulation indicates the tx would fail. Free on most RPCs.
async fn simulate_transaction_rpc(
    client: &Client,
    rpc_url: &str,
    encoded_tx: &str,
) -> Result<u64> {
    let resp = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "simulateTransaction",
            "params": [
                encoded_tx,
                {
                    "encoding": "base64",
                    "replaceRecentBlockhash": true,
                    "sigVerify": false,
                    "commitment": "processed"
                }
            ]
        }))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    if let Some(err) = resp.get("error") {
        return Err(anyhow!("Simulation RPC error: {}", err));
    }

    let value = resp
        .get("result")
        .and_then(|r| r.get("value"))
        .ok_or_else(|| anyhow!("No value in simulation response: {}", resp))?;

    if let Some(err) = value.get("err") {
        if !err.is_null() {
            return Err(anyhow!("Simulation failed (tx would revert on-chain): {}", err));
        }
    }

    let units = value
        .get("unitsConsumed")
        .and_then(|u| u.as_u64())
        .unwrap_or(0);

    Ok(units)
}

/// Simulate first, then send. Saves gas by skipping transactions that
/// would revert on-chain (slippage, insufficient funds, program errors).
async fn simulate_and_send(
    client: &Client,
    rpc_url: &str,
    encoded_tx: &str,
) -> Result<String> {
    let units = simulate_transaction_rpc(client, rpc_url, encoded_tx).await?;
    info!("Simulation OK: {} CU consumed", units);

    send_transaction_rpc(client, rpc_url, encoded_tx).await
}

/// Submit a transaction and wait for it to be processed. The function logs
/// two timings:
///   - "TX submitted in Nms" — when the RPC acknowledged receipt (HTTP 200
///     with a signature). This is the "snipe moment" — the tx is in the
///     leader's queue.
///   - (caller's "BUY/SELL confirmed" log) — after the tx is processed and
///     visible on-chain. Includes the ~400-800ms "processed" wait.
///
/// For the snipe bot, the submission time is what matters; the total
/// (submission + confirmation) is what the test_buy/test_sell logs show.
async fn send_transaction_rpc(client: &Client, rpc_url: &str, encoded_tx: &str) -> Result<String> {
    let start = Instant::now();
    let resp = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": [encoded_tx, {
                "encoding": "base64",
                "skipPreflight": true,  // already simulated
                "maxRetries": 3,
                "preflightCommitment": "processed"
            }]
        }))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    if let Some(err) = resp.get("error") {
        return Err(anyhow!("RPC sendTransaction error: {}", err));
    }

    let sig = resp
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("No transaction signature returned"))?
        .to_string();

    let submission_ms = start.elapsed().as_millis();
    info!("TX submitted in {}ms: {}", submission_ms, sig);

    confirm_transaction(client, rpc_url, &sig).await?;
    Ok(sig)
}

// ═══════════════════════════════════════
// Bonding Curve State (RPC-direct, no frontend API)
// ═══════════════════════════════════════

/// On-chain layout of pump.fun BondingCurve (after 8-byte Anchor discriminator):
///   virtual_token_reserves: u64 (offset 8)
///   virtual_sol_reserves:   u64 (offset 16)
///   real_token_reserves:    u64 (offset 24)
///   real_sol_reserves:      u64 (offset 32)
///   token_total_supply:     u64 (offset 40)
///   complete:               bool (offset 48)
///   creator:                Pubkey (offset 49, 32 bytes)
///   padding to 150 bytes
/// Source: pump-public-docs/docs/PUMP_PROGRAM.md
#[derive(Clone)]
struct BondingCurveState {
    bonding_curve: String,
    associated_bonding_curve: String,
    virtual_sol_reserves: u64,
    virtual_token_reserves: u64,
    complete: bool,
    token_program: String,
    /// New field added in Dec 2024 pump.fun upgrade. PDA-derived:
    /// `creator_vault = find_program_address([b"creator-vault", creator], PUMP_FUN_PROGRAM)`.
    /// Required at instruction account index 9 since the creator-fee update.
    /// Zeroed out (`11111...1111`) for pre-Dec-2024 coins that haven't been
    /// backfilled by the pump.fun backend.
    creator: String,
}

// ═══════════════════════════════════════
// Bonding Curve Cache (process-local, 2s TTL)
// ═══════════════════════════════════════
//
// Avoids a ~300ms `fetch_bonding_curve` RPC round-trip on the sell path
// when the curve was recently fetched (e.g. by a background pre-fetch
// kicked off at buy time). Cache lives for the process lifetime; resets
// on restart. Stale (>2s) entries are re-fetched.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
// (Instant imported at top of file)

struct CachedCurve {
    state: BondingCurveState,
    fetched_at: Instant,
}

static CURVE_CACHE: OnceLock<Mutex<HashMap<String, CachedCurve>>> = OnceLock::new();
// No TTL — cache lives until `curve_cache_remove(mint)` is called explicitly
// (typically after the position is fully sold). The cache keeps the curve
// state that was current at the time of the buy, plus the `complete` flag
// so graduation can still be detected without a refetch.

fn curve_cache_get(mint: &str) -> Option<BondingCurveState> {
    let map_lock = CURVE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let map = map_lock.lock().ok()?;
    map.get(mint).map(|c| c.state.clone())
}

fn curve_cache_put(mint: &str, state: BondingCurveState) {
    let map_lock = CURVE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut map) = map_lock.lock() {
        info!("Curve cache: PUT {}", &mint[..12]);
        map.insert(mint.to_string(), CachedCurve { state, fetched_at: Instant::now() });
    }
}

/// Explicitly evict a mint's cache entry — call after the position is fully
/// closed (last sell confirmed) to free memory.
pub fn curve_cache_remove(mint: &str) {
    let map_lock = CURVE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut map) = map_lock.lock() {
        if map.remove(mint).is_some() {
            info!("Curve cache: REMOVED {}", &mint[..12]);
        }
    }
}

/// Fetch bonding curve state via direct Solana RPC calls.
/// Returns (state, token_program). token_program is the mint's owner field
/// (TokenkegQ... for legacy, TokenzQd... for Token-2022). Faster than the
/// pump.fun frontend API — two direct RPC calls instead of one CDN roundtrip.
async fn fetch_bonding_curve(
    client: &Client,
    rpc_url: &str,
    mint: &str,
) -> Result<BondingCurveState> {
    let mint_pubkey = Pubkey::from_str(mint)?;
    let pump_program_pubkey = Pubkey::from_str(PUMP_FUN_PROGRAM)?;

    // Derive the bonding curve PDA
    let (bonding_curve_pubkey, _) = Pubkey::find_program_address(
        &[b"bonding-curve", mint_pubkey.as_ref()],
        &pump_program_pubkey,
    );

    // Fetch mint + bonding curve in parallel (both are independent reads)
    let (mint_resp, curve_resp) = tokio::try_join!(
        client.post(rpc_url).json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "getAccountInfo",
            "params": [mint, {"encoding": "jsonParsed"}]
        })).send(),
        client.post(rpc_url).json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "getAccountInfo",
            "params": [bonding_curve_pubkey.to_string(), {"encoding": "base64"}]
        })).send(),
    )?;

    let mint_json: serde_json::Value = mint_resp.json().await?;
    let curve_json: serde_json::Value = curve_resp.json().await?;

    // Validate the mint account exists (Anchor 3007 = AccountOwnedByWrongProgram
    // fires if we proceed with a phantom mint).
    let mint_value = mint_json
        .get("result").and_then(|r| r.get("value"))
        .ok_or_else(|| anyhow!("Mint {} does not exist on-chain", mint))?;

    // Extract token_program from mint account owner
    let token_program = mint_value
        .get("owner")
        .and_then(|o| o.as_str())
        .ok_or_else(|| anyhow!("Failed to get mint owner for {}", mint))?
        .to_string();

    // Parse bonding curve data
    let curve_value = curve_json
        .get("result").and_then(|r| r.get("value"))
        .ok_or_else(|| anyhow!(
            "Bonding curve account does not exist for {} — token is not on pump.fun (or has no bonding curve yet)",
            mint
        ))?;

    // Validate owner is pump.fun program (Anchor 3007 = AccountOwnedByWrongProgram)
    let curve_owner = curve_value
        .get("owner")
        .and_then(|o| o.as_str())
        .ok_or_else(|| anyhow!("Bonding curve has no owner field"))?;
    if curve_owner != PUMP_FUN_PROGRAM {
        return Err(anyhow!(
            "Bonding curve owned by {} (expected {}). This token is not on pump.fun or the mint address is wrong.",
            curve_owner, PUMP_FUN_PROGRAM
        ));
    }

    let data_b64 = curve_value
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .and_then(|s| s.as_str())
        .ok_or_else(|| anyhow!("No data in bonding curve account for {} (token may have graduated)", mint))?;

    let data = base64::engine::general_purpose::STANDARD.decode(data_b64)
        .map_err(|e| anyhow!("Failed to decode bonding curve data: {}", e))?;

    if data.len() < BONDING_CURVE_DATA_SIZE {
        return Err(anyhow!(
            "Bonding curve data too short: {} bytes (expected {})",
            data.len(), BONDING_CURVE_DATA_SIZE
        ));
    }

    // Skip 8-byte Anchor discriminator
    let virtual_token_reserves = u64::from_le_bytes(data[8..16].try_into()?);
    let virtual_sol_reserves  = u64::from_le_bytes(data[16..24].try_into()?);
    let complete               = data[48] != 0;

    // New layout (Dec 2024+): `creator: Pubkey` at offset 49..80 (after `complete: bool`).
    // For coins created on the legacy layout (<49 bytes data) this field is absent.
    // Default to the System Program ID — those legacy coins use a default creator_vault
    // PDA derived from that zeroed pubkey, which is what pump.fun's backend backfills
    // anyway. We never *spend* creator_vault, so this is safe.
    let creator = if data.len() >= BONDING_CURVE_DATA_SIZE_NEW {
        // Try to parse as a base58 pubkey from the 32 bytes
        let mut creator_bytes = [0u8; 32];
        creator_bytes.copy_from_slice(&data[49..81]);
        // Only treat as valid if not all zeros
        if creator_bytes == [0u8; 32] {
            Pubkey::default().to_string()
        } else {
            Pubkey::new_from_array(creator_bytes).to_string()
        }
    } else {
        // Legacy layout — backend will backfill
        warn!(
            "Bonding curve for {} is on legacy 49-byte layout (pre-Dec-2024). \
             Creator field missing. Buy may fail with 3007 if not backfilled.",
            mint
        );
        Pubkey::default().to_string()
    };

    let assoc_bonding_curve_pubkey = get_associated_token_address(
        &bonding_curve_pubkey, &mint_pubkey, &Pubkey::from_str(&token_program)?,
    );

    Ok(BondingCurveState {
        bonding_curve: bonding_curve_pubkey.to_string(),
        associated_bonding_curve: assoc_bonding_curve_pubkey.to_string(),
        virtual_sol_reserves,
        virtual_token_reserves,
        complete,
        token_program,
        creator,
    })
}

// ═══════════════════════════════════════
// Bonding Curve Math
// ═══════════════════════════════════════

/// sol_out = (tokens_in * virtual_sol_reserves) / (virtual_token_reserves + tokens_in)
fn calculate_sell_amount(tokens_in: u64, virtual_sol: u64, virtual_token: u64) -> u64 {
    if virtual_token == 0 {
        return 0;
    }
    let numerator = (tokens_in as u128) * (virtual_sol as u128);
    let denominator = (virtual_token as u128) + (tokens_in as u128);
    (numerator / denominator) as u64
}

// ═══════════════════════════════════════
// Instruction Builders
// ═══════════════════════════════════════

// All 8 normal fee recipients (from Global + FEE_RECIPIENTS doc).
const NORMAL_FEE_RECIPIENTS: [&str; 8] = [
    "62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV", // [0] = Global.fee_recipient
    "7VtfL8fvgNfhz17qKRMjzQEXgbdpnHHHQRh54R9jP2RJ",
    "7hTckgnGnLQR6sdH7YkqFTAA7VwTfYFaZ6EhEsU3saCX",
    "9rPYyANsfQZw3DnDmKE3YCQF5E8oD89UXoHn9JFEhJUz",
    "AVmoTthdrX6tKt4nDjco2D775W2YK3sDhxPcMmzUAmTY",
    "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM",
    "FWsW1xNtWscwNmKv6wVsU1iTzRN6wmmk3MjxRP5tT7hz",
    "G5UZAVbAf46s7cKWoyKu8kYTip9DGTpbLZ2qa9Aq69dP",
];

// WSOL (native SOL wrapped) for SOL-paired coins per BUY.md
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
// Legacy SPL Token program (for WSOL/quote)
const LEGACY_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

fn build_buy_v2_instruction(
    user: &Pubkey,
    mint: &Pubkey,
    bonding_curve: &Pubkey,
    associated_bonding_curve: &Pubkey,
    associated_user: &Pubkey,
    amount: u64,
    max_sol_cost: u64,
    token_prog: &Pubkey,
    creator: &Pubkey,
) -> Instruction {
    let pump_program = Pubkey::from_str(PUMP_FUN_PROGRAM).unwrap();
    let global = Pubkey::from_str(PUMP_GLOBAL).unwrap();
    let event_authority = Pubkey::from_str(PUMP_EVENT_AUTHORITY).unwrap();
    let global_vol = Pubkey::from_str(PUMP_GLOBAL_VOL_ACCUMULATOR).unwrap();
    let fee_program = Pubkey::from_str(PUMP_FEE_PROGRAM).unwrap();
    let wsol = Pubkey::from_str(WSOL_MINT).unwrap();
    let legacy_token_prog = Pubkey::from_str(LEGACY_TOKEN_PROGRAM).unwrap();
    let ata_prog = Pubkey::from_str(ASSOC_TOKEN_PROGRAM).unwrap();

    // PDAs
    let (creator_vault, _) = Pubkey::find_program_address(
        &[b"creator-vault", creator.as_ref()],
        &pump_program,
    );
    let (user_vol_accumulator, _) = Pubkey::find_program_address(
        &[b"user_volume_accumulator", user.as_ref()],
        &pump_program,
    );
    let (fee_config, _) = Pubkey::find_program_address(
        &[b"fee_config", pump_program.as_ref()],
        &fee_program,
    );
    let (sharing_config, _) = Pubkey::find_program_address(
        &[b"sharing-config", mint.as_ref()],
        &fee_program,
    );

    // Fee recipient: use Global.fee_recipient (index 0 of NORMAL_FEE_RECIPIENTS)
    let fee_recipient = Pubkey::from_str(NORMAL_FEE_RECIPIENTS[0])
        .expect("hard-coded fee recipient");
    // Buyback fee recipient: use index 5 (matches the working buy 3dkcwM5U...)
    let buyback_fee_recipient = Pubkey::from_str(BUYBACK_FEE_RECIPIENTS[5])
        .expect("hard-coded buyback fee recipient");

    // Associated token accounts (quote = WSOL, quote_token_program = legacy SPL Token)
    let (associated_quote_fee_recipient, _) = Pubkey::find_program_address(
        &[fee_recipient.as_ref(), legacy_token_prog.as_ref(), wsol.as_ref()],
        &ata_prog,
    );
    let (associated_quote_buyback_fee_recipient, _) = Pubkey::find_program_address(
        &[buyback_fee_recipient.as_ref(), legacy_token_prog.as_ref(), wsol.as_ref()],
        &ata_prog,
    );
    let (associated_quote_bonding_curve, _) = Pubkey::find_program_address(
        &[bonding_curve.as_ref(), legacy_token_prog.as_ref(), wsol.as_ref()],
        &ata_prog,
    );
    let (associated_quote_user, _) = Pubkey::find_program_address(
        &[user.as_ref(), legacy_token_prog.as_ref(), wsol.as_ref()],
        &ata_prog,
    );
    let (associated_creator_vault, _) = Pubkey::find_program_address(
        &[creator_vault.as_ref(), legacy_token_prog.as_ref(), wsol.as_ref()],
        &ata_prog,
    );
    let (associated_user_volume_accumulator, _) = Pubkey::find_program_address(
        &[user_vol_accumulator.as_ref(), legacy_token_prog.as_ref(), wsol.as_ref()],
        &ata_prog,
    );

    // buy_v2 data: amount (u64) + max_sol_cost (u64) = 16 bytes (no track_volume)
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&BUY_V2_DISCRIMINATOR);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&max_sol_cost.to_le_bytes());

    // Account order per official pump.json IDL for `buy_v2` (27 accounts).
    let accounts = vec![
        AccountMeta::new_readonly(global, false),                          // [0]  global
        AccountMeta::new_readonly(*mint, false),                           // [1]  base_mint
        AccountMeta::new_readonly(wsol, false),                            // [2]  quote_mint
        AccountMeta::new_readonly(*token_prog, false),                     // [3]  base_token_program
        AccountMeta::new_readonly(legacy_token_prog, false),               // [4]  quote_token_program
        AccountMeta::new_readonly(ata_prog, false),                        // [5]  associated_token_program
        AccountMeta::new(fee_recipient, false),                            // [6]  fee_recipient (writable)
        AccountMeta::new(associated_quote_fee_recipient, false),           // [7]  associated_quote_fee_recipient (writable)
        AccountMeta::new(buyback_fee_recipient, false),                    // [8]  buyback_fee_recipient (writable)
        AccountMeta::new(associated_quote_buyback_fee_recipient, false),   // [9]  associated_quote_buyback_fee_recipient (writable)
        AccountMeta::new(*bonding_curve, false),                           // [10] bonding_curve (writable)
        AccountMeta::new(*associated_bonding_curve, false),                // [11] associated_base_bonding_curve (writable)
        AccountMeta::new(associated_quote_bonding_curve, false),           // [12] associated_quote_bonding_curve (writable)
        AccountMeta::new(*user, true),                                     // [13] user (signer, writable)
        AccountMeta::new(*associated_user, false),                         // [14] associated_base_user (writable)
        AccountMeta::new(associated_quote_user, false),                    // [15] associated_quote_user (writable)
        AccountMeta::new(creator_vault, false),                            // [16] creator_vault (writable)
        AccountMeta::new(associated_creator_vault, false),                 // [17] associated_creator_vault (writable)
        AccountMeta::new_readonly(sharing_config, false),                  // [18] sharing_config
        AccountMeta::new_readonly(global_vol, false),                      // [19] global_volume_accumulator
        AccountMeta::new(user_vol_accumulator, false),                     // [20] user_volume_accumulator (writable)
        AccountMeta::new(associated_user_volume_accumulator, false),       // [21] associated_user_volume_accumulator (writable)
        AccountMeta::new_readonly(fee_config, false),                      // [22] fee_config
        AccountMeta::new_readonly(fee_program, false),                     // [23] fee_program
        AccountMeta::new_readonly(system_program::ID, false),              // [24] system_program
        AccountMeta::new_readonly(event_authority, false),                 // [25] event_authority
        AccountMeta::new_readonly(pump_program, false),                    // [26] program
    ];

    Instruction { program_id: pump_program, accounts, data }
}

fn build_sell_v2_instruction(
    user: &Pubkey,
    mint: &Pubkey,
    bonding_curve: &Pubkey,
    associated_bonding_curve: &Pubkey,
    associated_user: &Pubkey,
    amount: u64,
    min_sol_output: u64,
    token_prog: &Pubkey,
    creator: &Pubkey,
) -> Instruction {
    let pump_program = Pubkey::from_str(PUMP_FUN_PROGRAM).unwrap();
    let global = Pubkey::from_str(PUMP_GLOBAL).unwrap();
    let event_authority = Pubkey::from_str(PUMP_EVENT_AUTHORITY).unwrap();
    let global_vol = Pubkey::from_str(PUMP_GLOBAL_VOL_ACCUMULATOR).unwrap();
    let fee_program = Pubkey::from_str(PUMP_FEE_PROGRAM).unwrap();
    let wsol = Pubkey::from_str(WSOL_MINT).unwrap();
    let legacy_token_prog = Pubkey::from_str(LEGACY_TOKEN_PROGRAM).unwrap();
    let ata_prog = Pubkey::from_str(ASSOC_TOKEN_PROGRAM).unwrap();

    // PDAs
    let (creator_vault, _) = Pubkey::find_program_address(
        &[b"creator-vault", creator.as_ref()],
        &pump_program,
    );
    let (user_vol_accumulator, _) = Pubkey::find_program_address(
        &[b"user_volume_accumulator", user.as_ref()],
        &pump_program,
    );
    let (fee_config, _) = Pubkey::find_program_address(
        &[b"fee_config", pump_program.as_ref()],
        &fee_program,
    );
    let (sharing_config, _) = Pubkey::find_program_address(
        &[b"sharing-config", mint.as_ref()],
        &fee_program,
    );

    // Fee recipient: same as buy (Global.fee_recipient index 0)
    let fee_recipient = Pubkey::from_str(NORMAL_FEE_RECIPIENTS[0])
        .expect("hard-coded fee recipient");
    // Buyback fee recipient: index 5 (matches the working buy/sell 3dkcwM5U...)
    let buyback_fee_recipient = Pubkey::from_str(BUYBACK_FEE_RECIPIENTS[5])
        .expect("hard-coded buyback fee recipient");

    // Associated token accounts (quote = WSOL, quote_token_program = legacy SPL Token)
    let (associated_quote_fee_recipient, _) = Pubkey::find_program_address(
        &[fee_recipient.as_ref(), legacy_token_prog.as_ref(), wsol.as_ref()],
        &ata_prog,
    );
    let (associated_quote_buyback_fee_recipient, _) = Pubkey::find_program_address(
        &[buyback_fee_recipient.as_ref(), legacy_token_prog.as_ref(), wsol.as_ref()],
        &ata_prog,
    );
    let (associated_quote_bonding_curve, _) = Pubkey::find_program_address(
        &[bonding_curve.as_ref(), legacy_token_prog.as_ref(), wsol.as_ref()],
        &ata_prog,
    );
    let (associated_creator_vault, _) = Pubkey::find_program_address(
        &[creator_vault.as_ref(), legacy_token_prog.as_ref(), wsol.as_ref()],
        &ata_prog,
    );
    let (associated_user_volume_accumulator, _) = Pubkey::find_program_address(
        &[user_vol_accumulator.as_ref(), legacy_token_prog.as_ref(), wsol.as_ref()],
        &ata_prog,
    );
    let (associated_quote_user, _) = Pubkey::find_program_address(
        &[user.as_ref(), legacy_token_prog.as_ref(), wsol.as_ref()],
        &ata_prog,
    );

    // sell_v2 data: amount (u64) + min_sol_output (u64) = 16 bytes (no track_volume)
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&SELL_V2_DISCRIMINATOR);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&min_sol_output.to_le_bytes());

    // Account order per official pump-public-docs/docs/instructions/SELL.md
    // (26 accounts, 1-indexed in the doc, 0-indexed here).
    // Note: sell_v2 differs from buy_v2 — it OMITS global_volume_accumulator
    // (read-only, not needed for sell), but ADDS user + associated_*_user
    // (needed because the user is the signer transferring tokens).
    let accounts = vec![
        AccountMeta::new_readonly(global, false),                          // [0]  global
        AccountMeta::new_readonly(*mint, false),                           // [1]  base_mint
        AccountMeta::new_readonly(wsol, false),                            // [2]  quote_mint
        AccountMeta::new_readonly(*token_prog, false),                     // [3]  base_token_program
        AccountMeta::new_readonly(legacy_token_prog, false),               // [4]  quote_token_program
        AccountMeta::new_readonly(ata_prog, false),                        // [5]  associated_token_program
        AccountMeta::new(fee_recipient, false),                            // [6]  fee_recipient (writable)
        AccountMeta::new(associated_quote_fee_recipient, false),           // [7]  associated_quote_fee_recipient (writable)
        AccountMeta::new(buyback_fee_recipient, false),                    // [8]  buyback_fee_recipient (writable)
        AccountMeta::new(associated_quote_buyback_fee_recipient, false),   // [9]  associated_quote_buyback_fee_recipient (writable)
        AccountMeta::new(*bonding_curve, false),                           // [10] bonding_curve (writable)
        AccountMeta::new(*associated_bonding_curve, false),                // [11] associated_base_bonding_curve (writable)
        AccountMeta::new(associated_quote_bonding_curve, false),           // [12] associated_quote_bonding_curve (writable)
        AccountMeta::new(*user, true),                                     // [13] user (signer, writable)
        AccountMeta::new(*associated_user, false),                         // [14] associated_base_user (writable)
        AccountMeta::new(associated_quote_user, false),                    // [15] associated_quote_user (writable)
        AccountMeta::new(creator_vault, false),                            // [16] creator_vault (writable)
        AccountMeta::new(associated_creator_vault, false),                 // [17] associated_creator_vault (writable)
        AccountMeta::new_readonly(sharing_config, false),                  // [18] sharing_config
        AccountMeta::new(user_vol_accumulator, false),                     // [19] user_volume_accumulator (writable)
        AccountMeta::new(associated_user_volume_accumulator, false),       // [20] associated_user_volume_accumulator (writable)
        AccountMeta::new_readonly(fee_config, false),                      // [21] fee_config
        AccountMeta::new_readonly(fee_program, false),                     // [22] fee_program
        AccountMeta::new_readonly(system_program::ID, false),              // [23] system_program
        AccountMeta::new_readonly(event_authority, false),                 // [24] event_authority
        AccountMeta::new_readonly(pump_program, false),                    // [25] program
    ];

    Instruction { program_id: pump_program, accounts, data }
}

fn create_idempotent_ata_instruction(payer: &Pubkey, mint: &Pubkey, token_prog: &Pubkey) -> Instruction {
    let ata = get_associated_token_address(payer, mint, token_prog);
    let assoc_prog = Pubkey::from_str(ASSOC_TOKEN_PROGRAM).unwrap();

    Instruction {
        program_id: assoc_prog,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata, false),
            AccountMeta::new_readonly(*payer, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(*token_prog, false),
        ],
        data: vec![1], // create_idempotent discriminator
    }
}

/// Build the `initUserVolumeAccumulator` instruction. Required BEFORE buy for
/// wallets that have never traded (Aug 2025 volume-tracker update). Without
/// this, the buy fails with Anchor 3012 "expected account to be already
/// initialized" because the buy expects user_volume_accumulator to exist.
/// PDA: find_program_address([b"user_volume_accumulator", user], pump_program).
///
/// Account order per official IDL (pump-fun/pump-public-docs):
///   0. payer (writable, signer)              — pays rent
///   1. user (readonly)                       — the user owning the accumulator
///   2. userVolumeAccumulator (writable)      — PDA: [b"user_volume_accumulator", user]
///   3. systemProgram (readonly)              — System Program
///   4. eventAuthority (readonly)             — PDA: [b"__event_authority"], bump=254
///   5. program (readonly)                    — pump.fun program
fn build_init_user_volume_accumulator(payer: &Pubkey, user: &Pubkey) -> Instruction {
    let pump_program = Pubkey::from_str(PUMP_FUN_PROGRAM).unwrap();
    let (user_volume_accumulator, _) = Pubkey::find_program_address(
        &[b"user_volume_accumulator", user.as_ref()],
        &pump_program,
    );
    let event_authority = Pubkey::from_str(PUMP_EVENT_AUTHORITY).unwrap();
    Instruction {
        program_id: pump_program,
        accounts: vec![
            AccountMeta::new(*payer, true),                         // [0] payer
            AccountMeta::new_readonly(*user, false),                // [1] user
            AccountMeta::new(user_volume_accumulator, false),       // [2] userVolumeAccumulator
            AccountMeta::new_readonly(system_program::ID, false),  // [3] systemProgram
            AccountMeta::new_readonly(event_authority, false),      // [4] eventAuthority
            AccountMeta::new_readonly(pump_program, false),         // [5] program
        ],
        data: INIT_USER_VOLUME_ACCUMULATOR_DISCRIMINATOR.to_vec(),
    }
}

// ═══════════════════════════════════════
// Helpers
// ═══════════════════════════════════════

fn get_associated_token_address(wallet: &Pubkey, mint: &Pubkey, token_prog: &Pubkey) -> Pubkey {
    let assoc_prog = Pubkey::from_str(ASSOC_TOKEN_PROGRAM).unwrap();
    let seeds = &[wallet.as_ref(), token_prog.as_ref(), mint.as_ref()];
    Pubkey::find_program_address(seeds, &assoc_prog).0
}

async fn get_recent_blockhash(client: &Client, rpc_url: &str) -> Result<Hash> {
    let resp = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getLatestBlockhash",
            "params": [{"commitment": "finalized"}]
        }))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    let blockhash_str = resp
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.get("blockhash"))
        .and_then(|b| b.as_str())
        .ok_or_else(|| anyhow!("Failed to get blockhash: {}", resp))?;

    Ok(Hash::from_str(blockhash_str)?)
}

/// Wait for transaction confirmation (polls getSignatureStatuses).
async fn confirm_transaction(client: &Client, rpc_url: &str, sig: &str) -> Result<()> {
    let max_attempts = 30; // 30s timeout
    let poll_interval = tokio::time::Duration::from_secs(1);

    for attempt in 1..=max_attempts {
        tokio::time::sleep(poll_interval).await;

        let resp = client
            .post(rpc_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getSignatureStatuses",
                "params": [[sig], {"searchTransactionHistory": true}]
            }))
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        let status = resp
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first());

        match status {
            Some(s) if s.is_null() => {
                if attempt % 5 == 0 {
                    warn!("Tx {} not confirmed yet (attempt {}/{})", &sig[..12], attempt, max_attempts);
                }
                continue;
            }
            Some(s) => {
                let err = s.get("err");
                if err.is_some() && !err.unwrap().is_null() {
                    return Err(anyhow!("Transaction failed on-chain: {}", err.unwrap()));
                }

                let confirmation_status = s.get("confirmationStatus")
                    .and_then(|c| c.as_str())
                    .unwrap_or("unknown");

                match confirmation_status {
                    "confirmed" | "finalized" => return Ok(()),
                    _ => {
                        if attempt % 5 == 0 {
                            warn!("Tx {} status: {} (attempt {}/{})",
                                &sig[..12], confirmation_status, attempt, max_attempts);
                        }
                        continue;
                    }
                }
            }
            None => return Err(anyhow!("Unexpected response from getSignatureStatuses")),
        }
    }

    Err(anyhow!("Transaction confirmation timed out after {}s: {}", max_attempts, &sig[..12]))
}
