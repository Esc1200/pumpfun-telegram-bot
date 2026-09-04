use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::config::AlertsConfig;

pub struct AlertManager {
    config: AlertsConfig,
    bot_token: String,
    chat_id: String,
    client: Client,
}

impl AlertManager {
    pub fn new(config: AlertsConfig) -> Self {
        let bot_token = std::env::var("TELEGRAM_BOT_TOKEN")
            .unwrap_or_default();
        let chat_id = std::env::var("TELEGRAM_CHAT_ID")
            .unwrap_or_default();

        Self {
            config,
            bot_token,
            chat_id,
            client: Client::new(),
        }
    }

    /// Returns the bot token (used by the callback listener to poll getUpdates
    /// and call answerCallbackQuery on the same bot).
    pub fn bot_token(&self) -> &str { &self.bot_token }
    pub fn chat_id(&self) -> &str { &self.chat_id }
    pub fn http_client(&self) -> &Client { &self.client }

    /// Send a plain text message. Used for buy/sell alerts that don't need buttons.
    pub async fn send(&self, message: &str) {
        if !self.config.enabled { return; }
        if self.bot_token.is_empty() || self.chat_id.is_empty() { return; }

        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);
        let payload = json!({
            "chat_id": self.chat_id,
            "text": message,
            "parse_mode": "HTML",
            "disable_web_page_preview": true
        });

