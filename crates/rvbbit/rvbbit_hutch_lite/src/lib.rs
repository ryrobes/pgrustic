//! Early hutch that speaks `rvbbit-hutch-v1` and calls an OpenAI-compatible
//! chat endpoint (OpenRouter by default).
//!
//! Cache is in-memory, keyed by the contract storage key. Identical items
//! in flight share one provider call. `TAKES` / `PICK` are not implemented.
//! Embed, rerank, and decide return a per-item `unsupported` error.
//! Predict (MEANS), extract, and classify call the model.
//!
//! `OPENROUTER_API_KEY` is read from the environment and is never logged
//! or copied into an error string.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rvbbit_contract::{
    BatchRequest, BatchResponse, CacheHit, CacheLookupRequest, CacheLookupResponse, CacheMode,
    CachePurgeRequest, CachePurgeResponse, CachePutRequest, CachePutResponse, CacheStatsResponse,
    ItemError, ItemResponse, Operation, CONTRACT_VERSION, PATH_CACHE_LOOKUP, PATH_CACHE_PURGE,
    PATH_CACHE_PUT, PATH_CACHE_STATS,
};

const DEFAULT_MODEL: &str = "openai/gpt-4o-mini";
const DEFAULT_OPENAI_BASE: &str = "https://openrouter.ai/api/v1";

pub struct Config {
    pub openai_base: String,
    pub api_key: Option<String>,
    pub default_model: String,
    pub timeout: Duration,
    pub concurrency: usize,
    /// Used when the request has no bearer token.
    pub tenant: String,
    pub default_ttl: Duration,
    /// When set, require `Authorization: Bearer <value>`.
    pub expect_auth: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        let api_key = std::env::var("OPENROUTER_API_KEY").ok().filter(|s| !s.is_empty());
        let timeout_ms = std::env::var("HUTCH_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60_000);
        let concurrency = std::env::var("HUTCH_CONCURRENCY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4usize);
        let ttl_secs = std::env::var("HUTCH_CACHE_TTL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3600u64);
        Self {
            openai_base: std::env::var("OPENROUTER_BASE_URL")
                .unwrap_or_else(|_| DEFAULT_OPENAI_BASE.to_string()),
            api_key,
            default_model: std::env::var("HUTCH_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
            timeout: Duration::from_millis(timeout_ms),
            concurrency: concurrency.max(1),
            tenant: std::env::var("HUTCH_TENANT").unwrap_or_else(|_| "local".into()),
            default_ttl: Duration::from_secs(ttl_secs),
            expect_auth: std::env::var("HUTCH_EXPECT_AUTH").ok().filter(|s| !s.is_empty()),
        }
    }
}

struct CacheEntry {
    output: serde_json::Value,
    expires: Option<Instant>,
}

struct Flight {
    lock: Mutex<Option<Result<serde_json::Value, ItemError>>>,
    cv: Condvar,
}

struct Cache {
    entries: HashMap<String, CacheEntry>,
    inflight: HashMap<String, Arc<Flight>>,
    hits: u64,
    misses: u64,
    single_flight_waits: u64,
    provider_calls: u64,
    /// Operation POSTs, including ones served entirely from cache.
    requests: u64,
}

impl Cache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            inflight: HashMap::new(),
            hits: 0,
            misses: 0,
            single_flight_waits: 0,
            provider_calls: 0,
            requests: 0,
        }
    }
}

struct Server {
    cfg: Config,
    agent: ureq::Agent,
    cache: Mutex<Cache>,
}

pub fn serve(addr: &str, cfg: Config) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    serve_listener(listener, cfg)
}

pub fn serve_listener(listener: TcpListener, cfg: Config) -> std::io::Result<()> {
    let server = Arc::new(Server {
        cfg,
        agent: ureq::AgentBuilder::new().build(),
        cache: Mutex::new(Cache::new()),
    });
    for conn in listener.incoming() {
        let conn = conn?;
        let server = Arc::clone(&server);
        thread::spawn(move || {
            let _ = handle(conn, &server);
        });
    }
    Ok(())
}

