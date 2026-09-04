use anyhow::Result;
use rusqlite::{Connection, params};
use std::sync::{Arc, Mutex};
use chrono::Utc;

use crate::types::*;

#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
    pub fn new(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;

        // Enable WAL mode for better concurrent read performance
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch("PRAGMA busy_timeout=5000;")?;

        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.init_tables()?;
        Ok(db)
    }

    fn init_tables(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS creator_cache (
                wallet TEXT PRIMARY KEY,
                total_tokens INTEGER DEFAULT 0,
                avg_mcap REAL DEFAULT 0.0,
                rug_count INTEGER DEFAULT 0,
                graduated_count INTEGER DEFAULT 0,
                last_updated TEXT
            );

            CREATE TABLE IF NOT EXISTS funding_cache (
                wallet TEXT PRIMARY KEY,
                source TEXT,
                funder_wallet TEXT,
                hops_checked INTEGER,
                last_updated TEXT
            );

            CREATE TABLE IF NOT EXISTS failed_creators (
                wallet TEXT PRIMARY KEY,
                failed_at TEXT,
                ttl_secs INTEGER
            );

            CREATE TABLE IF NOT EXISTS failed_mints (
                mint TEXT PRIMARY KEY,
                failed_at TEXT,
                ttl_secs INTEGER
            );

            CREATE TABLE IF NOT EXISTS blacklist (
                wallet TEXT PRIMARY KEY,
                reason TEXT,
                added_at TEXT
            );

            CREATE TABLE IF NOT EXISTS whitelist (
                wallet TEXT PRIMARY KEY,
                score INTEGER,
                added_at TEXT,
                source TEXT DEFAULT 'daily'
            );

            CREATE TABLE IF NOT EXISTS positions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                mint TEXT,
                symbol TEXT,
                entry_price REAL,
                entry_mcap REAL,
                original_sol REAL,
                remaining_sol REAL,
                strategy_label TEXT,
                is_moonbag INTEGER DEFAULT 0,
                stop_loss_triggered INTEGER DEFAULT 0,
                created_at TEXT
            );

            CREATE TABLE IF NOT EXISTS trades (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                mint TEXT,
                symbol TEXT,
                side TEXT,
                sol_amount REAL,
                price REAL,
                mcap REAL,
                multiplier REAL,
                strategy_label TEXT,
                created_at TEXT
            );

            CREATE TABLE IF NOT EXISTS wallet_history (
                wallet TEXT,
                funder TEXT,
                funded_by TEXT,
                tx_count INTEGER DEFAULT 0,
                age_secs INTEGER DEFAULT 0,
                last_updated TEXT,
                PRIMARY KEY (wallet)
            );

            CREATE TABLE IF NOT EXISTS wallet_data (
                wallet TEXT PRIMARY KEY,
                tx_count INTEGER DEFAULT 0,
                age_secs INTEGER DEFAULT 0,
                first_seen TEXT,
                last_updated TEXT
            );

            CREATE TABLE IF NOT EXISTS creator_launches (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                creator TEXT,
                mint TEXT,
                launched_at TEXT
            );
            -- Composite index for the 24h launch count (used in mass-launcher cull).
            -- Without this, every whitelist hit does a full table scan of 70k+ rows.
            CREATE INDEX IF NOT EXISTS idx_creator_launches_creator_time
                ON creator_launches (creator, launched_at);

            CREATE TABLE IF NOT EXISTS creator_pass_count (
                wallet TEXT PRIMARY KEY,
                pass_count INTEGER DEFAULT 0,
                last_updated TEXT
            );

            -- Daily buy state: tracks last buy time per creator for the 24h first-of-day cap
            CREATE TABLE IF NOT EXISTS creator_buy_state (
                wallet TEXT PRIMARY KEY,
                last_buy_at TEXT
            );
            "
        )?;

        // Migration for existing DBs created before the 'source' column existed.
        // Use the table_info table-valued function via SELECT so query_map works.
        let mut col_stmt = conn.prepare("SELECT name FROM pragma_table_info('whitelist')")?;
        let cols: Vec<String> = col_stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|c| c.ok())
            .collect();
        drop(col_stmt);
        if !cols.iter().any(|c| c == "source") {
            conn.execute(
                "ALTER TABLE whitelist ADD COLUMN source TEXT DEFAULT 'daily'",
                [],
            )?;
            tracing::info!("DB migration: added 'source' column to whitelist table");
        }

        Ok(())
    }

    // Creator history
    pub async fn get_creator_history(&self, wallet: &str) -> Option<CreatorHistory> {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT wallet, total_tokens, avg_mcap, rug_count, graduated_count FROM creator_cache WHERE wallet = ?1"
            ).ok()?;

            stmt.query_row(params![wallet], |row| {
                Ok(CreatorHistory {
                    wallet: row.get(0)?,
                    total_tokens: row.get(1)?,
                    avg_mcap: row.get(2)?,
                    rug_count: row.get(3)?,
                    graduated_count: row.get(4)?,
                })
            }).ok()
        }).await.ok().flatten()
    }

    pub async fn update_creator_history(&self, history: &CreatorHistory) -> Result<()> {
        let conn = self.conn.clone();
        let history = history.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO creator_cache (wallet, total_tokens, avg_mcap, rug_count, graduated_count, last_updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    history.wallet,
                    history.total_tokens,
                    history.avg_mcap,
                    history.rug_count,
                    history.graduated_count,
                    Utc::now().to_rfc3339(),
                ],
            )?;
            Ok(())
        }).await?
    }

    // Funding cache
    pub async fn get_funding_cache(&self, wallet: &str) -> Option<FundingCacheEntry> {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT wallet, source, funder_wallet FROM funding_cache WHERE wallet = ?1"
            ).ok()?;

            stmt.query_row(params![wallet], |row| {
                let source_str: String = row.get(1)?;
                Ok(FundingCacheEntry {
                    wallet: row.get(0)?,
                    source: parse_funding_source(&source_str),
                    funder_wallet: row.get(2)?,
                })
            }).ok()
        }).await.ok().flatten()
    }

    pub async fn cache_funding(&self, wallet: &str, source: &FundingSource, funder: &str) -> Result<()> {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        let source_str = format!("{:?}", source);
        let funder = funder.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO funding_cache (wallet, source, funder_wallet, hops_checked, last_updated)
                 VALUES (?1, ?2, ?3, 0, ?4)",
                params![wallet, source_str, funder, Utc::now().to_rfc3339()],
            )?;
            Ok(())
        }).await?
    }

    // Wallet funder lookup
    pub async fn get_wallet_funder(&self, wallet: &str) -> Option<String> {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            // Check wallet_data first (new table), then wallet_history (legacy)
            let result = conn.prepare(
                "SELECT funded_by FROM wallet_history WHERE wallet = ?1 AND funded_by IS NOT NULL"
            ).ok()
                .and_then(|mut stmt| {
                    stmt.query_row(params![wallet], |row| row.get::<_, String>(0)).ok()
                });

            if result.is_some() {
                return result;
            }

            // Fallback to funder column
            let mut stmt = conn.prepare(
                "SELECT funder FROM wallet_history WHERE wallet = ?1 AND funder IS NOT NULL"
            ).ok()?;
            stmt.query_row(params![wallet], |row| row.get::<_, String>(0)).ok()
        }).await.ok().flatten()
    }

    pub async fn get_wallet_tx_count(&self, wallet: &str) -> u64 {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            // Try wallet_data first
            let result = conn.prepare(
                "SELECT tx_count FROM wallet_data WHERE wallet = ?1"
            ).ok()
                .and_then(|mut stmt| {
                    stmt.query_row(params![wallet], |row| {
                        let count: i64 = row.get(0)?;
                        Ok(count as u64)
                    }).ok()
                });

            if let Some(count) = result {
                if count > 0 {
                    return count;
                }
            }

            // Fallback to wallet_history
            let mut stmt = match conn.prepare(
                "SELECT tx_count FROM wallet_history WHERE wallet = ?1"
            ) {
                Ok(s) => s,
                Err(_) => return 0,
            };
            stmt.query_row(params![wallet], |row| {
                let count: i64 = row.get(0)?;
                Ok(count as u64)
            }).unwrap_or(0)
        }).await.unwrap_or(0)
    }

    pub async fn get_wallet_age_secs(&self, wallet: &str) -> u64 {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            // Try wallet_data first
            let result = conn.prepare(
                "SELECT age_secs FROM wallet_data WHERE wallet = ?1"
            ).ok()
                .and_then(|mut stmt| {
                    stmt.query_row(params![wallet], |row| {
                        let age: i64 = row.get(0)?;
                        Ok(age as u64)
                    }).ok()
                });

            if let Some(age) = result {
                if age > 0 {
                    return age;
                }
            }

            // Fallback to wallet_history
            let mut stmt = match conn.prepare(
                "SELECT age_secs FROM wallet_history WHERE wallet = ?1"
            ) {
                Ok(s) => s,
                Err(_) => return 0,
            };
            stmt.query_row(params![wallet], |row| {
                let age: i64 = row.get(0)?;
                Ok(age as u64)
            }).unwrap_or(0)
        }).await.unwrap_or(0)
    }

    /// Save wallet funder data (from blockchain trace)
    pub async fn save_wallet_funder(&self, wallet: &str, funder: &str, tx_count: u64, age_secs: u64) -> Result<()> {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        let funder = funder.to_string();
        let now = Utc::now().to_rfc3339();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            // Save to wallet_history (used by get_wallet_funder)
            conn.execute(
                "INSERT OR REPLACE INTO wallet_history (wallet, funder, funded_by, tx_count, age_secs, last_updated)
                 VALUES (?1, ?2, ?2, ?3, ?4, ?5)",
                params![wallet, funder, tx_count as i64, age_secs as i64, now],
            )?;
            // Also save to wallet_data
            conn.execute(
                "INSERT OR REPLACE INTO wallet_data (wallet, tx_count, age_secs, first_seen, last_updated)
                 VALUES (?1, ?2, ?3, COALESCE((SELECT first_seen FROM wallet_data WHERE wallet = ?1), ?4), ?4)",
                params![wallet, tx_count as i64, age_secs as i64, now],
            )?;
            Ok(())
        }).await?
    }

    /// Upsert wallet tracking data (tx_count, age, funder)
    pub async fn update_wallet_data(&self, wallet: &str, tx_count: u64, age_secs: u64) -> Result<()> {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        let now = Utc::now().to_rfc3339();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO wallet_data (wallet, tx_count, age_secs, first_seen, last_updated)
                 VALUES (?1, ?2, ?3, COALESCE((SELECT first_seen FROM wallet_data WHERE wallet = ?1), ?4), ?4)",
                params![wallet, tx_count as i64, age_secs as i64, now],
            )?;
            Ok(())
        }).await?
    }

    // Blacklist
    pub async fn creator_launch_count_24h(&self, creator: &str) -> u64 {
        let conn = self.conn.clone();
        let creator = creator.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT COUNT(*) FROM creator_launches WHERE creator = ?1 AND launched_at >= datetime('now', '-24 hours')"
            ).unwrap();
            stmt.query_row(params![creator], |row| {
                let count: i64 = row.get(0)?;
                Ok(count as u64)
            }).unwrap_or(0)
        }).await.unwrap_or(0)
    }

    pub async fn record_creator_launch(&self, creator: &str, mint: &str) -> Result<()> {
        let conn = self.conn.clone();
        let creator = creator.to_string();
        let mint = mint.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT INTO creator_launches (creator, mint, launched_at) VALUES (?1, ?2, datetime('now'))",
                params![creator, mint],
            )?;
            Ok(())
        }).await?
    }

    /// Record that we just bought a token from this creator. Used to enforce
    /// the 24h first-of-day buy cap (one buy per creator per rolling 24h).
    pub async fn record_buy_from_creator(&self, wallet: &str) -> Result<()> {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO creator_buy_state (wallet, last_buy_at) VALUES (?1, datetime('now'))",
                params![wallet],
            )?;
            Ok(())
        }).await?
    }

    /// Has the bot bought from this creator in the last 24h?
    /// Used to enforce the "max 1 buy per creator per 24h" cap.
    pub async fn recent_buy_from_creator_24h(&self, wallet: &str) -> bool {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT COUNT(*) FROM creator_buy_state WHERE wallet = ?1 AND last_buy_at >= datetime('now', '-24 hours')"
            ).unwrap();
            let count: i64 = stmt.query_row(params![wallet], |row| row.get(0)).unwrap_or(0);
            count > 0
        }).await.unwrap_or(false)
    }

    pub async fn remove_from_whitelist(&self, wallet: &str) -> Result<()> {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "DELETE FROM whitelist WHERE wallet = ?1",
                params![wallet],
            )?;
            Ok(())
        }).await?
    }

    pub async fn is_blacklisted(&self, wallet: &str) -> bool {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT COUNT(*) FROM blacklist WHERE wallet = ?1"
            ).unwrap();

            let count: i64 = stmt.query_row(params![wallet], |row| row.get(0)).unwrap_or(0);
            count > 0
        }).await.unwrap_or(false)
    }

    pub async fn blacklist_wallet(&self, wallet: &str) -> Result<()> {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO blacklist (wallet, reason, added_at) VALUES (?1, 'serial_rugger', ?2)",
                params![wallet, Utc::now().to_rfc3339()],
            )?;
            Ok(())
        }).await?
    }

    // Whitelist
    /// Increment creator pass count and return new count.
    /// Used by Bug 7 fix to require multiple successful passes before whitelisting.
    pub async fn increment_creator_pass_count(&self, wallet: &str) -> u64 {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT INTO creator_pass_count (wallet, pass_count, last_updated)
                 VALUES (?1, 1, ?2)
                 ON CONFLICT(wallet) DO UPDATE SET pass_count = pass_count + 1, last_updated = ?2",
                params![wallet, Utc::now().to_rfc3339()],
            ).ok();
            let mut stmt = conn.prepare(
                "SELECT pass_count FROM creator_pass_count WHERE wallet = ?1"
            ).ok()?;
            let count: i64 = stmt.query_row(params![wallet], |row| row.get(0)).unwrap_or(0);
            Some(count as u64)
        }).await.unwrap_or(None).unwrap_or(0)
    }

    pub async fn is_whitelisted(&self, wallet: &str) -> bool {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT COUNT(*) FROM whitelist WHERE wallet = ?1"
            ).unwrap();

            let count: i64 = stmt.query_row(params![wallet], |row| row.get(0)).unwrap_or(0);
            count > 0
        }).await.unwrap_or(false)
    }

    /// Get the score for a whitelisted wallet. Returns None if not in whitelist.
    pub async fn get_whitelist_score(&self, wallet: &str) -> Option<u8> {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT score FROM whitelist WHERE wallet = ?1"
            ).ok()?;
            let score: i64 = stmt.query_row(params![wallet], |row| row.get(0)).ok()?;
            Some(score as u8)
        }).await.unwrap_or(None)
    }

    pub async fn whitelist_wallet(&self, wallet: &str, score: u8) -> Result<()> {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO whitelist (wallet, score, added_at, source) VALUES (?1, ?2, ?3, 'firsttimer')",
                params![wallet, score, Utc::now().to_rfc3339()],
            )?;
            Ok(())
        }).await?
    }

    /// Bulk import whitelisted creators from dev_whitelist.json (generated by daily Dune scraper).
    /// Maps: creator → wallet, rank → score. Skips duplicates. Sets source='daily'.
    pub async fn import_whitelist_from_json(&self, json_path: &str) -> Result<usize> {
        let conn = self.conn.clone();
        let path = json_path.to_string();
        tokio::task::spawn_blocking(move || {
            let data = std::fs::read_to_string(&path)?;
            let entries: Vec<serde_json::Value> = serde_json::from_str(&data)?;
            let conn = conn.lock().unwrap();
            let mut imported = 0;
            for entry in &entries {
                let wallet = match entry.get("creator").and_then(|v| v.as_str()) {
                    Some(w) => w,
                    None => continue,
                };
                let score = entry.get("rank")
                    .or_else(|| entry.get("score"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(50)
                    .clamp(0, 100) as u8;
                conn.execute(
                    "INSERT OR IGNORE INTO whitelist (wallet, score, added_at, source) VALUES (?1, ?2, ?3, 'daily')",
                    params![wallet, score, Utc::now().to_rfc3339()],
                )?;
                if conn.changes() > 0 {
                    imported += 1;
                }
            }
            Ok(imported)
        }).await?
    }

    /// Full reload of the daily-scraped whitelist from JSON.
    /// Wraps the operation in a transaction: wipes all rows with source='daily'
    /// and re-inserts from JSON. Preserves first-timer additions (source='firsttimer')
    /// and any rows where source IS NULL (legacy data).
    /// Returns the number of entries loaded from JSON.
    /// Refuses to wipe if JSON is empty (safety: scraper failure shouldn't nuke the list).
    pub async fn reload_whitelist_from_json(&self, json_path: &str) -> Result<usize> {
        let conn = self.conn.clone();
        let path = json_path.to_string();
        tokio::task::spawn_blocking(move || {
            let data = std::fs::read_to_string(&path)?;
            let entries: Vec<serde_json::Value> = serde_json::from_str(&data)?;

            if entries.is_empty() {
                anyhow::bail!("Refusing to reload from empty JSON (would wipe the entire daily whitelist)");
            }

            let mut conn = conn.lock().unwrap();
            let tx = conn.transaction()?;

            // Wipe only daily-sourced rows. Keep firsttimer additions and legacy NULLs.
            tx.execute(
                "DELETE FROM whitelist WHERE source = 'daily'",
                [],
            )?;

            let now = Utc::now().to_rfc3339();
            let mut imported = 0;
            for entry in &entries {
                let wallet = match entry.get("creator").and_then(|v| v.as_str()) {
                    Some(w) => w,
                    None => continue,
                };
                let score = entry.get("rank")
                    .or_else(|| entry.get("score"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(50)
                    .clamp(0, 100) as u8;
                tx.execute(
                    "INSERT INTO whitelist (wallet, score, added_at, source) VALUES (?1, ?2, ?3, 'daily')",
                    params![wallet, score, now],
                )?;
                imported += 1;
            }

            tx.commit()?;
            Ok(imported)
        }).await?
    }

    // Failed cache
    pub async fn is_creator_cached_failed(&self, wallet: &str, ttl_secs: u64) -> bool {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT COUNT(*) FROM failed_creators WHERE wallet = ?1 AND (strftime('%s','now') - strftime('%s',failed_at)) < ?2"
            ).unwrap();

            let count: i64 = stmt.query_row(params![wallet, ttl_secs], |row| row.get(0)).unwrap_or(0);
            count > 0
        }).await.unwrap_or(false)
    }

    pub async fn cache_failed_creator(&self, wallet: &str, ttl_secs: u64) -> Result<()> {
        let conn = self.conn.clone();
        let wallet = wallet.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO failed_creators (wallet, failed_at, ttl_secs) VALUES (?1, ?2, ?3)",
                params![wallet, Utc::now().to_rfc3339(), ttl_secs],
            )?;
            Ok(())
        }).await?
    }

    pub async fn is_mint_cached_failed(&self, mint: &str, ttl_secs: u64) -> bool {
        let conn = self.conn.clone();
        let mint = mint.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT COUNT(*) FROM failed_mints WHERE mint = ?1 AND (strftime('%s','now') - strftime('%s',failed_at)) < ?2"
            ).unwrap();

            let count: i64 = stmt.query_row(params![mint, ttl_secs], |row| row.get(0)).unwrap_or(0);
            count > 0
        }).await.unwrap_or(false)
    }

    pub async fn cache_failed_mint(&self, mint: &str, ttl_secs: u64) -> Result<()> {
        let conn = self.conn.clone();
        let mint = mint.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO failed_mints (mint, failed_at, ttl_secs) VALUES (?1, ?2, ?3)",
                params![mint, Utc::now().to_rfc3339(), ttl_secs],
            )?;
            Ok(())
        }).await?
    }

    // Positions
    /// Check if there's an active position for a given mint (remaining_sol > 0)
    pub async fn has_active_position(&self, mint: &str) -> bool {
        let conn = self.conn.clone();
        let mint = mint.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT COUNT(*) FROM positions WHERE mint = ?1 AND remaining_sol > 0 AND stop_loss_triggered = 0"
            ).unwrap();
            let count: i64 = stmt.query_row(params![mint], |row| row.get(0)).unwrap_or(0);
            count > 0
        }).await.unwrap_or(false)
    }

    /// Load all positions flagged as moonbags (for restoring MoonbagTracker on restart)
    pub async fn get_moonbag_positions(&self) -> Vec<Position> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = match conn.prepare(
                "SELECT id, mint, symbol, entry_price, entry_mcap, original_sol, remaining_sol, strategy_label, is_moonbag, stop_loss_triggered, created_at FROM positions WHERE is_moonbag = 1 AND remaining_sol > 0"
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let positions: Vec<Position> = match stmt.query_map([], |row| {
                Ok(Position {
                    id: row.get(0)?,
                    mint: row.get(1)?,
                    symbol: row.get(2)?,
                    entry_price: row.get(3)?,
                    entry_mcap: row.get(4)?,
                    original_sol: row.get(5)?,
                    remaining_sol: row.get(6)?,
                    strategy_label: row.get(7)?,
                    is_moonbag: row.get::<_, i32>(8)? != 0,
                    stop_loss_triggered: row.get::<_, i32>(9)? != 0,
                    created_at: chrono::DateTime::parse_from_rfc3339(
                        &row.get::<_, String>(10)?
                    ).unwrap_or_default().with_timezone(&chrono::Utc),
                })
            }) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(_) => Vec::new(),
            };
            positions
        }).await.unwrap_or_default()
    }

    pub async fn save_position(&self, pos: &Position) -> Result<()> {
        let conn = self.conn.clone();
        let pos = pos.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT INTO positions (mint, symbol, entry_price, entry_mcap, original_sol, remaining_sol, strategy_label, is_moonbag, stop_loss_triggered, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    pos.mint, pos.symbol, pos.entry_price, pos.entry_mcap,
                    pos.original_sol, pos.remaining_sol, pos.strategy_label,
                    pos.is_moonbag as i32, pos.stop_loss_triggered as i32,
                    pos.created_at.to_rfc3339(),
                ],
            )?;
            Ok(())
        }).await?
    }

    pub async fn update_position(&self, pos: &Position) -> Result<()> {
        let conn = self.conn.clone();
        let pos = pos.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "UPDATE positions SET entry_price = ?1, remaining_sol = ?2, is_moonbag = ?3, stop_loss_triggered = ?4 WHERE id = (SELECT MAX(id) FROM positions WHERE mint = ?5)",
                params![
                    pos.entry_price,
                    pos.remaining_sol,
                    pos.is_moonbag as i32,
                    pos.stop_loss_triggered as i32,
                    pos.mint,
                ],
            )?;
            Ok(())
        }).await?
    }

    // Trades
    pub async fn log_trade(&self, trade: &TradeLog) -> Result<()> {
        let conn = self.conn.clone();
        let trade = trade.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT INTO trades (mint, symbol, side, sol_amount, price, mcap, multiplier, strategy_label, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    trade.mint, trade.symbol, trade.side, trade.sol_amount,
                    trade.price, trade.mcap, trade.multiplier,
                    trade.strategy_label, trade.created_at.to_rfc3339(),
                ],
            )?;
            Ok(())
        }).await?
    }

    pub async fn get_trades_since(&self, since: &str) -> Result<Vec<TradeLog>> {
        let conn = self.conn.clone();
        let since = since.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT id, mint, symbol, side, sol_amount, price, mcap, multiplier, strategy_label, created_at FROM trades WHERE created_at >= ?1"
            )?;

            let trades = stmt.query_map(params![since], |row| {
                Ok(TradeLog {
                    id: row.get(0)?,
                    mint: row.get(1)?,
                    symbol: row.get(2)?,
                    side: row.get(3)?,
                    sol_amount: row.get(4)?,
                    price: row.get(5)?,
                    mcap: row.get(6)?,
                    multiplier: row.get(7)?,
                    strategy_label: row.get(8)?,
                    created_at: chrono::DateTime::parse_from_rfc3339(
                        &row.get::<_, String>(9)?
                    ).unwrap_or_default().with_timezone(&chrono::Utc),
                })
            })?.filter_map(|r| r.ok()).collect();

            Ok(trades)
        }).await?
    }
}

// Helper structs
#[derive(Debug, Clone)]
pub struct FundingCacheEntry {
    pub wallet: String,
    pub source: FundingSource,
    pub funder_wallet: String,
}

fn parse_funding_source(s: &str) -> FundingSource {
    match s {
        "CEX" => FundingSource::CEX,
        "NormalWallet" => FundingSource::NormalWallet,
        "SerialRugger" => FundingSource::SerialRugger,
        "FreshWallet" => FundingSource::FreshWallet,
        "Mixer" => FundingSource::Mixer,
        _ => FundingSource::Unknown,
    }
}
