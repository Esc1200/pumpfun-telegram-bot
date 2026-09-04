use std::collections::HashSet;
use std::sync::RwLock;
use tracing::info;

/// In-memory blacklist for fast lookups
/// Synced from database on startup
pub struct BlacklistCache {
    wallets: RwLock<HashSet<String>>,
}

impl BlacklistCache {
    pub fn new() -> Self {
        Self {
            wallets: RwLock::new(HashSet::new()),
        }
    }

    pub fn is_blacklisted(&self, wallet: &str) -> bool {
        let wallets = self.wallets.read().unwrap();
        wallets.contains(wallet)
    }

    pub fn add(&self, wallet: &str) {
        let mut wallets = self.wallets.write().unwrap();
        wallets.insert(wallet.to_string());
    }

    pub fn remove(&self, wallet: &str) {
        let mut wallets = self.wallets.write().unwrap();
        wallets.remove(wallet);
    }

    pub fn load_from_db(&self, wallets: Vec<String>) {
        let mut w = self.wallets.write().unwrap();
        for wallet in wallets {
            w.insert(wallet);
        }
        info!("Loaded {} wallets into blacklist cache", w.len());
    }

    pub fn count(&self) -> usize {
        let wallets = self.wallets.read().unwrap();
        wallets.len()
    }
}