fn handle(mut stream: TcpStream, server: &Server) {
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
    let mut method = String::new();
    let mut path = String::new();
    let mut auth = String::new();
    let mut cache_header = String::new();
    for (i, line) in header.lines().enumerate() {
        if i == 0 {
            let mut parts = line.split_whitespace();
            method = parts.next().unwrap_or("").to_string();
            path = parts.next().unwrap_or("/").to_string();
        } else if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("authorization") {
                auth = value.trim().to_string();
            } else if name.eq_ignore_ascii_case("x-rvbbit-cache") {
                cache_header = value.trim().to_string();
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
    let (status, payload) = route(server, &method, path_only(&path), &auth, &cache_header, &body);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
    let _ = stream.write_all(response.as_bytes());
}

fn route(
    server: &Server,
    method: &str,
    path: &str,
    auth: &str,
    cache_header: &str,
    body: &[u8],
) -> (u16, String) {
    if method.eq_ignore_ascii_case("GET") && (path == "/stats" || path == PATH_CACHE_STATS) {
        return (200, stats_json(server, path == "/stats"));
    }
    if method.eq_ignore_ascii_case("POST") && path == "/reset" {
        let mut cache = lock(&server.cache);
        *cache = Cache::new();
        return (200, "{\"ok\":true}".into());
    }
    if let Some(expect) = &server.cfg.expect_auth {
        if auth != format!("Bearer {expect}") {
            return (401, "{\"error\":\"unauthorized\"}".into());
        }
    }
    let tenant = tenant_of(&server.cfg, auth);
    if method.eq_ignore_ascii_case("POST") && path == PATH_CACHE_STATS {
        return (200, stats_json(server, false));
    }
    if method.eq_ignore_ascii_case("POST") && path == PATH_CACHE_LOOKUP {
        let req: CacheLookupRequest = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return (400, format!("{{\"error\":\"{e}\"}}")),
        };
        return (200, lookup(server, &tenant, &req));
    }
    if method.eq_ignore_ascii_case("POST") && path == PATH_CACHE_PUT {
        let req: CachePutRequest = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return (400, format!("{{\"error\":\"{e}\"}}")),
        };
        return (200, put(server, &tenant, &req));
    }
    if method.eq_ignore_ascii_case("POST") && path == PATH_CACHE_PURGE {
        let req: CachePurgeRequest = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return (400, format!("{{\"error\":\"{e}\"}}")),
        };
        return (200, purge(server, &tenant, &req));
    }
    let operation = operation_of(path);
    if method.eq_ignore_ascii_case("POST") && operation.is_some() {
        if server.cfg.api_key.is_none() {
            return (500, "{\"error\":\"upstream api key is not set\"}".into());
        }
        let req: BatchRequest = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return (400, format!("{{\"error\":\"{e}\"}}")),
        };
        let op = operation.unwrap_or(req.operation);
        let mode = parse_cache_header(cache_header);
        {
            let mut cache = lock(&server.cache);
            cache.requests += 1;
        }
        let resp = run_batch(server, &tenant, op, mode, &req);
        return (200, serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into()));
    }
    (404, "{\"error\":\"not found\"}".into())
}

