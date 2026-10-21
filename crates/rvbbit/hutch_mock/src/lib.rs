//! Deterministic stand-in for the hutch semantic service.
//!
//! `POST /v1/means` accepts a JSON array of `{key, input, criterion}` and
//! returns a JSON array of bools, one per item, in order. A match is a
//! case-insensitive substring test (`input` contains `criterion`). Empty
//! criteria are false.
//!
//! Environment:
//! - `HUTCH_LATENCY_MS` — sleep before each response (default 0)
//! - `HUTCH_FAIL_EVERY` — return HTTP 429 every Nth call (0 disables)
//! - `HUTCH_TIMEOUT_EVERY` — sleep 60s every Nth call so the client times out
//!
//! `GET /stats` reports calls, rows, max batch, and per-call batch sizes.
//! `POST /reset` zeroes those counters.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use serde::Deserialize;

#[derive(Clone, Debug)]
pub struct Config {
    pub latency: Duration,
    pub fail_every: u64,
    pub timeout_every: u64,
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
        Self {
            latency: Duration::from_millis(latency_ms),
            fail_every,
            timeout_every,
        }
    }
}

#[derive(Deserialize)]
struct Item {
    #[allow(dead_code)]
    key: Option<String>,
    input: String,
    criterion: String,
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
    for (i, line) in header.lines().enumerate() {
        if i == 0 {
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("");
            path = parts.next().unwrap_or("/").to_string();
            is_post = method.eq_ignore_ascii_case("POST");
        } else if let Some(v) = line.split_once(':') {
            if v.0.eq_ignore_ascii_case("content-length") {
                content_length = v.1.trim().parse().unwrap_or(0);
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

    if !cfg.latency.is_zero() && path.starts_with("/v1/means") {
        thread::sleep(cfg.latency);
    }

    let (status, payload) = route(is_post, &path, &body, cfg, stats);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
    let _ = stream.write_all(response.as_bytes());
}

fn route(is_post: bool, path: &str, body: &[u8], cfg: &Config, stats: &Stats) -> (u16, String) {
    if path.starts_with("/stats") {
        return (200, stats.json());
    }
    if path.starts_with("/reset") {
        stats.reset();
        return (200, "{\"ok\":true}".to_string());
    }
    if is_post && path.starts_with("/v1/means") {
        let seen = stats.calls.load(Ordering::Relaxed);
        if cfg.timeout_every > 0 && seen % cfg.timeout_every == 0 {
            stats.record(0);
            thread::sleep(Duration::from_secs(60));
            return (504, "{\"error\":\"injected timeout\"}".to_string());
        }
        if cfg.fail_every > 0 && seen % cfg.fail_every == 0 {
            stats.record(0);
            return (429, "{\"error\":\"injected rate limit\"}".to_string());
        }
        let items: Vec<Item> = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return (400, format!("{{\"error\":\"{e}\"}}")),
        };
        stats.record(items.len());
        let results: Vec<bool> = items.iter().map(|it| matches(&it.input, &it.criterion)).collect();
        return (200, serde_json::to_string(&results).unwrap_or_else(|_| "[]".into()));
    }
    (404, "{\"error\":\"not found\"}".to_string())
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}
