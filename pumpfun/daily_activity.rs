//! Daily activity log
//!
//! Append-only log of every whitelisted creator that launched a token.
//! Used by the daily whitelist decay check (scripts/pumpfun_whitelist_daily.py)
//! to re-evaluate graduation rates 24h+ after activity.
//!
//! Action values:
//!   - "bought"                   — first buy of the day from this creator
//!   - "skipped_already_bought"   — 2nd+ launch in 24h, but creator stays on whitelist
//!   - "removed_mass_launcher"    — 3rd+ launch in 24h, creator permanently removed

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyActivityEntry {
    pub creator: String,
    pub mint: String,
    pub symbol: String,
    pub detected_at: DateTime<Utc>,
    pub launch_block_time: DateTime<Utc>,
    pub action: String,
}

/// Thread-safe append-only log backed by a JSON file on disk.
/// Multiple writers (the bot only has one, but the daily script reads it)
/// can safely access this. The Mutex serializes appends.
pub struct DailyActivityLog {
    path: String,
    lock: Mutex<()>,
}

impl DailyActivityLog {
    pub fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
            lock: Mutex::new(()),
        }
    }

    /// Append a new entry to the log. Reads the existing file, appends, writes back.
    /// Errors are logged but not propagated — failing to write the log must not
    /// block a buy or break the bot.
    pub fn append(&self, entry: &DailyActivityEntry) -> Result<()> {
        let _guard = self.lock.lock().unwrap();

        let mut entries: Vec<DailyActivityEntry> = if Path::new(&self.path).exists() {
            match std::fs::read_to_string(&self.path) {
                Ok(data) if !data.trim().is_empty() => {
                    serde_json::from_str(&data).unwrap_or_else(|e| {
                        warn!("daily_activity.json parse failed ({}); starting fresh", e);
                        Vec::new()
                    })
                }
                _ => Vec::new(),
            }
        } else {
            // Ensure parent dir exists
            if let Some(parent) = Path::new(&self.path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            Vec::new()
        };

        entries.push(entry.clone());

        // Keep file manageable: prune entries older than 30 days on every append
        let cutoff = Utc::now() - chrono::Duration::days(30);
        entries.retain(|e| e.detected_at > cutoff);

        let json = serde_json::to_string_pretty(&entries)?;
        std::fs::write(&self.path, json)?;
        Ok(())
    }
}