fn run_batch(
    server: &Server,
    tenant: &str,
    operation: Operation,
    mode: ParsedCache,
    req: &BatchRequest,
) -> BatchResponse {
    let outputs = Arc::new(Mutex::new(vec![
        ItemResponse {
            key: String::new(),
            value: None,
            error: None,
        };
        req.items.len()
    ]));
    let slots = server.cfg.concurrency.max(1);
    let gate = Arc::new((Mutex::new(slots), Condvar::new()));
    thread::scope(|scope| {
        for (i, item) in req.items.iter().enumerate() {
            let gate = Arc::clone(&gate);
            let outputs = Arc::clone(&outputs);
            scope.spawn(move || {
                acquire(&gate);
                let result = run_item(server, tenant, operation, &mode, req, item);
                release(&gate);
                let mut slot = outputs.lock().unwrap_or_else(|e| e.into_inner());
                slot[i] = match result {
                    Ok(value) => ItemResponse {
                        key: item.key.clone(),
                        value: Some(value),
                        error: None,
                    },
                    Err(err) => ItemResponse {
                        key: item.key.clone(),
                        value: None,
                        error: Some(err),
                    },
                };
            });
        }
    });
    let outputs = match Arc::try_unwrap(outputs) {
        Ok(mutex) => mutex.into_inner().unwrap_or_else(|e| e.into_inner()),
        Err(shared) => shared.lock().unwrap_or_else(|e| e.into_inner()).clone(),
    };
    BatchResponse {
        contract: CONTRACT_VERSION.to_string(),
        outputs,
    }
}

fn run_item(
    server: &Server,
    tenant: &str,
    operation: Operation,
    mode: &ParsedCache,
    req: &BatchRequest,
    item: &rvbbit_contract::ItemRequest,
) -> Result<serde_json::Value, ItemError> {
    match operation {
        Operation::Embed | Operation::Rerank | Operation::Decide => {
            return Err(item_err(
                "unsupported",
                match operation {
                    Operation::Embed => "embed is not implemented in hutch-lite",
                    Operation::Rerank => "rerank is not implemented in hutch-lite",
                    _ => "decide is not implemented in hutch-lite",
                },
            ));
        }
        Operation::Predict | Operation::Extract | Operation::Classify => {}
    }
    let storage = storage_of(&item.cache_key, tenant);
    let ttl = mode.ttl.unwrap_or(server.cfg.default_ttl);
    let compute = || provider_call(server, operation, req, item);
    match mode.mode {
        CacheMode::Off => compute(),
        CacheMode::Refresh => {
            let value = compute()?;
            store(server, &storage, &value, ttl);
            Ok(value)
        }
        CacheMode::ReadWrite => get_or_compute(server, &storage, ttl, compute),
    }
}

fn get_or_compute(
    server: &Server,
    storage: &str,
    ttl: Duration,
    compute: impl FnOnce() -> Result<serde_json::Value, ItemError>,
) -> Result<serde_json::Value, ItemError> {
    {
        let mut cache = lock(&server.cache);
        if let Some(hit) = fresh(&cache, storage) {
            cache.hits += 1;
            return Ok(hit);
        }
        if let Some(flight) = cache.inflight.get(storage).cloned() {
            cache.single_flight_waits += 1;
            drop(cache);
            return wait_flight(&flight);
        }
        let flight = Arc::new(Flight {
            lock: Mutex::new(None),
            cv: Condvar::new(),
        });
        cache.inflight.insert(storage.to_string(), Arc::clone(&flight));
        cache.misses += 1;
        drop(cache);
        let result = compute();
        if let Ok(value) = &result {
            store(server, storage, value, ttl);
        }
        {
            let mut cache = lock(&server.cache);
            cache.inflight.remove(storage);
        }
        {
            let mut slot = flight.lock.lock().unwrap_or_else(|e| e.into_inner());
            *slot = Some(match &result {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(e.clone()),
            });
            flight.cv.notify_all();
        }
        result
    }
}

fn wait_flight(flight: &Flight) -> Result<serde_json::Value, ItemError> {
    let mut slot = flight.lock.lock().unwrap_or_else(|e| e.into_inner());
    while slot.is_none() {
        slot = flight.cv.wait(slot).unwrap_or_else(|e| e.into_inner());
    }
    slot.clone().expect("flight finished")
}

