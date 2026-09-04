use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info};

/// Generic cache with TTL (time-to-live) support.
/// Used to store filter results, wallet history, and other data
/// so repeated lookups are near-instant instead of making RPC calls.
pub struct Cache<T: Clone> {
    store: RwLock<HashMap<String, CacheEntry<T>>>,
    default_ttl: Duration,
}

struct CacheEntry<T: Clone> {
    value: T,
    inserted_at: Instant,
    ttl: Duration,
}

impl<T: Clone> Cache<T> {
    pub fn new(default_ttl: Duration) -> Self {
        Cache {
            store: RwLock::new(HashMap::new()),
            default_ttl,
        }
    }

    pub async fn get(&self, key: &str) -> Option<T> {
        let store = self.store.read().await;
        if let Some(entry) = store.get(key) {
            if entry.inserted_at.elapsed() < entry.ttl {
                debug!("Cache HIT for key: {}", key);
                return Some(entry.value.clone());
            }
            debug!("Cache EXPIRED for key: {}", key);
        }
        debug!("Cache MISS for key: {}", key);
        None
    }

    pub async fn set(&self, key: &str, value: T) {
        self.set_with_ttl(key, value, self.default_ttl).await;
    }

    pub async fn set_with_ttl(&self, key: &str, value: T, ttl: Duration) {
        let mut store = self.store.write().await;
        store.insert(
            key.to_string(),
            CacheEntry {
                value,
                inserted_at: Instant::now(),
                ttl,
            },
        );
        debug!("Cache SET for key: {} (TTL: {:?})", key, ttl);
    }

    pub async fn remove(&self, key: &str) {
        let mut store = self.store.write().await;
        store.remove(key);
    }

    pub async fn contains(&self, key: &str) -> bool {
        let store = self.store.read().await;
        if let Some(entry) = store.get(key) {
            return entry.inserted_at.elapsed() < entry.ttl;
        }
        false
    }

    pub async fn cleanup_expired(&self) {
        let mut store = self.store.write().await;
        let before_count = store.len();
        store.retain(|_, entry| entry.inserted_at.elapsed() < entry.ttl);
        let removed = before_count - store.len();
        if removed > 0 {
            info!("Cache cleanup: removed {} expired entries", removed);
        }
    }

    pub async fn len(&self) -> usize {
        let store = self.store.read().await;
        store.len()
    }

    pub async fn clear(&self) {
        let mut store = self.store.write().await;
        store.clear();
    }
}
