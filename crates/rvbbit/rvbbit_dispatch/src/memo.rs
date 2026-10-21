//! Process LRU in front of hutch, plus a per-query map.
//!
//! The query map is cleared from executor start, the same place EXPLAIN
//! stats are cleared. It does not survive the query. The LRU does, until
//! `semantic.memo_size` evicts it or `semantic.memo_ttl` expires it.
//! Neither map writes Postgres, so a read-only transaction and a hot
//! standby can answer from memory.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rvbbit_contract::ItemResponse;

#[derive(Clone)]
pub struct MemoHit {
    pub output: serde_json::Value,
    pub score: Option<f64>,
    stored: Instant,
}

struct Lru {
    cap: usize,
    ttl: Duration,
    next: u64,
    order: HashMap<String, u64>,
    values: HashMap<String, MemoHit>,
}

struct Maps {
    query: HashMap<String, MemoHit>,
    lru: Lru,
}

fn maps() -> &'static Mutex<Maps> {
    static MAPS: std::sync::OnceLock<Mutex<Maps>> = std::sync::OnceLock::new();
    MAPS.get_or_init(|| {
        Mutex::new(Maps {
            query: HashMap::new(),
            lru: Lru {
                cap: 1024,
                ttl: Duration::from_secs(3600),
                next: 1,
                order: HashMap::new(),
                values: HashMap::new(),
            },
        })
    })
}

static GUC: Mutex<Option<fn(&str) -> Option<String>>> = Mutex::new(None);

pub fn set_guc_reader(reader: fn(&str) -> Option<String>) {
    *GUC.lock().unwrap_or_else(|e| e.into_inner()) = Some(reader);
}

fn setting(name: &str, env: &str, default: &str) -> String {
    if let Some(reader) = *GUC.lock().unwrap_or_else(|e| e.into_inner()) {
        if let Some(value) = reader(name) {
            if !value.is_empty() {
                return value;
            }
        }
    }
    std::env::var(env)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// `on` (honor the catalog mode), `off`, or `refresh`.
pub fn cache_mode() -> String {
    setting("semantic.cache", "RVBBIT_SEMANTIC_CACHE", "on")
}

pub fn audit_payloads() -> bool {
    matches!(
        setting("semantic.audit_payloads", "RVBBIT_SEMANTIC_AUDIT", "off").as_str(),
        "on" | "true" | "1"
    )
}

fn memo_cap() -> usize {
    setting("semantic.memo_size", "RVBBIT_SEMANTIC_MEMO_SIZE", "1024")
        .parse()
        .unwrap_or(1024)
}

fn memo_ttl() -> Duration {
    let secs: u64 = setting("semantic.memo_ttl", "RVBBIT_SEMANTIC_MEMO_TTL", "3600")
        .parse()
        .unwrap_or(3600);
    Duration::from_secs(secs)
}

/// Drop the per-query map. The process LRU stays.
pub fn begin_query() {
    let mut maps = maps().lock().unwrap_or_else(|e| e.into_inner());
    maps.query.clear();
    maps.lru.cap = memo_cap().max(1);
    maps.lru.ttl = memo_ttl();
}

pub fn reset_for_tests() {
    let mut maps = maps().lock().unwrap_or_else(|e| e.into_inner());
    maps.query.clear();
    maps.lru.values.clear();
    maps.lru.order.clear();
}

pub fn recall(key: &str) -> Option<MemoHit> {
    let mut maps = maps().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(hit) = maps.query.get(key) {
        return Some(hit.clone());
    }
    let ttl = maps.lru.ttl;
    let stale = maps
        .lru
        .values
        .get(key)
        .is_some_and(|hit| !ttl.is_zero() && hit.stored.elapsed() > ttl);
    if stale {
        maps.lru.values.remove(key);
        maps.lru.order.remove(key);
        return None;
    }
    maps.lru.values.get(key).cloned()
}

pub fn remember(key: &str, output: serde_json::Value, score: Option<f64>) {
    if key.is_empty() {
        return;
    }
    let hit = MemoHit {
        output,
        score,
        stored: Instant::now(),
    };
    let mut maps = maps().lock().unwrap_or_else(|e| e.into_inner());
    maps.query.insert(key.to_string(), hit.clone());
    let cap = maps.lru.cap.max(1);
    let stamp = maps.lru.next;
    maps.lru.next = maps.lru.next.saturating_add(1);
    maps.lru.order.insert(key.to_string(), stamp);
    maps.lru.values.insert(key.to_string(), hit);
    while maps.lru.values.len() > cap {
        let oldest = maps
            .lru
            .order
            .iter()
            .min_by_key(|(_, stamp)| *stamp)
            .map(|(k, _)| k.clone());
        let Some(old) = oldest else { break };
        maps.lru.order.remove(&old);
        maps.lru.values.remove(&old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_map_clears_and_lru_evicts() {
        reset_for_tests();
        remember("a", serde_json::json!(true), Some(0.9));
        assert!(recall("a").is_some());
        begin_query();
        assert!(recall("a").is_some(), "lru survives the query");
        let mut maps = maps().lock().unwrap();
        maps.lru.cap = 1;
        drop(maps);
        remember("b", serde_json::json!(false), None);
        assert!(recall("b").is_some());
        assert!(recall("a").is_none(), "cap 1 evicts the older entry");
        reset_for_tests();
    }
}

pub fn hit_response(key: &str, hit: &MemoHit) -> ItemResponse {
    ItemResponse {
        key: key.to_string(),
        value: Some(hit.output.clone()),
        score: hit.score,
        scores: None,
        calls: None,
        error: None,
    }
}