fn provider_call(
    server: &Server,
    operation: Operation,
    req: &BatchRequest,
    item: &rvbbit_contract::ItemRequest,
) -> Result<serde_json::Value, ItemError> {
    {
        let mut cache = lock(&server.cache);
        cache.provider_calls += 1;
    }
    let model = choose_model(&server.cfg, &req.model);
    let (system, user) = prompts(operation, item);
    let max_tokens = req.max_tokens.or(match operation {
        Operation::Extract => Some(256),
        _ => Some(32),
    });
    let text = chat(
        &server.cfg,
        &server.agent,
        &model,
        req.temperature,
        max_tokens,
        &system,
        &user,
    )
    .map_err(|message| item_err("provider", &message))?;
    match operation {
        Operation::Predict => parse_bool(&text).map(serde_json::Value::Bool).ok_or_else(|| {
            item_err("parse", "could not parse a boolean from model output")
        }),
        Operation::Extract => parse_json_value(&text).map_err(|message| item_err("parse", &message)),
        Operation::Classify => Ok(serde_json::Value::String(text.trim().to_string())),
        _ => Err(item_err("unsupported", "operation is not implemented in hutch-lite")),
    }
}

fn prompts(operation: Operation, item: &rvbbit_contract::ItemRequest) -> (String, String) {
    let user_filled = item
        .prompt
        .as_deref()
        .map(|p| substitute(p, &item.inputs))
        .filter(|s| !s.is_empty());
    let system = item
        .system
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| match operation {
            Operation::Predict => "Answer with only true or false.".into(),
            Operation::Extract => "Return only JSON. No markdown.".into(),
            Operation::Classify => "Reply with only the label.".into(),
            _ => String::new(),
        });
    let user = user_filled.unwrap_or_else(|| match operation {
        Operation::Predict => {
            let criterion = item
                .criterion
                .clone()
                .or_else(|| json_str(&item.inputs, "criterion"))
                .unwrap_or_default();
            let input = json_str(&item.inputs, "input").unwrap_or_default();
            format!("Criterion: {criterion}\nText: {input}")
        }
        Operation::Extract => {
            let input = json_str(&item.inputs, "input").unwrap_or_else(|| item.inputs.to_string());
            format!("Extract JSON from this text:\n{input}")
        }
        Operation::Classify => {
            let input = json_str(&item.inputs, "input").unwrap_or_else(|| item.inputs.to_string());
            format!("Label:\n{input}")
        }
        _ => String::new(),
    });
    (system, user)
}

fn chat(
    cfg: &Config,
    agent: &ureq::Agent,
    model: &str,
    temperature: f64,
    max_tokens: Option<u32>,
    system: &str,
    user: &str,
) -> Result<String, String> {
    let key = cfg.api_key.as_deref().unwrap_or("");
    let url = format!(
        "{}/chat/completions",
        cfg.openai_base.trim_end_matches('/')
    );
    let mut body = serde_json::json!({
        "model": model,
        "temperature": temperature,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user}
        ]
    });
    if let Some(n) = max_tokens {
        body["max_tokens"] = serde_json::json!(n);
    }
    let payload = match serde_json::to_string(&body) {
        Ok(s) => s,
        Err(e) => return Err(redact(&e.to_string(), key)),
    };
    let send = agent
        .post(&url)
        .timeout(cfg.timeout)
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {key}"))
        .send_string(&payload);
    match send {
        Ok(resp) => {
            let text = resp.into_string().map_err(|e| redact(&e.to_string(), key))?;
            completion_text(&text).map_err(|e| redact(&e, key))
        }
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp.into_string().unwrap_or_default();
            Err(redact(&format!("upstream HTTP {code}: {detail}"), key))
        }
        Err(e) => Err(redact(&format!("upstream transport: {e}"), key)),
    }
}

fn completion_text(body: &str) -> Result<String, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("upstream json: {e}"))?;
    if let Some(msg) = v.pointer("/error/message").and_then(|m| m.as_str()) {
        return Err(format!("upstream error: {msg}"));
    }
    v.pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "upstream response has no choices[0].message.content".into())
}

fn choose_model(cfg: &Config, requested: &str) -> String {
    if requested.is_empty() || requested == "mock" {
        cfg.default_model.clone()
    } else {
        requested.to_string()
    }
}

