use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::model::source::Source;

#[derive(Debug, Clone)]
pub struct SourceCache {
    max_size: usize,
    order: VecDeque<String>,
    values: HashMap<String, Arc<Vec<Source>>>,
}

impl SourceCache {
    pub fn new(max_size: usize) -> Self {
        Self {
            max_size: max_size.max(1),
            order: VecDeque::new(),
            values: HashMap::new(),
        }
    }

    pub fn set(&mut self, session_id: String, sources: Arc<Vec<Source>>) {
        if self.values.contains_key(&session_id) {
            self.order.retain(|existing| existing != &session_id);
        }
        self.order.push_back(session_id.clone());
        self.values.insert(session_id, sources);

        while self.order.len() > self.max_size {
            if let Some(oldest) = self.order.pop_front() {
                self.values.remove(&oldest);
            }
        }
    }

    pub fn get(&mut self, session_id: &str) -> Option<Arc<Vec<Source>>> {
        let sources = self.values.get(session_id).cloned()?;
        self.order.retain(|existing| existing != session_id);
        self.order.push_back(session_id.to_string());
        Some(sources)
    }
}

/// A LRU cache of search-result lists keyed by query + filter signature, with
/// an entry time-to-live. Shared by supplemental-source fan-out so repeated
/// queries inside the TTL don't re-hit free engines (DuckDuckGo/Bing) or burn
/// paid-provider quota. `ttl == 0` disables the cache entirely (tests default
/// it off so no state leaks between test cases).
#[derive(Debug, Clone)]
struct QueryEntry {
    sources: Vec<Source>,
    origin: &'static str,
    fetched_at: Instant,
}

#[derive(Debug, Clone)]
pub struct QueryResultCache {
    max_size: usize,
    ttl: Duration,
    order: VecDeque<String>,
    values: HashMap<String, QueryEntry>,
}

impl QueryResultCache {
    pub fn new(max_size: usize, ttl: Duration) -> Self {
        Self {
            max_size: max_size.max(1),
            ttl,
            order: VecDeque::new(),
            values: HashMap::new(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        !self.ttl.is_zero()
    }

    /// Store a result list under `key`. Entries older than `ttl` (and any
    /// over the size cap) are evicted on write.
    pub fn set(&mut self, key: String, sources: Vec<Source>, origin: &'static str) {
        if !self.is_enabled() {
            return;
        }
        self.evict_expired();
        if self.values.contains_key(&key) {
            self.order.retain(|existing| existing != &key);
        }
        self.order.push_back(key.clone());
        self.values.insert(
            key,
            QueryEntry {
                sources,
                origin,
                fetched_at: Instant::now(),
            },
        );
        while self.order.len() > self.max_size {
            if let Some(oldest) = self.order.pop_front() {
                self.values.remove(&oldest);
            }
        }
    }

    /// Return a live (unexpired) entry and refresh its LRU position.
    pub fn get(&mut self, key: &str) -> Option<(Vec<Source>, &'static str)> {
        if !self.is_enabled() {
            return None;
        }
        self.evict_expired();
        let entry = self.values.get(key)?;
        if entry.fetched_at.elapsed() > self.ttl {
            self.values.remove(key);
            self.order.retain(|existing| existing != key);
            return None;
        }
        let sources = entry.sources.clone();
        let origin = entry.origin;
        self.order.retain(|existing| existing != key);
        self.order.push_back(key.to_string());
        Some((sources, origin))
    }

    fn evict_expired(&mut self) {
        let ttl = self.ttl;
        let expired: Vec<String> = self
            .values
            .iter()
            .filter(|(_, entry)| entry.fetched_at.elapsed() > ttl)
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired {
            self.values.remove(&key);
            self.order.retain(|existing| existing != &key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_cache_stores_nothing() {
        let mut cache = QueryResultCache::new(10, Duration::ZERO);
        cache.set(
            "q".to_string(),
            vec![Source::new("https://a", "tavily")],
            "tavily",
        );
        assert!(cache.get("q").is_none());
    }

    #[test]
    fn cache_returns_live_entry_with_origin() {
        let mut cache = QueryResultCache::new(10, Duration::from_secs(60));
        let sources = vec![Source::new("https://a", "bing")];
        cache.set("q".to_string(), sources.clone(), "bing");
        let (restored, origin) = cache.get("q").unwrap();
        assert_eq!(restored, sources);
        assert_eq!(origin, "bing");
    }

    #[test]
    fn cache_evicts_over_capacity_lru_style() {
        let mut cache = QueryResultCache::new(2, Duration::from_secs(60));
        for key in ["a", "b", "c"] {
            cache.set(key.to_string(), Vec::new(), "tavily");
        }
        assert!(cache.get("a").is_none(), "oldest entry evicted");
        assert!(cache.get("b").is_some());
        assert!(cache.get("c").is_some());
    }

    #[test]
    fn cache_get_refreshes_recency_position() {
        let mut cache = QueryResultCache::new(2, Duration::from_secs(60));
        cache.set("a".to_string(), Vec::new(), "tavily");
        cache.set("b".to_string(), Vec::new(), "tavily");
        let _ = cache.get("a"); // touches "a", making "b" the LRU tail
        cache.set("c".to_string(), Vec::new(), "tavily");
        assert!(cache.get("a").is_some(), "touched entry survives");
        assert!(cache.get("b").is_none(), "least-recently used evicted");
    }
}
