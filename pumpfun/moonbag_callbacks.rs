//! Telegram callback listener for moonbag sell buttons.
//!
//! The bot sends moonbag alerts with two inline buttons: "💸 Sell 50%" and
//! "💸 Sell 100%". When the user taps one, Telegram sends a `callback_query`
//! to the bot. This module polls `getUpdates` for those callbacks and
//! executes the requested sell.
//!
//! The callback_data format is `moonbag_sell:<mint>:<pct>`, where pct is
//! either 50 or 100. We use long polling with a 1-second timeout so we don't
//! hammer the Telegram API.

use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;
use tracing::{info, warn, error};

use crate::executor::TradeExecutor;
use crate::exit::moonbag::MoonbagTracker;
use crate::types::TokenData;
use crate::utils::alerts::{AlertManager, CallbackQueryUpdate};
use crate::utils::rpc::SolanaRpc;

/// Polls Telegram getUpdates for callback_query updates. On each
/// `moonbag_sell:<mint>:<pct>` callback, executes the requested sell.
pub async fn poll_moonbag_callbacks(
    alerts: Arc<AlertManager>,
    executor: Arc<TradeExecutor>,
    moonbag_tracker: Arc<TokioMutex<MoonbagTracker>>,
    _rpc: SolanaRpc,
) {
    let bot_token = alerts.bot_token().to_string();
    if bot_token.is_empty() {
        return;
    }
    let url = format!("https://api.telegram.org/bot{}/getUpdates", bot_token);
    let client = alerts.http_client().clone();
    let chat_id = alerts.chat_id().to_string();

    // Use an offset to only fetch new updates (Telegram returns them
    // oldest-first, so we track the last update_id we saw)
    let mut offset: i64 = 0;
    let mut backoff_secs: u64 = 1;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;

        // Long-poll: pass timeout=30 so Telegram holds the connection open
        // for up to 30s if there are no new updates. This avoids hammering.
        let body = serde_json::json!({
            "allowed_updates": ["callback_query"],
            "offset": offset,
            "timeout": 25,
        });

        let resp = match client.post(&url)
            .timeout(std::time::Duration::from_secs(35))
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!("Telegram getUpdates failed: {} — backing off {}s", e, backoff_secs);
                backoff_secs = (backoff_secs * 2).min(30);
                continue;
            }
        };

        if !resp.status().is_success() {
            warn!("Telegram getUpdates returned non-200: {} — backing off", resp.status());
            backoff_secs = (backoff_secs * 2).min(30);
            continue;
        }

        backoff_secs = 1; // reset on success

        let parsed: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to parse Telegram getUpdates response: {}", e);
                continue;
            }
        };

        let updates = match parsed["result"].as_array() {
            Some(arr) => arr,
            None => continue,
        };

        for update in updates {
            // Advance offset to ack this update (Telegram requires +1)
            if let Some(uid) = update.get("update_id").and_then(|v| v.as_i64()) {
                if uid + 1 > offset {
                    offset = uid + 1;
                }
            }

            // Parse just the callback_query bits we need
            let parsed: CallbackQueryUpdate = match serde_json::from_value(update.clone()) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let cb = match parsed.callback_query {
                Some(cb) => cb,
                None => continue,
            };

            let data = match cb.data {
                Some(d) => d,
                None => continue,
            };

            // We only handle moonbag_sell:<mint>:<pct>
            if !data.starts_with("moonbag_sell:") {
                continue;
            }

            let parts: Vec<&str> = data.splitn(3, ':').collect();
            if parts.len() != 3 {
                warn!("Malformed moonbag_sell callback: {}", data);
                continue;
            }
            let mint = parts[1].to_string();
            let pct: f64 = match parts[2].parse() {
                Ok(p) => p,
                Err(_) => {
                    warn!("Invalid pct in moonbag_sell callback: {}", data);
                    continue;
                }
            };
            if pct != 50.0 && pct != 100.0 {
                warn!("Unsupported pct {} in moonbag_sell callback", pct);
                continue;
            }

            // Ack the callback (this dismisses the loading spinner on the button)
            let cb_id = cb.id.clone();
            let _ = answer_callback(&client, &bot_token, &cb_id, &format!("Selling {:.0}%…", pct)).await;

            // Look up the moonbag position
            let (symbol, balance) = {
                let tracker = moonbag_tracker.lock().await;
                let s = tracker.symbol_of(&mint);
                let b = tracker.balance_of(&mint);
                (s, b)
            };

            let symbol = match symbol {
                Some(s) => s,
                None => {
                    warn!("Moonbag sell for unknown mint: {}", mint);
                    let _ = alerts.edit_message(
                        &chat_id,
                        cb.message.as_ref().map(|m| m.message_id).unwrap_or(0),
                        "❌ Position no longer tracked (likely already sold or removed).",
                    ).await;
                    continue;
                }
            };

            let balance = match balance {
                Some(b) if b > 0.0 => b,
                _ => {
                    warn!("Moonbag sell for empty balance: {}", mint);
                    let _ = alerts.edit_message(
                        &chat_id,
                        cb.message.as_ref().map(|m| m.message_id).unwrap_or(0),
                        "❌ Balance is 0 — nothing to sell.",
                    ).await;
                    continue;
                }
            };

            // Compute sell amount
            let sell_amount = balance * (pct / 100.0);

            info!(
                "MOONBAG USER SELL: {} | {:.0}% of {:.0} tokens = {:.0} tokens",
                symbol, pct, balance, sell_amount
            );

            // Execute the sell
            let token_data = TokenData {
                mint: mint.clone(),
                symbol: symbol.clone(),
                name: String::new(),
                creator: String::new(),
                created_at: chrono::Utc::now(),
                metadata_uri: String::new(),
            };

            match executor.execute_sell(&token_data, sell_amount, 6).await {
                Ok(tx) => {
                    info!("Moonbag sell TX: {}", tx);

                    // Update the tracker's balance (or remove if 100% sold)
                    {
                        let mut tracker = moonbag_tracker.lock().await;
                        if pct >= 100.0 {
                            tracker.remove(&mint);
                        } else {
                            let new_balance = balance - sell_amount;
                            tracker.update_balance(&mint, new_balance);
                        }
                    }

                    // Edit the original alert to show it was actioned
                    if let Some(msg) = cb.message.as_ref() {
                        let _ = alerts.edit_message(
                            &chat_id,
                            msg.message_id,
                            &format!(
                                "✅ <b>SOLD {:.0}%</b> of <b>{}</b>\n\n\
                                Tokens sold: {:.0}\n\
                                <a href=\"https://solscan.io/tx/{}\">View tx</a>",
                                pct, symbol, sell_amount, tx
                            ),
                        ).await;
                    }

                    // Send a follow-up confirmation
                    let price = crate::utils::rpc::SolanaRpc::fetch_spot_price_static(&mint).await;
                    let multiplier = if price > 0.0 { price } else { 0.0 };
                    let _ = alerts.send_moonbag_sell_confirm(
                        &symbol,
                        pct,
                        multiplier,
                        &tx,
                        &mint,
                    ).await;
                }
                Err(e) => {
                    error!("Moonbag sell failed for {}: {}", mint, e);
                    if let Some(msg) = cb.message.as_ref() {
                        let _ = alerts.edit_message(
                            &chat_id,
                            msg.message_id,
                            &format!("❌ Sell failed: {}", e),
                        ).await;
                    }
                }
            }
        }
    }
}

/// Call Telegram's answerCallbackQuery API to dismiss the loading spinner
/// on the button. Without this, the user sees a spinning loader until the
/// alert is updated.
async fn answer_callback(
    client: &reqwest::Client,
    bot_token: &str,
    callback_query_id: &str,
    text: &str,
) -> anyhow::Result<()> {
    let url = format!("https://api.telegram.org/bot{}/answerCallbackQuery", bot_token);
    let body = serde_json::json!({
        "callback_query_id": callback_query_id,
        "text": text,
        "show_alert": false,
    });
    let _ = client.post(&url).json(&body).send().await?;
    Ok(())
}