        match self.client.post(&url).json(&payload).send().await {
            Ok(_) => {}
            Err(e) => warn!("Failed to send Telegram alert: {}", e),
        }
    }

    /// Send a message with an inline keyboard. `buttons` is rows of (text, callback_data)
    /// pairs. The callback listener (`poll_callbacks`) handles `callback_data` strings
    /// matching the format "moonbag_sell:<mint>:<pct>".
    pub async fn send_with_buttons(&self, message: &str, buttons: Vec<Vec<(String, String)>>) {
        if !self.config.enabled { return; }
        if self.bot_token.is_empty() || self.chat_id.is_empty() { return; }

        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);

        let inline_keyboard: Vec<Vec<serde_json::Value>> = buttons
            .iter()
            .map(|row| {
                row.iter()
                    .map(|(text, cb_data)| json!({
                        "text": text,
                        "callback_data": cb_data
                    }))
                    .collect()
            })
            .collect();

        let payload = json!({
            "chat_id": self.chat_id,
            "text": message,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
            "reply_markup": { "inline_keyboard": inline_keyboard }
        });

        match self.client.post(&url).json(&payload).send().await {
            Ok(_) => {}
            Err(e) => warn!("Failed to send Telegram alert (with buttons): {}", e),
        }
    }

    /// Edit an existing message's text + buttons. Used by the callback listener to
    /// update a moonbag alert after the user taps Sell 50% / Sell 100% (e.g.,
    /// remove the buttons, add a confirmation line).
    pub async fn edit_message(
        &self,
        chat_id: &str,
        message_id: i64,
        new_text: &str,
    ) {
        if self.bot_token.is_empty() { return; }
        let url = format!("https://api.telegram.org/bot{}/editMessageText", self.bot_token);
        let payload = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": new_text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true
        });
        let _ = self.client.post(&url).json(&payload).send().await;
    }

    pub async fn send_buy_alert(
        &self,
        symbol: &str,
        amount_sol: f64,
        entry_price: f64,
        mcap: f64,
        strategy: &str,
        score: u8,
        mint: &str,
        tx: &str,
    ) {
        if !self.config.alert_on_buy { return; }

        let dexscreener = format!("https://dexscreener.com/solana/{}", mint);
        let geckoterminal = format!("https://geckoterminal.com/solana/pools/{}", mint);
        let solscan = format!("https://solscan.io/tx/{}", tx);

        self.send(&format!(
            "🟢 <b>BUY</b>\n\
            Token: <b>{}</b>\n\
            Amount: {:.4} SOL\n\
            Entry: {:.12} SOL/token\n\
            Mcap: ${:.0}\n\
            Strategy: {} | Score: {}\n\
            \n\
            <a href=\"{}\">DexScreener</a> | <a href=\"{}\">GeckoTerminal</a> | <a href=\"{}\">Tx</a>",
            symbol, amount_sol, entry_price, mcap, strategy, score,
            dexscreener, geckoterminal, solscan
        )).await;
    }

    pub async fn send_sell_alert(
        &self,
        symbol: &str,
        sell_pct: f64,
        multiplier: f64,
        strategy: &str,
        mint: &str,
    ) {
        if !self.config.alert_on_sell { return; }

        let dexscreener = format!("https://dexscreener.com/solana/{}", mint);

        self.send(&format!(
            "🔴 <b>{} SELL</b>\n\
            Token: <b>{}</b>\n\
            Sold: {:.0}% at {:.1}x\n\
            \n\
            <a href=\"{}\">DexScreener</a>",
            strategy, symbol, sell_pct, multiplier, dexscreener
        )).await;
    }

    pub async fn send_stop_loss_alert(
        &self,
        symbol: &str,
        multiplier: f64,
        pnl_pct: f64,
        moonbag_sol: f64,
        moonbag_pct: f64,
        mint: &str,
    ) {
        if !self.config.alert_on_stop_loss { return; }

        let dexscreener = format!("https://dexscreener.com/solana/{}", mint);

        self.send(&format!(
            "🛑 <b>STOP LOSS</b>\n\
            Token: <b>{}</b>\n\
            Exit: {:.2}x ({:.1}%)\n\
            Moonbag: {:.4} SOL ({:.0}%)\n\
            \n\
            <a href=\"{}\">DexScreener</a>",
            symbol, multiplier, pnl_pct, moonbag_sol, moonbag_pct, dexscreener
        )).await;
    }

    /// Special alert for first-timer buys (brand-new CEX-funded creator caught at launch).
    /// Highlights the "first-timer" angle so the operator can spot them at a glance.
    pub async fn send_first_timer_buy_alert(
        &self,
        symbol: &str,
        creator: &str,
        amount_sol: f64,
        score: u8,
        wallet_age_secs: u64,
        sol_balance: f64,
        mint: &str,
        tx: &str,
    ) {
        if !self.config.alert_on_first_timer_buy { return; }

        let dexscreener = format!("https://dexscreener.com/solana/{}", mint);
        let solscan_tx = format!("https://solscan.io/tx/{}", tx);
        let pumpfun = format!("https://pump.fun/{}", mint);
        let creator_short = &creator[..creator.len().min(8)];

        // Convert age to a friendly unit
        let age_str = if wallet_age_secs < 3600 {
            format!("{}m", wallet_age_secs / 60)
        } else if wallet_age_secs < 86400 {
            format!("{:.1}h", wallet_age_secs as f64 / 3600.0)
        } else {
            format!("{:.1}d", wallet_age_secs as f64 / 86400.0)
        };

        self.send(&format!(
            "🔥 <b>FIRST-TIMER BUY</b>\n\
            \n\
            Token: <b>{}</b>\n\
            Creator: <code>{}</code>\n\
            \n\
            Wallet age: {} | Balance: {:.2} SOL\n\
            Amount: <b>{:.4} SOL</b> (score {})\n\
            \n\
            <a href=\"{}\">Pump.fun</a> | <a href=\"{}\">DexScreener</a> | <a href=\"{}\">Tx</a>",
            symbol, creator_short, age_str, sol_balance, amount_sol, score,
            pumpfun, dexscreener, solscan_tx
        )).await;
    }

    /// Moonbag pump alert — fires every time the price doubles from the last
    /// alert. Includes two inline buttons: Sell 50% and Sell 100%. The user
    /// decides whether to sell; the bot never auto-sells moonbags.
    ///
    /// `pct_from_entry` is the % rise from the position's buy entry. We use
    /// this in the message so the user can see "this is a 1000x from buy"
    /// etc.
    pub async fn send_moonbag_alert(
        &self,
        symbol: &str,
        multiplier: f64,
        pct_from_entry: f64,
        token_balance: f64,
        estimated_sol: f64,
        mint: &str,
    ) {
        if !self.config.alert_on_moonbag_pump { return; }

        let dexscreener = format!("https://dexscreener.com/solana/{}", mint);
        let pumpfun = format!("https://pump.fun/{}", mint);

        let msg = format!(
            "🌙 <b>MOONBAG PUMP</b>\n\
            \n\
            Token: <b>{}</b>\n\
            Multiplier: <b>{:.1}x</b> from entry (+{:.0}%)\n\
            \n\
            Holding: {:.0} tokens (~{:.4} SOL)\n\
            \n\
            <a href=\"{}\">Pump.fun</a> | <a href=\"{}\">DexScreener</a>\n\
            \n\
            <i>Tap a button to sell. No action = keep holding.</i>",
            symbol, multiplier, pct_from_entry, token_balance, estimated_sol,
            pumpfun, dexscreener
        );

        // Inline buttons: one row, two actions
        let buttons = vec![vec![
            ("💸 Sell 50%".to_string(), format!("moonbag_sell:{}:50", mint)),
            ("💸 Sell 100%".to_string(), format!("moonbag_sell:{}:100", mint)),
        ]];

        self.send_with_buttons(&msg, buttons).await;
    }

    /// Confirmation message after a user-triggered moonbag sell. Replaces the
    /// original alert's text so the buttons don't appear stale.
    pub async fn send_moonbag_sell_confirm(
        &self,
        symbol: &str,
        sell_pct: f64,
        multiplier: f64,
        tx: &str,
        mint: &str,
    ) {
        let solscan = format!("https://solscan.io/tx/{}", tx);
        self.send(&format!(
            "✅ <b>MOONBAG SOLD</b>\n\
            \n\
            Token: <b>{}</b>\n\
            Sold: {:.0}% at {:.1}x\n\
            \n\
            <a href=\"{}\">Tx</a>",
            symbol, sell_pct, multiplier, solscan
        )).await;
    }
}

/// Decoded payload of a Telegram update with a callback_query. We only need
/// the bits the listener cares about (chat_id, message_id, callback_data).
#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQueryUpdate {
    pub update_id: i64,
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: Option<CallbackUser>,
    pub message: Option<CallbackMessage>,
    pub data: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackUser {
    pub id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackMessage {
    pub chat: CallbackChat,
    pub message_id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackChat {
    pub id: i64,
}

/// Response from getUpdates — Telegram returns a `result` array of either
/// message updates OR callback_query updates. We only care about the latter.
#[derive(Debug, Clone, Deserialize)]
pub struct GetUpdatesResponse {
    pub ok: bool,
    pub result: Vec<serde_json::Value>,
}
