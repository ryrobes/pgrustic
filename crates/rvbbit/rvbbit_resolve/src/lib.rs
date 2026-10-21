//! Process-local view of the rvbbit catalog.
//!
//! The parser and the executor cannot depend on SPI (that crate depends on
//! them). `seams_init` installs [`set_ensure`] and [`set_execute`] pointing
//! at `rvbbit_catalog`. Until then, both hooks are no-ops and MEANS keeps
//! the oid-9020 fallback.

use std::collections::HashMap;
use std::sync::Mutex;

use rvbbit_oids::{BATCH_ROWS, CONCURRENCY, MEANS_OID};
use types_error::PgResult;

#[derive(Clone, Debug)]
pub struct Endpoint {
    pub base_url: String,
    pub auth_env: Option<String>,
    pub batch_rows: usize,
    pub max_concurrent: usize,
    pub timeout_ms: u64,
    pub retries: u32,
    pub model: String,
    pub temperature: f64,
    pub max_tokens: Option<u32>,
    pub fn_name: String,
    pub fn_version: i32,
    pub system_prompt: String,
    pub user_prompt: String,
    pub return_type: String,
    /// Catalog `cache_mode`: `memoize`, `refresh`, or `off`.
    pub cache_mode: String,
    /// Declared classify labels. Empty for MEANS, score, and extract.
    pub labels: Vec<String>,
    /// Parallel to [`Self::labels`]. Empty string means the default sentence.
    pub label_descriptions: Vec<String>,
    /// Noul cutoff for this function or operator. `None` means the hutch
    /// server default (`HUTCH_DECISION_THRESHOLD`, 0.5).
    pub threshold: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct Resolved {
    pub word: String,
    pub func_oid: u32,
    pub endpoint: Endpoint,
}

static ENSURE: Mutex<Option<fn() -> PgResult<()>>> = Mutex::new(None);
static EXECUTE: Mutex<Option<fn(&str) -> PgResult<()>>> = Mutex::new(None);
static CACHE: Mutex<Cache> = Mutex::new(Cache {
    by_word: None,
    by_oid: None,
});

struct Cache {
    by_word: Option<HashMap<String, Resolved>>,
    by_oid: Option<HashMap<u32, Resolved>>,
}

fn lock_hooks<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn set_ensure(hook: fn() -> PgResult<()>) {
    *lock_hooks(&ENSURE) = Some(hook);
}

pub fn set_execute(hook: fn(&str) -> PgResult<()>) {
    *lock_hooks(&EXECUTE) = Some(hook);
}

/// Create the catalog schema if this statement needs it, then refresh the
/// process cache from `rvbbit.*`. No-op until [`set_ensure`].
pub fn ensure() -> PgResult<()> {
    if let Some(hook) = *lock_hooks(&ENSURE) {
        hook()?;
    }
    Ok(())
}

/// Run one rvbbit DDL statement. No-op-error until [`set_execute`].
pub fn execute(sql: &str) -> PgResult<()> {
    match *lock_hooks(&EXECUTE) {
        Some(hook) => hook(sql),
        None => Err(Box::new(types_error::PgError::error(
            "rvbbit catalog hook is not installed".to_string(),
        ))),
    }
}

pub fn install(entries: Vec<Resolved>) {
    let mut by_word = HashMap::new();
    let mut by_oid = HashMap::new();
    for e in entries {
        by_oid.insert(e.func_oid, e.clone());
        by_word.insert(e.word.to_ascii_lowercase(), e);
    }
    let mut g = lock_hooks(&CACHE);
    g.by_word = Some(by_word);
    g.by_oid = Some(by_oid);
}

pub fn func_oid(word: &str) -> Option<u32> {
    let g = lock_hooks(&CACHE);
    g.by_word.as_ref()?.get(&word.to_ascii_lowercase()).map(|e| e.func_oid)
}

pub fn is_semantic_func(oid: u32) -> bool {
    if oid == MEANS_OID {
        return true;
    }
    let g = lock_hooks(&CACHE);
    g.by_oid.as_ref().is_some_and(|m| m.contains_key(&oid))
}

pub fn endpoint_for(oid: u32) -> Endpoint {
    let found = {
        let g = lock_hooks(&CACHE);
        g.by_oid.as_ref().and_then(|m| m.get(&oid)).map(|e| e.endpoint.clone())
    };
    found.unwrap_or_else(default_endpoint)
}

pub fn batch_rows() -> usize {
    word_endpoint("means")
        .map(|e| e.batch_rows)
        .unwrap_or(BATCH_ROWS)
}

pub fn max_concurrent() -> usize {
    word_endpoint("means")
        .map(|e| e.max_concurrent)
        .unwrap_or(CONCURRENCY)
}

fn word_endpoint(word: &str) -> Option<Endpoint> {
    let g = lock_hooks(&CACHE);
    Some(g.by_word.as_ref()?.get(word)?.endpoint.clone())
}

/// `RVBBIT_HUTCH_URL`, with a trailing `/v1/means` or `/v1/predict` removed
/// so the spike's old setting still names the same server.
pub fn default_base_url() -> String {
    let raw = std::env::var("RVBBIT_HUTCH_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8099".to_string());
    let trimmed = raw.trim().trim_end_matches('/');
    let trimmed = trimmed
        .strip_suffix("/v1/means")
        .or_else(|| trimmed.strip_suffix("/v1/predict"))
        .unwrap_or(trimmed);
    trimmed.trim_end_matches('/').to_string()
}

pub fn default_endpoint() -> Endpoint {
    let timeout_ms = std::env::var("RVBBIT_HUTCH_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000);
    Endpoint {
        base_url: default_base_url(),
        auth_env: None,
        batch_rows: BATCH_ROWS,
        max_concurrent: CONCURRENCY,
        timeout_ms,
        retries: 5,
        model: "mock".into(),
        temperature: 0.0,
        max_tokens: Some(16),
        fn_name: "means".into(),
        fn_version: 1,
        system_prompt: String::new(),
        user_prompt: String::new(),
        return_type: "boolean".into(),
        // Pre-catalog fallback. Catalog rows send their own mode.
        cache_mode: "off".into(),
        labels: Vec::new(),
        label_descriptions: Vec::new(),
        threshold: None,
    }
}
