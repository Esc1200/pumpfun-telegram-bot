use chrono::Utc;
use tracing::{info, warn, debug};

use crate::types::TokenData;

pub struct TransactionParser {
    program_id: String,
}

impl TransactionParser {
    pub fn new(program_id: &str) -> Self {
        Self {
            program_id: program_id.to_string(),
        }
    }

    /// Parse a programNotification from standard Solana programSubscribe
    /// The notification format is:
    /// { "method": "programNotification", "params": { "result": { "value": { "pubkey": "...", "account": {...} } } } }
    pub fn parse_program_notification(&self, notification: &serde_json::Value) -> Option<TokenData> {
        let params = notification.get("params")?;
        let result = params.get("result")?;
        let value = result.get("value")?;

        let pubkey = value.get("pubkey")?.as_str()?;
        let account = value.get("account")?;
        let owner = account.get("owner")?.as_str()?;

        // Only process accounts owned by pump.fun program
        if owner != self.program_id {
            return None;
        }

        // Get account data (base64 encoded)
        let data = account.get("data")?;
        let data_array = data.as_array()?;
        if data_array.len() < 2 {
            return None;
        }

        let encoded_data = data_array[0].as_str()?;
        let encoding = data_array[1].as_str()?;

        if encoding != "base64" {
            return None;
        }

        // Decode base64 data
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD.decode(encoded_data).ok()?;

        // Pump.fun bonding curve accounts have a specific layout
        // The account data contains the mint address at a known offset
        // Account layout (simplified):
        //   [0..8]   - discriminator (8 bytes)
        //   [8..16]  - virtual token reserves (u64)
        //   [16..24] - virtual sol reserves (u64)
        //   [24..32] - real token reserves (u64)
        //   [32..40] - real sol reserves (u64)
        //   [40..48] - token total supply (u64)
        //   [48]     - complete flag (bool)
        //   [49..81] - mint address (32 bytes, base58)

        if decoded.len() < 81 {
            debug!("Account data too small ({} bytes), not a bonding curve", decoded.len());
            return None;
        }

        // Extract mint address from bytes 49..81
        let mint_bytes = &decoded[49..81];
        let mint = bs58::encode(mint_bytes).into_string();

        // Validate it looks like a pubkey
        if mint.len() < 32 || mint.len() > 44 {
            return None;
        }

        // Extract virtual SOL reserves to estimate market cap
        // Bytes 16..24 = virtual_sol_reserves (little-endian u64)
        let virtual_sol = u64::from_le_bytes(decoded[16..24].try_into().ok()?) as f64;

        // Skip accounts with very low SOL (dust accounts)
        if virtual_sol < 1_000_000.0 {
            // Less than 0.001 SOL
            return None;
        }

        info!(
            "Bonding curve activity: mint={}, sol_reserves={:.4} SOL",
            &mint[..mint.len().min(8)],
            virtual_sol / 1e9
        );

        // We don't have the creator directly from the account data
        // Use the account pubkey as a placeholder — the main loop will
        // need to query the transaction to get the actual creator
        Some(TokenData {
            mint: mint.clone(),
            symbol: format!("{}...", &mint[..4]),
            name: "Unknown".to_string(),
            creator: pubkey.to_string(), // Placeholder
            created_at: Utc::now(),
            metadata_uri: String::new(),
        })
    }

    /// Parse a Helius transactionNotification (enhanced plan only)
    pub fn parse_notification(&self, notification: &serde_json::Value) -> Option<TokenData> {
        let params = notification.get("params")?;
        let result = params.get("result")?;

        let signature = result.get("signature")?.as_str()?;

        let tx = result.get("transaction")?;
        let transaction = tx.get("transaction")?;
        let message = transaction.get("message")?;
        let account_keys = message.get("accountKeys")?.as_array()?;

        let meta = tx.get("meta")?;
        let logs = meta
            .get("logMessages")
            .and_then(|l| l.as_array())
            .cloned()
            .unwrap_or_default();

        let is_create = logs.iter().any(|log| {
            log.as_str().map_or(false, |s| {
                s.contains("Program log: Instruction: Create")
                    || s.contains("Program log: Instruction: InitializeMint")
                    || s.contains("create")
            })
        });

        if !is_create {
            let has_program = account_keys
                .iter()
                .any(|key| key.as_str().map_or(false, |k| k == self.program_id));

            if !has_program {
                return None;
            }
        }

        let mint = self.extract_mint(account_keys, &logs)?;
        let creator = account_keys.first()?.as_str()?.to_string();

        let (name, symbol) = self.extract_metadata_from_logs(&logs);
        let metadata_uri = self.extract_metadata_uri(&logs);

        info!(
            "Parsed new token: {} ({}) by {} | sig: {}",
            symbol,
            &mint[..8.min(mint.len())],
            &creator[..8.min(creator.len())],
            &signature[..16.min(signature.len())]
        );

        Some(TokenData {
            mint,
            symbol,
            name,
            creator,
            created_at: Utc::now(),
            metadata_uri,
        })
    }

    /// Extract mint address from transaction logs (for polling approach)
    pub fn extract_mint_from_logs(&self, logs: &[serde_json::Value]) -> Option<String> {
        for log in logs {
            if let Some(s) = log.as_str() {
                // Look for mint in various log formats
                for prefix in &["mint:", "mint: ", "Mint: "] {
                    if let Some(rest) = s.split(prefix).nth(1) {
                        let mint = rest.trim().split_whitespace().next()?;
                        if mint.len() >= 32 && mint.len() <= 44 && mint.chars().all(|c| c.is_alphanumeric()) {
                            return Some(mint.to_string());
                        }
                    }
                }
            }
        }
        None
    }

    fn extract_mint(
        &self,
        account_keys: &[serde_json::Value],
        logs: &[serde_json::Value],
    ) -> Option<String> {
        if account_keys.len() >= 2 {
            if let Some(mint) = account_keys.get(1).and_then(|v| v.as_str()) {
                if mint.len() >= 32 && mint.len() <= 44 {
                    return Some(mint.to_string());
                }
            }
        }

        for log in logs {
            if let Some(s) = log.as_str() {
                if s.contains("mint:") {
                    if let Some(mint) = s.split("mint:").nth(1) {
                        let mint = mint.trim().split_whitespace().next()?;
                        if mint.len() >= 32 && mint.len() <= 44 {
                            return Some(mint.to_string());
                        }
                    }
                }
            }
        }

        None
    }

    pub fn extract_metadata_from_logs(&self, logs: &[serde_json::Value]) -> (String, String) {
        let mut name = String::new();
        let mut symbol = String::new();

        for log in logs {
            if let Some(s) = log.as_str() {
                if s.contains("name:") {
                    if let Some(val) = s.split("name:").nth(1) {
                        name = val
                            .trim()
                            .split(',')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string();
                    }
                }
                if s.contains("symbol:") {
                    if let Some(val) = s.split("symbol:").nth(1) {
                        symbol = val
                            .trim()
                            .split(',')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string();
                    }
                }
            }
        }

        if name.is_empty() {
            name = "Unknown".to_string();
        }
        if symbol.is_empty() {
            symbol = "???".to_string();
        }

        (name, symbol)
    }

    fn extract_metadata_uri(&self, logs: &[serde_json::Value]) -> String {
        for log in logs {
            if let Some(s) = log.as_str() {
                if s.contains("uri:") || s.contains("metadata_uri:") {
                    if let Some(val) = s
                        .split("uri:")
                        .nth(1)
                        .or_else(|| s.split("metadata_uri:").nth(1))
                    {
                        return val.trim().to_string();
                    }
                }
            }
        }
        String::new()
    }
}