pub fn parse_bool(text: &str) -> Option<bool> {
    let trimmed = text.trim().trim_matches('`').trim();
    let word = trimmed
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_ascii_alphabetic())
        .to_ascii_lowercase();
    match word.as_str() {
        "true" | "yes" => Some(true),
        "false" | "no" => Some(false),
        _ => None,
    }
}

pub fn parse_json_value(text: &str) -> Result<serde_json::Value, String> {
    let stripped = strip_fences(text.trim());
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stripped) {
        return Ok(v);
    }
    let start = stripped.find(['{', '[']);
    if let Some(i) = start {
        let tail = &stripped[i..];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(tail) {
            return Ok(v);
        }
        // Trim trailing prose after the JSON value.
        let end = tail.rfind(['}', ']']).map(|n| n + 1).unwrap_or(tail.len());
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&tail[..end]) {
            return Ok(v);
        }
    }
    Err("model output was not JSON".into())
}

fn strip_fences(text: &str) -> &str {
    let t = text.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '+');
    let rest = rest.trim_start();
    rest.strip_suffix("```").unwrap_or(rest).trim()
}

fn substitute(template: &str, inputs: &serde_json::Value) -> String {
    let Some(obj) = inputs.as_object() else {
        return template.to_string();
    };
    let mut out = template.to_string();
    for (key, value) in obj {
        let replacement = match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let needle = format!("{{{{{key}}}}}");
        out = out.replace(&needle, &replacement);
    }
    out
}

fn json_str(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

#[derive(Clone, Copy)]
struct ParsedCache {
    mode: CacheMode,
    ttl: Option<Duration>,
}

fn parse_cache_header(header: &str) -> ParsedCache {
    let mut mode = CacheMode::ReadWrite;
    let mut ttl = None;
    if header.trim().is_empty() {
        return ParsedCache { mode, ttl };
    }
    for part in header.split(';') {
        let Some((k, v)) = part.trim().split_once('=') else {
            continue;
        };
        match k.trim() {
            "mode" => {
                mode = match v.trim() {
                    "off" => CacheMode::Off,
                    "refresh" => CacheMode::Refresh,
                    _ => CacheMode::ReadWrite,
                }
            }
            "ttl" => {
                if let Ok(secs) = v.trim().parse::<u64>() {
                    ttl = Some(Duration::from_secs(secs));
                }
            }
            _ => {}
        }
    }
    ParsedCache { mode, ttl }
}

fn storage_of(client_hex: &str, tenant: &str) -> String {
    let raw = decode_hex32(client_hex).unwrap_or_else(|| blake3::hash(client_hex.as_bytes()).into());
    hex32(&rvbbit_contract::storage_key(&raw, tenant))
}

fn lookup(server: &Server, tenant: &str, req: &CacheLookupRequest) -> String {
    let mut hits = Vec::new();
    let mut misses = Vec::new();
    let mut cache = lock(&server.cache);
    for key in &req.keys {
        let storage = storage_of(key, tenant);
        if let Some(output) = fresh(&cache, &storage) {
            cache.hits += 1;
            hits.push(CacheHit { key: key.clone(), output });
        } else {
            cache.misses += 1;
            misses.push(key.clone());
        }
    }
    serde_json::to_string(&CacheLookupResponse { hits, misses }).unwrap_or_else(|_| "{}".into())
}

fn put(server: &Server, tenant: &str, req: &CachePutRequest) -> String {
    let mut stored = 0u32;
    for entry in &req.entries {
        let storage = storage_of(&entry.key, tenant);
        let ttl = entry
            .ttl_secs
            .map(Duration::from_secs)
            .unwrap_or(server.cfg.default_ttl);
        store(server, &storage, &entry.output, ttl);
        stored += 1;
    }
    serde_json::to_string(&CachePutResponse { stored }).unwrap_or_else(|_| "{}".into())
}

fn purge(server: &Server, tenant: &str, req: &CachePurgeRequest) -> String {
    let mut cache = lock(&server.cache);
    let before = cache.entries.len();
    let purged = if req.all {
        cache.entries.clear();
        before as u32
    } else {
        let mut n = 0u32;
        for key in &req.keys {
            if cache.entries.remove(&storage_of(key, tenant)).is_some() {
                n += 1;
            }
        }
        n
    };
    serde_json::to_string(&CachePurgeResponse { purged }).unwrap_or_else(|_| "{}".into())
}

fn store(server: &Server, storage: &str, value: &serde_json::Value, ttl: Duration) {
    let expires = if ttl.is_zero() { None } else { Some(Instant::now() + ttl) };
    let mut cache = lock(&server.cache);
    cache.entries.insert(
        storage.to_string(),
        CacheEntry { output: value.clone(), expires },
    );
}

fn fresh(cache: &Cache, storage: &str) -> Option<serde_json::Value> {
    let entry = cache.entries.get(storage)?;
    if let Some(exp) = entry.expires {
        if Instant::now() >= exp {
            return None;
        }
    }
    Some(entry.output.clone())
}

fn stats_json(server: &Server, lite: bool) -> String {
    let cache = lock(&server.cache);
    let now = Instant::now();
    let entries = cache
        .entries
        .values()
        .filter(|e| e.expires.map(|t| now < t).unwrap_or(true))
        .count() as u64;
    if lite {
        return serde_json::json!({
            "entries": entries,
            "hits": cache.hits,
            "misses": cache.misses,
            "single_flight_waits": cache.single_flight_waits,
            "provider_calls": cache.provider_calls,
            "requests": cache.requests,
        })
        .to_string();
    }
    serde_json::to_string(&CacheStatsResponse {
        entries,
        hits: cache.hits,
        misses: cache.misses,
        single_flight_waits: cache.single_flight_waits,
    })
    .unwrap_or_else(|_| "{}".into())
}

fn tenant_of(cfg: &Config, auth: &str) -> String {
    let token = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))
        .unwrap_or("");
    if token.is_empty() {
        cfg.tenant.clone()
    } else {
        hex32(&blake3::hash(token.as_bytes()).into())
    }
}

