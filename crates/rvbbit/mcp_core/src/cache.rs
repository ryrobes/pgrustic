use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Process cache for read-only tool results. The key is a caller-supplied
/// hash of server, tool, and canonical arguments.
#[derive(Debug, Default)]
pub struct ToolCache {
    rows: HashMap<String, (String, Instant)>,
}

impl ToolCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&mut self, key: &str, now: Instant) -> Option<String> {
        match self.rows.get(key) {
            Some((value, exp)) if *exp > now => Some(value.clone()),
            Some(_) => {
                self.rows.remove(key);
                None
            }
            None => None,
        }
    }

    pub fn put(&mut self, key: String, value: String, ttl: Duration, now: Instant) {
        if ttl.is_zero() {
            return;
        }
        self.rows.insert(key, (value, now + ttl));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_entries_are_misses() {
        let mut cache = ToolCache::new();
        let now = Instant::now();
        cache.put("k".into(), "v".into(), Duration::from_secs(10), now);
        assert_eq!(cache.get("k", now + Duration::from_secs(5)).as_deref(), Some("v"));
        assert!(cache.get("k", now + Duration::from_secs(11)).is_none());
    }
}
