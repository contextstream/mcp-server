//! Generic bounded TTL cache for tool-call results.
//!
//! Used by search-like tools to short-circuit repeat calls with the same
//! key within a warm window, saving an API round-trip and letting the
//! caller emit a compact `[*_CACHED]` marker instead of re-rendering full
//! results.
//!
//! Design notes:
//! - Keys are opaque strings — each caller builds a canonical key from
//!   the relevant inputs (scope, query, mode, filters).
//! - TTL and max-entry budgets are set per-cache instance.
//! - Eviction on put: if over budget, the oldest entry (by insertion
//!   timestamp) is removed. Not a strict LRU — access does not refresh
//!   the timestamp — but adequate for a sub-minute warm window.
//! - Lock is a single `Mutex` since puts/gets are fast and rare enough
//!   that contention isn't the bottleneck.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Return whether a rendered tool result fits within a caller-selected cache
/// entry budget. This keeps larger shared-cache capacities from multiplying a
/// single unexpectedly large structured response across process memory.
pub fn rendered_entry_fits(text: &str, structured: &serde_json::Value, max_bytes: usize) -> bool {
    let Some(structured_budget) = max_bytes.checked_sub(text.len()) else {
        return false;
    };
    serde_json::to_vec(structured)
        .map(|encoded| encoded.len() <= structured_budget)
        .unwrap_or(false)
}

struct Entry<V> {
    value: V,
    stored_at: Instant,
    partition: Option<String>,
}

pub struct ResultCache<V> {
    ttl: Duration,
    max_entries: usize,
    entries: Mutex<HashMap<String, Entry<V>>>,
}

impl<V: Clone> ResultCache<V> {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            ttl,
            max_entries,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Look up a cached value by key. Returns None when the entry is
    /// missing or has expired. Expired entries are lazily evicted here.
    pub fn get(&self, key: &str) -> Option<V> {
        let mut guard = self.entries.lock().ok()?;
        let entry = guard.get(key)?;
        if entry.stored_at.elapsed() > self.ttl {
            guard.remove(key);
            return None;
        }
        Some(entry.value.clone())
    }

    /// Insert or replace an entry. If the cache is over `max_entries`
    /// after insert, evict the oldest entry by `stored_at`.
    #[cfg(test)]
    pub fn put(&self, key: String, value: V) {
        self.put_inner(None, key, value, None);
    }

    /// Insert a caller-partitioned entry. `max_entries_per_partition` prevents
    /// one busy caller from evicting every other caller's warm results while
    /// the cache's global `max_entries` remains the hard process memory bound.
    pub fn put_partitioned(
        &self,
        partition: &str,
        key: String,
        value: V,
        max_entries_per_partition: usize,
    ) {
        self.put_inner(
            Some(partition.to_string()),
            key,
            value,
            Some(max_entries_per_partition),
        );
    }

    fn put_inner(
        &self,
        partition: Option<String>,
        key: String,
        value: V,
        max_entries_per_partition: Option<usize>,
    ) {
        let Ok(mut guard) = self.entries.lock() else {
            return;
        };
        guard.retain(|_, entry| entry.stored_at.elapsed() <= self.ttl);
        guard.insert(
            key,
            Entry {
                value,
                stored_at: Instant::now(),
                partition: partition.clone(),
            },
        );

        if let (Some(partition), Some(max_entries_per_partition)) =
            (partition.as_deref(), max_entries_per_partition)
        {
            while guard
                .values()
                .filter(|entry| entry.partition.as_deref() == Some(partition))
                .count()
                > max_entries_per_partition
            {
                let Some(oldest_key) = guard
                    .iter()
                    .filter(|(_, entry)| entry.partition.as_deref() == Some(partition))
                    .min_by_key(|(_, entry)| entry.stored_at)
                    .map(|(key, _)| key.clone())
                else {
                    break;
                };
                guard.remove(&oldest_key);
            }
        }

        while guard.len() > self.max_entries {
            if let Some(oldest_key) = guard
                .iter()
                .min_by_key(|(_, e)| e.stored_at)
                .map(|(k, _)| k.clone())
            {
                guard.remove(&oldest_key);
            } else {
                break;
            }
        }
    }

