//! Deterministic stand-in for the hutch semantic service.
//!
//! Speaks `rvbbit-hutch-v1` (`rvbbit_contract`):
//! `POST /v1/{predict,classify,extract,embed,rerank,decide}` and
//! `POST /v1/cache/lookup`. Predict is a case-insensitive substring
//! (`inputs.input` contains `inputs.criterion` or the item's `criterion`).
//! `/stats` counts operation posts only, not cache lookups.
//!
//! Environment:
//! - `HUTCH_LATENCY_MS` — sleep before each operation response (default 0)
//! - `HUTCH_FAIL_EVERY` — HTTP 429 with `Retry-After: 0` every Nth operation
//! - `HUTCH_TIMEOUT_EVERY` — sleep 60s every Nth operation
//! - `HUTCH_EXPECT_AUTH` — if set, require `Authorization: Bearer <value>`

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use rvbbit_contract::{
    BatchRequest, BatchResponse, CacheLookupRequest, CacheLookupResponse, CallMeter, ItemError,
    ItemResponse, Operation, CONTRACT_VERSION,
};

#[derive(Clone, Debug)]
pub struct Config {
    pub latency: Duration,
    pub fail_every: u64,
    pub timeout_every: u64,
    pub expect_auth: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        let latency_ms: u64 = std::env::var("HUTCH_LATENCY_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let fail_every = std::env::var("HUTCH_FAIL_EVERY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let timeout_every = std::env::var("HUTCH_TIMEOUT_EVERY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let expect_auth = std::env::var("HUTCH_EXPECT_AUTH").ok().filter(|s| !s.is_empty());
        Self {
            latency: Duration::from_millis(latency_ms),
            fail_every,
            timeout_every,
            expect_auth,
        }
    }
}

pub struct Stats {
    pub calls: AtomicU64,
    pub rows: AtomicU64,
    pub max_batch: AtomicU64,
    pub batches: Mutex<Vec<usize>>,
}

impl Stats {
    fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
            rows: AtomicU64::new(0),
            max_batch: AtomicU64::new(0),
            batches: Mutex::new(Vec::new()),
        }
    }

    fn record(&self, n: usize) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.rows.fetch_add(n as u64, Ordering::Relaxed);
        self.max_batch.fetch_max(n as u64, Ordering::Relaxed);
        self.batches.lock().unwrap_or_else(|e| e.into_inner()).push(n);
    }

    fn reset(&self) {
        self.calls.store(0, Ordering::Relaxed);
        self.rows.store(0, Ordering::Relaxed);
        self.max_batch.store(0, Ordering::Relaxed);
        self.batches.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn json(&self) -> String {
        let batches = self.batches.lock().unwrap_or_else(|e| e.into_inner()).clone();
        serde_json::json!({
            "calls": self.calls.load(Ordering::Relaxed),
            "rows": self.rows.load(Ordering::Relaxed),
            "max_batch": self.max_batch.load(Ordering::Relaxed),
            "batches": batches,
        })
        .to_string()
    }
}

/// Case-insensitive substring. This is the whole semantic model of the mock.
pub fn matches(input: &str, criterion: &str) -> bool {
    if criterion.is_empty() {
        return false;
    }
    let input = input.to_ascii_lowercase();
    let criterion = criterion.to_ascii_lowercase();
    input.contains(&criterion)
}

pub fn serve(addr: &str, cfg: Config) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    serve_listener(listener, cfg)
}

pub fn serve_listener(listener: TcpListener, cfg: Config) -> std::io::Result<()> {
    let stats = std::sync::Arc::new(Stats::new());
    for conn in listener.incoming() {
        let conn = conn?;
        let cfg = cfg.clone();
        let stats = std::sync::Arc::clone(&stats);
        thread::spawn(move || {
            let _ = handle(conn, &cfg, &stats);
        });
    }
    Ok(())
}

fn handle(mut stream: TcpStream, cfg: &Config, stats: &Stats) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => return,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(i) = find_header_end(&buf) {
                    header_end = i;
                    break;
                }
                if buf.len() > 8 * 1024 * 1024 {
                    return;
                }
            }
            Err(_) => return,
        }
    }
    let header = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut content_length = 0usize;
    let mut is_post = false;
    let mut path = String::new();
    let mut auth = String::new();
    for (i, line) in header.lines().enumerate() {
        if i == 0 {
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("");
            path = parts.next().unwrap_or("/").to_string();
            is_post = method.eq_ignore_ascii_case("POST");
        } else if let Some(v) = line.split_once(':') {
            if v.0.eq_ignore_ascii_case("content-length") {
                content_length = v.1.trim().parse().unwrap_or(0);
            } else if v.0.eq_ignore_ascii_case("authorization") {
                auth = v.1.trim().to_string();
            }
        }
    }
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
        }
    }
    body.truncate(content_length);

    let operation = is_operation(&path);
    if !cfg.latency.is_zero() && operation {
        thread::sleep(cfg.latency);
    }

    let (status, extra, payload) = route(is_post, &path, &auth, &body, cfg, stats);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
    let _ = stream.write_all(response.as_bytes());
}

fn is_operation(path: &str) -> bool {
    matches!(
        path_only(path),
        "/v1/predict" | "/v1/classify" | "/v1/extract" | "/v1/embed" | "/v1/rerank" | "/v1/decide"
            | "/v1/means"
    )
}

fn path_only(path: &str) -> &str {
    path.split('?').next().unwrap_or(path)
}

