//! In-memory cache with TTL support.

use dashmap::DashMap;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Cache entry with expiration.
#[derive(Debug, Clone)]
struct CacheEntry<V> {
    value: V,
    expires_at: Instant,
}

impl<V> CacheEntry<V> {
    fn new(value: V, ttl: Duration) -> Self {
        Self {
            value,
            expires_at: Instant::now() + ttl,
        }
    }

    fn is_expired(&self) -> bool {
        Instant::now() > self.expires_at
    }
}

/// Thread-safe cache with TTL support.
pub struct Cache<K, V> {
    entries: DashMap<K, CacheEntry<V>>,
    default_ttl: Duration,
}

impl<K: Eq + Hash, V> std::fmt::Debug for Cache<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cache")
            .field("entries_count", &self.entries.len())
            .field("default_ttl", &self.default_ttl)
            .finish()
    }
}

impl<K, V> Cache<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    /// Create a new cache with the specified default TTL.
    pub fn new(default_ttl: Duration) -> Self {
        Self {
            entries: DashMap::new(),
            default_ttl,
        }
    }

    /// Get a value from the cache.
    pub fn get(&self, key: &K) -> Option<V> {
        let entry = self.entries.get(key)?;
        if entry.is_expired() {
            drop(entry);
            self.entries.remove(key);
            return None;
        }
        Some(entry.value.clone())
    }

    /// Set a value with the default TTL.
    pub fn set(&self, key: K, value: V) {
        self.set_with_ttl(key, value, self.default_ttl);
    }

    /// Set a value with a custom TTL.
    pub fn set_with_ttl(&self, key: K, value: V, ttl: Duration) {
        self.entries.insert(key, CacheEntry::new(value, ttl));
    }

    /// Remove a value from the cache.
    pub fn remove(&self, key: &K) -> Option<V> {
        self.entries.remove(key).map(|(_, e)| e.value)
    }

    /// Remove all values whose keys match the predicate.
    pub fn remove_matching<F>(&self, mut predicate: F) -> usize
    where
        F: FnMut(&K) -> bool,
    {
        let keys: Vec<K> = self
            .entries
            .iter()
            .filter_map(|entry| {
                if entry.is_expired() || predicate(entry.key()) {
                    Some(entry.key().clone())
                } else {
                    None
                }
            })
            .collect();

        let removed = keys.len();
        for key in keys {
            self.entries.remove(&key);
        }
        removed
    }

    /// Check if a key exists and is not expired.
    pub fn contains(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    /// Clear all entries from the cache.
    pub fn clear(&self) {
        self.entries.clear();
    }

    /// Remove expired entries.
    pub fn cleanup(&self) {
        self.entries.retain(|_, entry| !entry.is_expired());
    }

    /// Get the number of entries (including expired).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Common cache key patterns.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CacheKey {
    CreditBalance,
    AccountContext,
    Workspace(uuid::Uuid),
    Project(uuid::Uuid),
    WorkspaceOverview(uuid::Uuid),
    ProjectOverview(uuid::Uuid),
    IntegrationStatus(String),
    Custom(String),
}

impl CacheKey {
    pub fn credit_balance() -> Self {
        Self::CreditBalance
    }

    pub fn workspace(id: uuid::Uuid) -> Self {
        Self::Workspace(id)
    }

    pub fn project(id: uuid::Uuid) -> Self {
        Self::Project(id)
    }

    pub fn integration_status(name: impl Into<String>) -> Self {
        Self::IntegrationStatus(name.into())
    }

    pub fn custom(key: impl Into<String>) -> Self {
        Self::Custom(key.into())
    }
}

/// Common TTL values.
pub mod ttl {
    use std::time::Duration;

    pub const CREDIT_BALANCE: Duration = Duration::from_secs(60);
    pub const INTEGRATION_STATUS: Duration = Duration::from_secs(5 * 60);
    pub const RULES_NOTICE: Duration = Duration::from_secs(10 * 60);
    pub const WORKSPACE: Duration = Duration::from_secs(5 * 60);
    pub const PROJECT: Duration = Duration::from_secs(5 * 60);
    pub const ACCOUNT_CONTEXT: Duration = Duration::from_secs(60);
}

/// Shared global cache instance.
pub type GlobalCache = Arc<Cache<CacheKey, serde_json::Value>>;

/// Create a new global cache.
pub fn create_global_cache() -> GlobalCache {
    Arc::new(Cache::new(Duration::from_secs(5 * 60)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_basic() {
        let cache: Cache<String, i32> = Cache::new(Duration::from_secs(60));

        cache.set("key".to_string(), 42);
        assert_eq!(cache.get(&"key".to_string()), Some(42));

        cache.remove(&"key".to_string());
        assert_eq!(cache.get(&"key".to_string()), None);
    }

    #[test]
    fn test_cache_expiration() {
        let cache: Cache<String, i32> = Cache::new(Duration::from_millis(1));

        cache.set("key".to_string(), 42);
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(cache.get(&"key".to_string()), None);
    }

    #[test]
    fn test_cache_remove_matching() {
        let cache: Cache<String, i32> = Cache::new(Duration::from_secs(60));

        cache.set("docs:list:one".to_string(), 1);
        cache.set("docs:list:two".to_string(), 2);
        cache.set("search:one".to_string(), 3);

        let removed = cache.remove_matching(|key| key.starts_with("docs:list:"));

        assert_eq!(removed, 2);
        assert_eq!(cache.get(&"docs:list:one".to_string()), None);
        assert_eq!(cache.get(&"docs:list:two".to_string()), None);
        assert_eq!(cache.get(&"search:one".to_string()), Some(3));
    }
}