    /// Drop every cached entry. Useful on scope switches or explicit
    /// invalidation events.
    #[allow(dead_code)]
    pub fn clear(&self) {
        if let Ok(mut guard) = self.entries.lock() {
            guard.clear();
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.lock().map(|g| g.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn returns_cached_value_within_ttl() {
        let cache: ResultCache<String> = ResultCache::new(Duration::from_secs(5), 4);
        cache.put("k".to_string(), "v".to_string());
        assert_eq!(cache.get("k"), Some("v".to_string()));
    }

    #[test]
    fn returns_none_after_ttl_expires() {
        let cache: ResultCache<String> = ResultCache::new(Duration::from_millis(50), 4);
        cache.put("k".to_string(), "v".to_string());
        sleep(Duration::from_millis(80));
        assert_eq!(cache.get("k"), None);
    }

    #[test]
    fn evicts_oldest_when_over_capacity() {
        let cache: ResultCache<String> = ResultCache::new(Duration::from_secs(5), 2);
        cache.put("a".to_string(), "1".to_string());
        sleep(Duration::from_millis(2));
        cache.put("b".to_string(), "2".to_string());
        sleep(Duration::from_millis(2));
        cache.put("c".to_string(), "3".to_string());
        // "a" was oldest; should be evicted.
        assert_eq!(cache.len(), 2);
        assert!(cache.get("a").is_none());
        assert!(cache.get("b").is_some());
        assert!(cache.get("c").is_some());
    }

    #[test]
    fn replacing_an_existing_key_does_not_grow_past_budget() {
        let cache: ResultCache<String> = ResultCache::new(Duration::from_secs(5), 2);
        cache.put("a".to_string(), "1".to_string());
        cache.put("a".to_string(), "1'".to_string());
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("a"), Some("1'".to_string()));
    }

    #[test]
    fn clear_drops_all_entries() {
        let cache: ResultCache<String> = ResultCache::new(Duration::from_secs(5), 4);
        cache.put("a".to_string(), "1".to_string());
        cache.put("b".to_string(), "2".to_string());
        cache.clear();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn partition_quota_prevents_noisy_neighbor_eviction() {
        let cache: ResultCache<String> = ResultCache::new(Duration::from_secs(5), 4);
        cache.put_partitioned("caller-a", "a1".to_string(), "1".to_string(), 2);
        sleep(Duration::from_millis(2));
        cache.put_partitioned("caller-a", "a2".to_string(), "2".to_string(), 2);
        sleep(Duration::from_millis(2));
        cache.put_partitioned("caller-a", "a3".to_string(), "3".to_string(), 2);
        assert!(cache.get("a1").is_none());
        assert!(cache.get("a2").is_some());
        assert!(cache.get("a3").is_some());

        cache.put_partitioned("caller-b", "b1".to_string(), "1".to_string(), 2);
        sleep(Duration::from_millis(2));
        cache.put_partitioned("caller-b", "b2".to_string(), "2".to_string(), 2);
        sleep(Duration::from_millis(2));
        cache.put_partitioned("caller-b", "b3".to_string(), "3".to_string(), 2);

        assert_eq!(cache.len(), 4);
        assert!(cache.get("a2").is_some());
        assert!(cache.get("a3").is_some());
        assert!(cache.get("b1").is_none());
        assert!(cache.get("b2").is_some());
        assert!(cache.get("b3").is_some());
    }

    #[test]
    fn rendered_entry_budget_counts_text_and_structured_content() {
        let structured = serde_json::json!({"result": "bounded"});
        let encoded_len = serde_json::to_vec(&structured).unwrap().len();
        let exact_budget = "text".len() + encoded_len;

        assert!(rendered_entry_fits("text", &structured, exact_budget));
        assert!(!rendered_entry_fits("text", &structured, exact_budget - 1));
        assert!(!rendered_entry_fits(
            "oversized text",
            &structured,
            "oversized".len()
        ));
    }
}