fn route(
    is_post: bool,
    path: &str,
    auth: &str,
    body: &[u8],
    cfg: &Config,
    stats: &Stats,
) -> (u16, String, String) {
    let path = path_only(path);
    if path.starts_with("/stats") {
        return (200, String::new(), stats.json());
    }
    if path.starts_with("/reset") {
        stats.reset();
        return (200, String::new(), "{\"ok\":true}".to_string());
    }
    if let Some(expect) = &cfg.expect_auth {
        let want = format!("Bearer {expect}");
        if auth != want {
            return (401, String::new(), "{\"error\":\"unauthorized\"}".to_string());
        }
    }
    if is_post && path == "/v1/cache/lookup" {
        let req: CacheLookupRequest = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return (400, String::new(), format!("{{\"error\":\"{e}\"}}")),
        };
        let resp = CacheLookupResponse {
            hits: Vec::new(),
            misses: req.keys,
        };
        return (
            200,
            String::new(),
            serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into()),
        );
    }
    if is_post && is_operation(path) {
        let seen = stats.calls.load(Ordering::Relaxed);
        if cfg.timeout_every > 0 && seen % cfg.timeout_every == 0 {
            stats.record(0);
            thread::sleep(Duration::from_secs(60));
            return (504, String::new(), "{\"error\":\"injected timeout\"}".to_string());
        }
        if cfg.fail_every > 0 && seen % cfg.fail_every == 0 {
            stats.record(0);
            return (
                429,
                "Retry-After: 0\r\n".to_string(),
                "{\"error\":\"injected rate limit\"}".to_string(),
            );
        }
        let req: BatchRequest = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return (400, String::new(), format!("{{\"error\":\"{e}\"}}")),
        };
        stats.record(req.items.len());
        let op = operation_of(path).unwrap_or(req.operation);
        let outputs = req
            .items
            .iter()
            .map(|item| {
                let (value, score) = mock_scored(op, item);
                ItemResponse {
                    key: item.key.clone(),
                    value: Some(value),
                    score,
                    scores: None,
                    calls: Some(vec![mock_meter(op)]),
                    error: None,
                    identical_takes: None,
                }
            })
            .collect();
        let resp = BatchResponse {
            contract: CONTRACT_VERSION.to_string(),
            outputs,
        };
        return (
            200,
            String::new(),
            serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into()),
        );
    }
    (404, String::new(), "{\"error\":\"not found\"}".to_string())
}

/// One miss at the jev seed price so EXPLAIN ANALYZE can show USD.
/// 300 billed tokens at $0.042 per million.
fn mock_meter(op: Operation) -> CallMeter {
    let name = match op {
        Operation::Predict => "predict",
        Operation::Classify => "classify",
        Operation::Extract => "extract",
        Operation::Embed => "embed",
        Operation::Rerank => "rerank",
        Operation::Decide => "decide",
    };
    CallMeter {
        op: name.into(),
        model: "jev".into(),
        tokens_in: Some(300),
        tokens_out: None,
        cost_usd: Some(300.0 * 0.042 / 1_000_000.0),
        cost_saved_usd: None,
        provider_latency_ms: Some(1),
        latency_ms: 1,
        cache: "miss".into(),
        items: 1,
        tenant: "mock".into(),
        timestamp: "1970-01-01T00:00:00.000Z".into(),
        generation_id: None,
        identical: None,
        node: None,
        score: None,
        payload: None,
    }
}

fn operation_of(path: &str) -> Option<Operation> {
    Some(match path {
        "/v1/predict" | "/v1/means" => Operation::Predict,
        "/v1/classify" => Operation::Classify,
        "/v1/extract" => Operation::Extract,
        "/v1/embed" => Operation::Embed,
        "/v1/rerank" => Operation::Rerank,
        "/v1/decide" => Operation::Decide,
        _ => return None,
    })
}

fn mock_scored(op: Operation, item: &rvbbit_contract::ItemRequest) -> (serde_json::Value, Option<f64>) {
    let input = item
        .inputs
        .get("input")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let criterion = item
        .criterion
        .clone()
        .or_else(|| item.inputs.get("criterion").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .unwrap_or_default();
    let folded = input.to_ascii_lowercase();
    match op {
        Operation::Predict => {
            let yes = matches(&input, &criterion);
            (serde_json::Value::Bool(yes), Some(if yes { 0.82 } else { 0.15 }))
        }
        Operation::Classify => (serde_json::Value::String(criterion), Some(0.75)),
        Operation::Extract => {
            if item.extract_mode.as_deref() == Some("many") {
                let listed = if folded.contains("biped") || folded.contains("hairy") {
                    "[\"biped\"]"
                } else {
                    "[]"
                };
                (serde_json::Value::String(listed.into()), None)
            } else {
                (serde_json::json!({}), None)
            }
        }
        Operation::Embed => (serde_json::json!([0.0]), None),
        Operation::Rerank => {
            let score = if folded.contains("logging") { 0.91 } else { 0.12 };
            (serde_json::json!(score), Some(score))
        }
        Operation::Decide => (serde_json::json!({"label": "yes", "status": "ok"}), Some(0.75)),
    }
}

/// Kept so a test can force a positional item error without a new server.
#[allow(dead_code)]
pub fn item_error(key: &str, message: &str) -> ItemResponse {
    ItemResponse {
        key: key.to_string(),
        value: None,
        score: None,
        scores: None,
        calls: None,
        error: Some(ItemError {
            code: "mock".into(),
            message: message.into(),
        }),
        identical_takes: None,
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}
