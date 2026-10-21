//! File cache for `HUTCH_CACHE=persistent`.
//!
//! One JSON file per storage key. The storage key already mixes the
//! tenant, so a restart of this process serves the same tenant's hits
//! and never another tenant's. Memory mode does not touch the directory.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

pub fn save(dir: &Path, storage: &str, value: &Value, score: Option<f64>, ttl_ms: Option<u128>) {
    let _ = fs::create_dir_all(dir);
    let mut body = serde_json::json!({
        "value": value,
        "score": score,
    });
    if let Some(ms) = ttl_ms {
        if let Some(n) = serde_json::Number::from_u128(now_ms().saturating_add(ms)) {
            body["expires_ms"] = Value::Number(n);
        }
    }
    let path = dir.join(format!("{storage}.json"));
    if let Ok(text) = serde_json::to_string(&body) {
        let _ = fs::write(path, text);
    }
}

pub fn load(dir: &Path, storage: &str) -> Option<(Value, Option<f64>)> {
    let path = dir.join(format!("{storage}.json"));
    let text = fs::read_to_string(&path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    if let Some(exp) = v.get("expires_ms").and_then(|n| n.as_u64()) {
        if now_ms() >= u128::from(exp) {
            let _ = fs::remove_file(&path);
            return None;
        }
    }
    let value = v.get("value")?.clone();
    let score = v.get("score").and_then(|n| n.as_f64()).filter(|n| n.is_finite());
    Some((value, score))
}

pub fn remove(dir: &Path, storage: &str) {
    let _ = fs::remove_file(dir.join(format!("{storage}.json")));
}

pub fn clear_dir(dir: &Path) {
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