fn item_err(code: &str, message: &str) -> ItemError {
    ItemError {
        code: code.to_string(),
        message: message.to_string(),
    }
}

fn redact(message: &str, key: &str) -> String {
    if key.is_empty() {
        message.to_string()
    } else {
        message.replace(key, "[redacted]")
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

fn acquire(gate: &Arc<(Mutex<usize>, Condvar)>) {
    let (lock, cv) = gate.as_ref();
    let mut free = lock.lock().unwrap_or_else(|e| e.into_inner());
    while *free == 0 {
        free = cv.wait(free).unwrap_or_else(|e| e.into_inner());
    }
    *free -= 1;
}

fn release(gate: &Arc<(Mutex<usize>, Condvar)>) {
    let (lock, cv) = gate.as_ref();
    let mut free = lock.lock().unwrap_or_else(|e| e.into_inner());
    *free += 1;
    cv.notify_one();
}

fn lock(cache: &Mutex<Cache>) -> std::sync::MutexGuard<'_, Cache> {
    cache.lock().unwrap_or_else(|e| e.into_inner())
}

fn path_only(path: &str) -> &str {
    path.split('?').next().unwrap_or(path)
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn decode_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn hex32(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_and_json_parsers() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("FALSE."), Some(false));
        assert_eq!(parse_bool("maybe"), None);
        let v = parse_json_value("```json\n{\"creature\":\"sasquatch\"}\n```").unwrap();
        assert_eq!(v["creature"], "sasquatch");
    }

    #[test]
    fn redact_removes_the_key() {
        assert_eq!(redact("bearer sk-secret failed", "sk-secret"), "bearer [redacted] failed");
    }

    #[test]
    fn prompt_substitution_uses_input() {
        let inputs = serde_json::json!({"input": "tracks in the mud"});
        assert_eq!(
            substitute("Text: {{input}}", &inputs),
            "Text: tracks in the mud"
        );
    }
}
