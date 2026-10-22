//! Early hutch that speaks `rvbbit-hutch-v1`.
//!
//! Predict and decide call OpenRouter's Decisions API (Jev by default) and
//! return a boolean plus the yes-probability. Fixed-label classify uses
//! one `choice` question on that same API and returns the winning label,
//! its probability, and the per-label map. Extract, and classify with no
//! label set, still call an OpenAI-compatible chat endpoint. Rerank is a
//! noul relevance score on the decision model unless the request or
//! `HUTCH_RERANK_MODEL` names a dedicated OpenRouter reranker.
//! Embed posts one OpenRouter `{base}/embeddings` for the cache misses in
//! the batch (`openai/text-embedding-3-small` unless the request names
//! another slug). Vectors are cached per normalized text and model.
//! `TAKES` / `PICK` are not parsed in SQL; a request may still send
//! `decision` to stack models or repeats.
//!
//! `OPENROUTER_API_KEY` is read from the environment and is never logged
//! or copied into an error string.
//!
//! Each provider call records tokens, USD cost, and latency. Chat cost is
//! `usage.cost`. Decisions cost is `usage.cost` with `input_tokens` and
//! `output_tokens`. A missing cost falls back to `GET {base}/generation`.

mod cost;
mod persist;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rvbbit_contract::{
    client_cache_key_hex, BatchRequest, BatchResponse, CacheHit, CacheKeyParts, CacheLookupRequest,
    CacheLookupResponse, CacheMode, CachePurgeRequest, CachePurgeResponse, CachePutRequest,
    CachePutResponse, CacheStatsResponse, CallMeter, DecisionStack, ItemError, ItemRequest,
    ItemResponse, Operation, CONTRACT_VERSION, EMBED_MODEL, EMBED_USD_PER_MILLION,
    PATH_CACHE_LOOKUP, PATH_CACHE_PURGE, PATH_CACHE_PUT, PATH_CACHE_STATS,
};

use cost::{
    generation_id_ok, merge_generation, meter_log_line, op_name, percentile, rfc3339_now,
    summarize_produced, usage_from_body, ProviderUsage,
};

/// Latency samples kept per op and model. The miss counter on
/// `/stats` is not limited to this window. The call ledger uses
/// [`rvbbit_contract::CALL_LOG_CAP`], which is much larger.
const LATENCY_SAMPLE_CAP: usize = 1024;

/// Attempts for one OpenRouter POST or GET. The first try plus two
/// retries. Sleeps between them are 50ms then 200ms.
const TRANSPORT_ATTEMPTS: u32 = 3;
const TRANSPORT_BACKOFF_MS: [u64; 2] = [50, 200];

thread_local! {
    static AUDIT_SNIP: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

fn audit_payload() -> Option<String> {
    AUDIT_SNIP.with(|c| c.borrow().clone())
}

const DEFAULT_MODEL: &str = "openai/gpt-4o-mini";
const DEFAULT_DECISION_MODEL: &str = "typesafe/jev-1.14";
const DEFAULT_OPENAI_BASE: &str = "https://openrouter.ai/api/v1";
const DECISION_TAKES_CAP: u32 = 8;

pub struct Config {
    pub openai_base: String,
    /// Full Decisions API URL. Derived from [`Self::openai_base`] unless
    /// `OPENROUTER_DECISIONS_URL` is set.
    pub decisions_url: String,
    pub api_key: Option<String>,
    /// Chat default (`HUTCH_MODEL`). Extract and free-form classify.
    pub default_model: String,
    /// Decision default (`HUTCH_DECISION_MODEL`). Predict, decide, and
    /// fixed-label classify when the request model is empty or `mock`.
    /// Also the Jev noul fallback for `RERANK`.
    pub decision_model: String,
    /// Dedicated rerank slug (`HUTCH_RERANK_MODEL`). Empty or `jev` keeps
    /// the noul fallback. `nvidia/nemotron-3.5-rerank-8b:free` and
    /// `voyageai/rerank-3` call OpenRouter `POST /rerank`.
    pub rerank_model: String,
    /// Embeddings slug (`HUTCH_EMBED_MODEL`). Empty, `mock`, `default`,
    /// and `jev` on a request use this. Default is
    /// `openai/text-embedding-3-small`.
    pub embed_model: String,
    pub decision_takes: u32,
    /// `mean` or `vote`.
    pub decision_combine: String,
    pub decision_threshold: f64,
    pub timeout: Duration,
    pub concurrency: usize,
    /// Used when the request has no bearer token.
    pub tenant: String,
    pub default_ttl: Duration,
    /// When set, require `Authorization: Bearer <value>`.
    pub expect_auth: Option<String>,
    /// `off`, `memory`, or `persistent`. `off` skips the contract cache.
    /// `persistent` writes the same entries under [`Self::cache_dir`] so a
    /// restart still serves them.
    pub cache_store: String,
    pub cache_dir: String,
    /// Record a short input snippet on each call meter.
    pub audit_payloads: bool,
}

/// `https://openrouter.ai/api/v1` -> `https://openrouter.ai/api/alpha/decisions`.
/// A test base that ends in `/v1` -> `{origin}/alpha/decisions`.
pub fn decisions_url_from_base(base: &str) -> String {
    let b = base.trim_end_matches('/');
    if let Some(root) = b.strip_suffix("/api/v1") {
        format!("{root}/api/alpha/decisions")
    } else if let Some(root) = b.strip_suffix("/v1") {
        format!("{root}/alpha/decisions")
    } else {
        format!("{b}/alpha/decisions")
    }
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
        let openai_base = std::env::var("OPENROUTER_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_OPENAI_BASE.to_string());
        let decisions_url = std::env::var("OPENROUTER_DECISIONS_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| decisions_url_from_base(&openai_base));
        let decision_takes = std::env::var("HUTCH_DECISION_TAKES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1u32)
            .clamp(1, DECISION_TAKES_CAP);
        let decision_threshold = std::env::var("HUTCH_DECISION_THRESHOLD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.5f64);
        let combine = std::env::var("HUTCH_DECISION_COMBINE").unwrap_or_else(|_| "mean".into());
        Self {
            decisions_url,
            openai_base,
            api_key,
            default_model: std::env::var("HUTCH_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
            decision_model: std::env::var("HUTCH_DECISION_MODEL")
                .unwrap_or_else(|_| DEFAULT_DECISION_MODEL.to_string()),
            rerank_model: std::env::var("HUTCH_RERANK_MODEL").unwrap_or_default(),
            embed_model: std::env::var("HUTCH_EMBED_MODEL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| EMBED_MODEL.to_string()),
            decision_takes,
            decision_combine: if combine == "vote" { "vote".into() } else { "mean".into() },
            decision_threshold,
            timeout: Duration::from_millis(timeout_ms),
            concurrency: concurrency.max(1),
            tenant: std::env::var("HUTCH_TENANT").unwrap_or_else(|_| "local".into()),
            default_ttl: Duration::from_secs(ttl_secs),
            expect_auth: std::env::var("HUTCH_EXPECT_AUTH").ok().filter(|s| !s.is_empty()),
            cache_store: cache_store_from_env(),
            cache_dir: std::env::var("HUTCH_CACHE_DIR").unwrap_or_else(|_| "hutch-cache".into()),
            audit_payloads: std::env::var("HUTCH_AUDIT_PAYLOADS")
                .map(|s| s == "on" || s == "1" || s == "true")
                .unwrap_or(false),
        }
    }
}

fn cache_store_from_env() -> String {
    match std::env::var("HUTCH_CACHE").unwrap_or_else(|_| "memory".into()).as_str() {
        "off" => "off".into(),
        "persistent" => "persistent".into(),
        _ => "memory".into(),
    }
}

#[derive(Clone)]
struct Scored {
    value: serde_json::Value,
    score: Option<f64>,
    scores: Option<serde_json::Value>,
    /// USD charged by misses while producing this value. None when there
    /// was no miss, or when a miss omitted cost. Memo hits do not add.
    produced_cost_usd: Option<f64>,
    /// Operation that produced the value. Empty for a bare put and for a
    /// file written before op was persisted. Lookup uses it for the
    /// per-op hit and skips that row when it is empty.
    op: String,
    model: String,
    generation_id: Option<String>,
    /// Meters for this invocation. Cleared before the value is stored.
    calls: Vec<CallMeter>,
    /// Set when there were two or more takes. True only when they agreed.
    identical_takes: Option<bool>,
}

impl Scored {
    fn bare(value: serde_json::Value, score: Option<f64>, scores: Option<serde_json::Value>) -> Self {
        Self {
            value,
            score,
            scores,
            produced_cost_usd: None,
            op: String::new(),
            model: String::new(),
            generation_id: None,
            calls: Vec::new(),
            identical_takes: None,
        }
    }

    fn from_calls(
        value: serde_json::Value,
        score: Option<f64>,
        scores: Option<serde_json::Value>,
        calls: Vec<CallMeter>,
    ) -> Self {
        let (produced_cost_usd, model, generation_id) = summarize_produced(&calls);
        let op = calls.first().map(|c| c.op.clone()).unwrap_or_default();
        Self {
            value,
            score,
            scores,
            produced_cost_usd,
            op,
            model,
            generation_id,
            calls,
            identical_takes: None,
        }
    }

    fn from_disk(entry: persist::DiskEntry) -> Self {
        Self {
            value: entry.value,
            score: entry.score,
            scores: None,
            produced_cost_usd: entry.produced_cost_usd,
            op: entry.op,
            model: entry.model,
            generation_id: None,
            calls: Vec::new(),
            identical_takes: None,
        }
    }
}

struct CacheEntry {
    scored: Scored,
    expires: Option<Instant>,
}

struct Flight {
    lock: Mutex<Option<Result<Scored, ItemError>>>,
    cv: Condvar,
}

struct DecisionFlight {
    lock: Mutex<Option<Result<String, ItemError>>>,
    cv: Condvar,
}

struct Cache {
    entries: HashMap<String, CacheEntry>,
    inflight: HashMap<String, Arc<Flight>>,
    /// Raw Decisions API bodies for identical requests (tenant, take, JSON).
    decision_memo: HashMap<String, String>,
    decision_inflight: HashMap<String, Arc<DecisionFlight>>,
    hits: u64,
    misses: u64,
    single_flight_waits: u64,
    provider_calls: u64,
    /// Operation POSTs, including ones served entirely from cache.
    requests: u64,
    /// Items inside those POSTs. One row is one item.
    rows: u64,
    /// Largest item count in a single operation POST.
    max_batch: u64,
    /// Recent provider calls and cache hits. `/reset` clears this.
    calls: VecDeque<CallMeter>,
    total_cost_usd: f64,
    cost_saved_usd: f64,
    /// Provider latency of misses, last [`LATENCY_SAMPLE_CAP`] per op+model.
    latency_samples: HashMap<(String, String), VecDeque<u64>>,
    /// Misses per op+model. Not capped when the sample window is.
    latency_misses: HashMap<(String, String), u64>,
    /// Decision-memo and contract-cache hits per op+model. Same reuse
    /// that `rvbbit.calls` records as `cache=hit`.
    latency_hits: HashMap<(String, String), u64>,
}

impl Cache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            inflight: HashMap::new(),
            decision_memo: HashMap::new(),
            decision_inflight: HashMap::new(),
            hits: 0,
            misses: 0,
            single_flight_waits: 0,
            provider_calls: 0,
            requests: 0,
            rows: 0,
            max_batch: 0,
            calls: VecDeque::new(),
            total_cost_usd: 0.0,
            cost_saved_usd: 0.0,
            latency_samples: HashMap::new(),
            latency_misses: HashMap::new(),
            latency_hits: HashMap::new(),
        }
    }
}

fn note_batch(cache: &mut Cache, items: usize) {
    let n = items as u64;
    cache.requests += 1;
    cache.rows = cache.rows.saturating_add(n);
    if n > cache.max_batch {
        cache.max_batch = n;
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
    // Idle-pool size is not an in-flight cap, but a pool of one makes
    // a burst of summary chats open and discard sockets. Size it to
    // the same concurrency the batch gate uses.
    let pool = cfg.concurrency.max(8);
    let server = Arc::new(Server {
        cfg,
        agent: ureq::AgentBuilder::new()
            .max_idle_connections(pool)
            .max_idle_connections_per_host(pool)
            .build(),
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
    let mut audit_header = String::new();
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
            } else if name.eq_ignore_ascii_case("x-rvbbit-audit") {
                audit_header = value.trim().to_string();
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
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let audit = server.cfg.audit_payloads || audit_header.split(',').any(|p| p.trim() == "payloads");
    let (status, payload) = route(
        server,
        &method,
        path_only(&path),
        query,
        &auth,
        &cache_header,
        audit,
        &body,
    );
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
    query: &str,
    auth: &str,
    cache_header: &str,
    audit: bool,
    body: &[u8],
) -> (u16, String) {
    if method.eq_ignore_ascii_case("GET") && (path == "/stats" || path == PATH_CACHE_STATS) {
        return (200, stats_json(server, path == "/stats"));
    }
    if method.eq_ignore_ascii_case("GET") && path == "/v1/calls" {
        return (200, calls_json(server, calls_limit(query)));
    }
    if method.eq_ignore_ascii_case("POST") && path == "/reset" {
        let mut cache = lock(&server.cache);
        *cache = Cache::new();
        if server.cfg.cache_store == "persistent" {
            persist::clear_dir(std::path::Path::new(&server.cfg.cache_dir));
        }
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
        let mut mode = parse_cache_header(cache_header);
        if server.cfg.cache_store == "off" {
            mode.mode = CacheMode::Off;
        }
        {
            let mut cache = lock(&server.cache);
            note_batch(&mut cache, req.items.len());
        }
        let resp = run_batch(server, &tenant, op, mode, &req, audit);
        return (200, serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into()));
    }
    (404, "{\"error\":\"not found\"}".into())
}

/// A dedicated rerank slug, or `None` when this batch stays on jev noul.
/// `mock`, empty, and `default` mean "the server default". `jev` and the
/// decision model always stay on noul, even if `HUTCH_RERANK_MODEL` is set.
fn dedicated_rerank_model(cfg: &Config, requested: &str) -> Option<String> {
    dedicated_rerank_slug(&cfg.decision_model, &cfg.rerank_model, requested)
}

fn dedicated_rerank_slug(decision_model: &str, env_model: &str, requested: &str) -> Option<String> {
    let requested = requested.trim();
    if requested.eq_ignore_ascii_case("jev") || requested == decision_model {
        return None;
    }
    let chosen = if requested.is_empty() || requested == "mock" || requested == "default" {
        env_model.trim()
    } else {
        requested
    };
    if chosen.is_empty() || chosen.eq_ignore_ascii_case("jev") || chosen == decision_model {
        None
    } else {
        Some(chosen.to_string())
    }
}

fn choose_embed_model(cfg: &Config, requested: &str) -> String {
    let requested = requested.trim();
    if requested.is_empty()
        || requested == "mock"
        || requested == "default"
        || requested.eq_ignore_ascii_case("jev")
    {
        cfg.embed_model.clone()
    } else {
        requested.to_string()
    }
}

/// Storage key for one text and model. The client cache key is ignored so
/// ABOUT and `semantic_similarity` share a vector.
fn embed_storage(model: &str, text: &str, tenant: &str) -> String {
    let norm = rvbbit_contract::normalize_embed_text(text);
    let inputs = serde_json::json!({ "input": norm });
    let hex = client_cache_key_hex(&CacheKeyParts {
        fn_name: "embed",
        fn_version: 1,
        system_prompt: "",
        user_prompt: "",
        return_schema: "vector",
        model,
        temperature: 0.0,
        max_tokens: None,
        take_index: 0,
        seed: "",
        opts: &serde_json::json!({}),
        threshold: None,
        extract_mode: "",
        inputs: &inputs,
    });
    storage_for(&hex, tenant, None, &[], &[])
}

fn embed_item_text(item: &ItemRequest) -> String {
    json_str(&item.inputs, "input")
        .or_else(|| json_str(&item.inputs, "text"))
        .unwrap_or_else(|| item.inputs.to_string())
}

/// One OpenRouter `POST /embeddings` for every distinct text that missed
/// the per-text cache. `value` is the vector. Cost is the provider
/// `usage.cost`, or tokens times the published $0.02 per million.
fn embed_via_openrouter(
    server: &Server,
    tenant: &str,
    mode: ParsedCache,
    req: &BatchRequest,
) -> BatchResponse {
    let model = choose_embed_model(&server.cfg, &req.model);
    let ttl = mode.ttl.unwrap_or(server.cfg.default_ttl);
    let mut outputs = Vec::with_capacity(req.items.len());
    let mut pending: Vec<EmbedPending> = Vec::new();
    for (index, item) in req.items.iter().enumerate() {
        let text = embed_item_text(item);
        if rvbbit_contract::normalize_embed_text(&text).is_empty() {
            outputs.push(scored_item(
                item,
                Scored::from_calls(serde_json::json!([]), None, None, Vec::new()),
            ));
            continue;
        }
        let storage = embed_storage(&model, &text, tenant);
        if mode.mode == CacheMode::ReadWrite {
            let started = Instant::now();
            let hit = {
                let mut cache = lock(&server.cache);
                fresh(server, &mut cache, &storage).map(|hit| {
                    cache.hits += 1;
                    hit
                })
            };
            if let Some(hit) = hit {
                let scored = meter_cached(server, Operation::Embed, tenant, &hit, "hit", started, item.node.clone());
                outputs.push(scored_item(item, scored));
                continue;
            }
        }
        pending.push(EmbedPending {
            index,
            storage,
            text: rvbbit_contract::normalize_embed_text(&text),
            node: item.node.clone(),
        });
        outputs.push(ItemResponse {
            key: item.key.clone(),
            value: None,
            score: None,
            scores: None,
            calls: None,
            error: Some(item_err("provider", "embed pending")),
            identical_takes: None,
        });
    }
    if pending.is_empty() {
        return BatchResponse {
            contract: CONTRACT_VERSION.to_string(),
            outputs,
        };
    }
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut seen: Vec<(String, usize)> = Vec::new();
    for (slot, row) in pending.iter().enumerate() {
        if let Some((_, group)) = seen.iter().find(|(text, _)| text == &row.text) {
            groups[*group].push(slot);
        } else {
            seen.push((row.text.clone(), groups.len()));
            groups.push(vec![slot]);
        }
    }
    let unique: Vec<String> = groups
        .iter()
        .map(|slots| pending[slots[0]].text.clone())
        .collect();
    let node = pending.iter().find_map(|row| row.node.clone());
    match post_openrouter_embed(server, tenant, &model, &unique, node) {
        Ok((vectors, meter)) => {
            let n_out = pending.len().max(1);
            for (offset, slots) in groups.iter().enumerate() {
                let vector = &vectors[offset];
                for slot in slots {
                    let row = &pending[*slot];
                    let mut call = meter.clone();
                    call.score = None;
                    call.items = u32::try_from(n_out).unwrap_or(u32::MAX);
                    if n_out > 1 {
                        if let Some(cost) = call.cost_usd {
                            call.cost_usd = Some(cost / n_out as f64);
                        }
                        if let Some(tokens) = call.tokens_in {
                            call.tokens_in = Some(tokens / n_out as u64);
                        }
                    }
                    let scored = Scored::from_calls(serde_json::json!(vector), None, None, vec![call]);
                    if mode.mode != CacheMode::Off {
                        store(server, &row.storage, &scored, ttl);
                    }
                    outputs[row.index] = scored_item(&req.items[row.index], scored);
                }
            }
        }
        Err(err) => {
            for row in &pending {
                outputs[row.index].error = Some(err.clone());
            }
        }
    }
    BatchResponse {
        contract: CONTRACT_VERSION.to_string(),
        outputs,
    }
}

struct EmbedPending {
    index: usize,
    storage: String,
    text: String,
    node: Option<String>,
}

fn post_openrouter_embed(
    server: &Server,
    tenant: &str,
    model: &str,
    texts: &[String],
    node: Option<String>,
) -> Result<(Vec<Vec<f64>>, CallMeter), ItemError> {
    if texts.is_empty() {
        return Err(item_err("invalid", "embed needs text"));
    }
    let url = format!("{}/embeddings", server.cfg.openai_base.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "input": texts,
    });
    let payload = serde_json::to_string(&body).map_err(|e| item_err("provider", &e.to_string()))?;
    {
        let mut cache = lock(&server.cache);
        cache.provider_calls += 1;
        cache.misses += 1;
    }
    let started = Instant::now();
    let text = match post_json(&server.cfg, &server.agent, &url, &payload) {
        Ok(text) => text,
        Err(message) => {
            let ms = elapsed_ms(started);
            let meter = miss_meter(
                Operation::Embed,
                tenant,
                model,
                &ProviderUsage::default(),
                ms,
                ms,
                node,
                None,
            );
            record_call(server, &meter);
            return Err(item_err("provider", &message));
        }
    };
    let provider_ms = elapsed_ms(started);
    let mut usage = usage_from_body(&text);
    if usage.model.is_none() {
        usage.model = Some(model.to_string());
    }
    if usage.cost_usd.is_none() {
        fill_generation(server, &mut usage);
    }
    if usage.cost_usd.is_none() {
        if let Some(tokens) = usage.tokens_in.or(usage.tokens_out) {
            usage.cost_usd = Some(tokens as f64 * EMBED_USD_PER_MILLION / 1_000_000.0);
        }
    }
    let latency_ms = elapsed_ms(started);
    let mut meter = miss_meter(
        Operation::Embed,
        tenant,
        model,
        &usage,
        provider_ms,
        latency_ms,
        node,
        None,
    );
    meter.items = u32::try_from(texts.len()).unwrap_or(u32::MAX);
    record_call(server, &meter);
    let vectors = parse_embeddings(&text, texts.len()).map_err(|message| item_err("parse", &message))?;
    Ok((vectors, meter))
}

fn parse_embeddings(body: &str, n: usize) -> Result<Vec<Vec<f64>>, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("embed json: {e}"))?;
    if let Some(msg) = v.pointer("/error/message").and_then(|m| m.as_str()) {
        return Err(format!("upstream error: {msg}"));
    }
    let data = v
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or("embeddings response has no data")?;
    let mut out: Vec<Option<Vec<f64>>> = vec![None; n];
    for (fallback, row) in data.iter().enumerate() {
        let index = row.get("index").and_then(|i| i.as_u64()).unwrap_or(fallback as u64) as usize;
        let nums = row
            .get("embedding")
            .and_then(|e| e.as_array())
            .ok_or("embedding missing")?;
        let mut vector = Vec::with_capacity(nums.len());
        for number in nums {
            let Some(x) = number.as_f64() else {
                return Err("embedding is not a numeric vector".into());
            };
            vector.push(x);
        }
        if vector.is_empty() {
            return Err("embedding is empty".into());
        }
        if index >= n {
            return Err(format!("embedding index {index} is outside {n} texts"));
        }
        out[index] = Some(vector);
    }
    out.into_iter()
        .enumerate()
        .map(|(i, vector)| vector.ok_or_else(|| format!("embedding missing vector {i}")))
        .collect()
}

/// One OpenRouter `POST /rerank` for every document that shares a query.
/// Jev noul does not come through here.
fn rerank_via_openrouter(
    server: &Server,
    tenant: &str,
    mode: ParsedCache,
    req: &BatchRequest,
    model: &str,
) -> BatchResponse {
    let ttl = mode.ttl.unwrap_or(server.cfg.default_ttl);
    let mut outputs = Vec::with_capacity(req.items.len());
    let mut pending: Vec<RerankPending> = Vec::new();
    for (index, item) in req.items.iter().enumerate() {
        let doc = json_str(&item.inputs, "input").unwrap_or_else(|| item.inputs.to_string());
        let query = item
            .criterion
            .clone()
            .or_else(|| json_str(&item.inputs, "criterion"))
            .unwrap_or_default();
        let storage = storage_for(&item.cache_key, tenant, None, &[], &[]);
        if mode.mode == CacheMode::ReadWrite {
            let started = Instant::now();
            let hit = {
                let mut cache = lock(&server.cache);
                fresh(server, &mut cache, &storage).map(|hit| {
                    cache.hits += 1;
                    hit
                })
            };
            if let Some(hit) = hit {
                let scored = meter_cached(server, Operation::Rerank, tenant, &hit, "hit", started, item.node.clone());
                outputs.push(scored_item(item, scored));
                continue;
            }
        }
        pending.push(RerankPending { index, storage, doc, query, node: item.node.clone() });
        outputs.push(ItemResponse {
            key: item.key.clone(),
            value: None,
            score: None,
            scores: None,
            calls: None,
            error: Some(item_err("provider", "rerank pending")),
            identical_takes: None,
        });
    }
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (slot, row) in pending.iter().enumerate() {
        groups.entry(row.query.clone()).or_default().push(slot);
    }
    for (query, slots) in groups {
        let docs: Vec<String> = slots.iter().map(|slot| pending[*slot].doc.clone()).collect();
        let node = slots.iter().find_map(|slot| pending[*slot].node.clone());
        match post_openrouter_rerank(server, tenant, model, &query, &docs, node) {
            Ok((scores, meter)) => {
                for (offset, slot) in slots.iter().enumerate() {
                    let row = &pending[*slot];
                    let score = scores[offset];
                    let mut call = meter.clone();
                    call.score = Some(score);
                    if docs.len() > 1 {
                        if let Some(cost) = call.cost_usd {
                            call.cost_usd = Some(cost / docs.len() as f64);
                        }
                    }
                    let scored = Scored::from_calls(serde_json::json!(score), Some(score), None, vec![call]);
                    if mode.mode != CacheMode::Off {
                        store(server, &row.storage, &scored, ttl);
                    }
                    outputs[row.index] = scored_item(&req.items[row.index], scored);
                }
            }
            Err(err) => {
                for slot in slots {
                    let row = &pending[slot];
                    outputs[row.index].error = Some(err.clone());
                }
            }
        }
    }
    BatchResponse {
        contract: CONTRACT_VERSION.to_string(),
        outputs,
    }
}

struct RerankPending {
    index: usize,
    storage: String,
    doc: String,
    query: String,
    node: Option<String>,
}

fn scored_item(item: &ItemRequest, scored: Scored) -> ItemResponse {
    ItemResponse {
        key: item.key.clone(),
        value: Some(scored.value),
        score: scored.score,
        scores: scored.scores,
        calls: if scored.calls.is_empty() { None } else { Some(scored.calls) },
        error: None,
        identical_takes: scored.identical_takes,
    }
}

fn post_openrouter_rerank(
    server: &Server,
    tenant: &str,
    model: &str,
    query: &str,
    documents: &[String],
    node: Option<String>,
) -> Result<(Vec<f64>, CallMeter), ItemError> {
    if documents.is_empty() {
        return Err(item_err("invalid", "rerank needs a document"));
    }
    let url = format!("{}/rerank", server.cfg.openai_base.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "query": query,
        "documents": documents,
        "top_n": documents.len(),
    });
    let payload = serde_json::to_string(&body).map_err(|e| item_err("provider", &e.to_string()))?;
    {
        let mut cache = lock(&server.cache);
        cache.provider_calls += 1;
        cache.misses += 1;
    }
    let started = Instant::now();
    let text = match post_json(&server.cfg, &server.agent, &url, &payload) {
        Ok(text) => text,
        Err(message) => {
            let ms = elapsed_ms(started);
            let meter = miss_meter(
                Operation::Rerank,
                tenant,
                model,
                &ProviderUsage::default(),
                ms,
                ms,
                node,
                None,
            );
            record_call(server, &meter);
            return Err(item_err("provider", &message));
        }
    };
    let provider_ms = elapsed_ms(started);
    let mut usage = usage_from_body(&text);
    if usage.model.is_none() {
        usage.model = Some(model.to_string());
    }
    if usage.cost_usd.is_none() {
        fill_generation(server, &mut usage);
    }
    let latency_ms = elapsed_ms(started);
    let mut meter = miss_meter(
        Operation::Rerank,
        tenant,
        model,
        &usage,
        provider_ms,
        latency_ms,
        node,
        None,
    );
    meter.items = u32::try_from(documents.len()).unwrap_or(u32::MAX);
    record_call(server, &meter);
    let scores = parse_rerank_scores(&text, documents.len()).map_err(|message| item_err("parse", &message))?;
    Ok((scores, meter))
}

fn parse_rerank_scores(body: &str, n: usize) -> Result<Vec<f64>, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("rerank json: {e}"))?;
    if let Some(msg) = v.pointer("/error/message").and_then(|m| m.as_str()) {
        return Err(format!("upstream error: {msg}"));
    }
    let results = v
        .get("results")
        .and_then(|r| r.as_array())
        .ok_or("rerank response has no results")?;
    let mut scores = vec![None; n];
    for result in results {
        let index = result
            .get("index")
            .and_then(|i| i.as_u64())
            .ok_or("rerank result missing index")? as usize;
        let score = result
            .get("relevance_score")
            .and_then(|s| s.as_f64())
            .ok_or("rerank result missing relevance_score")?;
        if index >= n {
            return Err(format!("rerank index {index} is outside {n} documents"));
        }
        scores[index] = Some(score);
    }
    scores
        .into_iter()
        .enumerate()
        .map(|(i, score)| score.ok_or_else(|| format!("rerank missing score for document {i}")))
        .collect()
}

fn run_batch(
    server: &Server,
    tenant: &str,
    operation: Operation,
    mode: ParsedCache,
    req: &BatchRequest,
    audit: bool,
) -> BatchResponse {
    if operation == Operation::Embed {
        return embed_via_openrouter(server, tenant, mode, req);
    }
    if operation == Operation::Rerank {
        if let Some(model) = dedicated_rerank_model(&server.cfg, &req.model) {
            return rerank_via_openrouter(server, tenant, mode, req, &model);
        }
    }
    let outputs = Arc::new(Mutex::new(vec![
        ItemResponse {
            key: String::new(),
            value: None,
            score: None,
            scores: None,
            calls: None,
            error: None,
            identical_takes: None,
        };
        req.items.len()
    ]));
    // One worker per item, capped by cfg.concurrency. scope joins them
    // all, so a batch of summary chats overlaps instead of running one
    // provider call after another.
    let slots = server.cfg.concurrency.max(1);
    let gate = Arc::new((Mutex::new(slots), Condvar::new()));
    thread::scope(|scope| {
        for (i, item) in req.items.iter().enumerate() {
            let gate = Arc::clone(&gate);
            let outputs = Arc::clone(&outputs);
            scope.spawn(move || {
                acquire(&gate);
                let result = run_item(server, tenant, operation, mode, req, item, audit);
                release(&gate);
                let mut slot = outputs.lock().unwrap_or_else(|e| e.into_inner());
                slot[i] = match result {
                    Ok(scored) => ItemResponse {
                        key: item.key.clone(),
                        value: Some(scored.value),
                        score: scored.score,
                        scores: scored.scores,
                        calls: if scored.calls.is_empty() { None } else { Some(scored.calls) },
                        error: None,
                        identical_takes: scored.identical_takes,
                    },
                    Err(err) => ItemResponse {
                        key: item.key.clone(),
                        value: None,
                        score: None,
                        scores: None,
                        calls: None,
                        error: Some(err),
                        identical_takes: None,
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
    mode: ParsedCache,
    req: &BatchRequest,
    item: &rvbbit_contract::ItemRequest,
    audit: bool,
) -> Result<Scored, ItemError> {
    if audit {
        let text = item.inputs.to_string();
        let snip = if text.chars().count() > 180 {
            text.chars().take(180).collect()
        } else {
            text
        };
        AUDIT_SNIP.with(|c| *c.borrow_mut() = Some(snip));
    } else {
        AUDIT_SNIP.with(|c| *c.borrow_mut() = None);
    }
    match operation {
        Operation::Embed => {
            return Err(item_err("unsupported", "embed is handled before per-item dispatch"));
        }
        Operation::Predict | Operation::Decide | Operation::Extract | Operation::Classify | Operation::Rerank => {}
    }
    let labels = item.labels.clone().unwrap_or_default();
    let descriptions = item.label_descriptions.clone().unwrap_or_default();
    let fixed_classify = operation == Operation::Classify && labels.len() >= 2;
    let plan = (matches!(operation, Operation::Predict | Operation::Decide | Operation::Rerank) || fixed_classify)
        .then(|| decision_plan(&server.cfg, req));
    let storage = storage_for(&item.cache_key, tenant, plan.as_ref(), &labels, &descriptions);
    let ttl = mode.ttl.unwrap_or(server.cfg.default_ttl);
    let started = Instant::now();
    let compute = || provider_call(server, tenant, operation, req, item, mode.mode);
    match mode.mode {
        CacheMode::Off => compute(),
        CacheMode::Refresh => {
            let value = compute()?;
            store(server, &storage, &value, ttl);
            Ok(value)
        }
        CacheMode::ReadWrite => {
            get_or_compute(server, &storage, ttl, operation, tenant, started, item.node.clone(), compute)
        }
    }
}

fn get_or_compute(
    server: &Server,
    storage: &str,
    ttl: Duration,
    operation: Operation,
    tenant: &str,
    started: Instant,
    node: Option<String>,
    compute: impl FnOnce() -> Result<Scored, ItemError>,
) -> Result<Scored, ItemError> {
    {
        let mut cache = lock(&server.cache);
        if let Some(hit) = fresh(server, &mut cache, storage) {
            cache.hits += 1;
            drop(cache);
            return Ok(meter_cached(server, operation, tenant, &hit, "hit", started, node.clone()));
        }
        if let Some(flight) = cache.inflight.get(storage).cloned() {
            cache.single_flight_waits += 1;
            drop(cache);
            let shared = wait_flight(&flight)?;
            return Ok(meter_cached(
                server,
                operation,
                tenant,
                &shared,
                "single-flight",
                started,
                node.clone(),
            ));
        }
        let flight = Arc::new(Flight {
            lock: Mutex::new(None),
            cv: Condvar::new(),
        });
        cache.inflight.insert(storage.to_string(), Arc::clone(&flight));
        drop(cache);
        let result = compute();
        if let Ok(scored) = &result {
            store(server, storage, scored, ttl);
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

fn wait_flight(flight: &Flight) -> Result<Scored, ItemError> {
    let mut slot = flight.lock.lock().unwrap_or_else(|e| e.into_inner());
    while slot.is_none() {
        slot = flight.cv.wait(slot).unwrap_or_else(|e| e.into_inner());
    }
    slot.clone().expect("flight finished")
}

fn provider_call(
    server: &Server,
    tenant: &str,
    operation: Operation,
    req: &BatchRequest,
    item: &ItemRequest,
    mode: CacheMode,
) -> Result<Scored, ItemError> {
    // The decision-body memo is only for read-write. `off` and `refresh`
    // call the provider again. MEANS and means_score share a call when
    // both are memoize and the Decisions JSON matches.
    let memo = mode == CacheMode::ReadWrite;
    if matches!(operation, Operation::Predict | Operation::Decide | Operation::Rerank) {
        return decision_scored(server, tenant, operation, req, item, memo);
    }
    if operation == Operation::Extract && plan_is_wide(req) {
        return extract_fanout(server, tenant, operation, req, item, memo);
    }
    if operation == Operation::Classify {
        let n = item.labels.as_ref().map(|l| l.len()).unwrap_or(0);
        if n == 1 {
            return Err(item_err(
                "invalid",
                "classify label set needs at least two labels",
            ));
        }
        if n >= 2 {
            return choice_scored(server, tenant, operation, req, item, memo);
        }
    }
    let model = choose_model(&server.cfg, &req.model);
    let (system, user) = prompts(operation, item);
    let max_tokens = req.max_tokens.or(match operation {
        Operation::Extract => Some(256),
        _ => Some(32),
    });
    let (text, mut meter) = chat(
        server,
        operation,
        tenant,
        &model,
        req.temperature,
        max_tokens,
        &system,
        &user,
        item.node.clone(),
        item.response_schema.as_ref(),
        chat_seed(item.extract_mode.as_deref()),
    )
        .map_err(|message| item_err("provider", &message))?;
    // SUMMARIZE shares the extract route. The call log has to say
    // summarize so the planner can price it apart from EXTRACT MANY.
    if item.extract_mode.as_deref() == Some("summary") {
        meter.op = "summarize".into();
    }
    let value = match operation {
        Operation::Extract => parse_extract(&text, item.extract_mode.as_deref())
            .map_err(|message| item_err("parse", &message))?,
        Operation::Classify => serde_json::Value::String(text.trim().to_string()),
        _ => return Err(item_err("unsupported", "operation is not implemented in hutch-lite")),
    };
    Ok(Scored::from_calls(value, None, None, vec![meter]))
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
            Operation::Extract => match item.extract_mode.as_deref() {
                Some("scalar") => {
                    "Return one short noun or literal, at most three words. No sentence, no description, no markdown, no quotes.".into()
                }
                Some("summary") => "Summarize the notes in one or two sentences. No markdown.".into(),
                Some("many") => {
                    "Return JSON {\"mentions\":[{\"mention\",\"category\"}]} and no markdown. \
                     category is specific_instance_of_kind, generic_kind_word, attribute_or_part, place, sound_or_event, or other. \
                     Copy the whole noun phrase (\"black dog\", not \"dog\"). \
                     Examples, do not copy: creature in \"My story of Bigfoot\" is Bigfoot; \
                     color in \"Large gray triangular aircraft\" is gray; \
                     creature in \"a sasquatch crossed the road\" is sasquatch. \
                     Copy only words from the user text. Leave out anything that is not that kind. \
                     If there are none, mentions is []."
                        .into()
                }
                _ => "Return only JSON. No markdown.".into(),
            },
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
            if item.extract_mode.as_deref() == Some("summary") {
                input
            } else if item.extract_mode.as_deref() == Some("many") {
                format!(
                    "List every mention of the requested kind. Copy the whole noun phrase. \
                     Set category to specific_instance_of_kind, generic_kind_word, attribute_or_part, place, sound_or_event, or other. \
                     Leave out anything that is not that kind. If there are none, mentions is [].\n{input}"
                )
            } else if item.extract_mode.as_deref() == Some("scalar") {
                format!("Extract one short value from this text:\n{input}")
            } else {
                format!("Extract JSON from this text:\n{input}")
            }
        }
        Operation::Classify => {
            let input = json_str(&item.inputs, "input").unwrap_or_else(|| item.inputs.to_string());
            format!("Label:\n{input}")
        }
        _ => String::new(),
    });
    (system, user)
}

#[derive(Clone)]
struct DecisionPlan {
    models: Vec<String>,
    takes: u32,
    combine: Combine,
    threshold: f64,
    phrasings: Vec<String>,
}

#[derive(Clone, Copy)]
enum Combine {
    Mean,
    Vote,
}

impl Combine {
    fn as_str(self) -> &'static str {
        match self {
            Combine::Mean => "mean",
            Combine::Vote => "vote",
        }
    }
}

fn decision_plan(cfg: &Config, req: &BatchRequest) -> DecisionPlan {
    let stack = req.decision.clone().unwrap_or(DecisionStack {
        models: Vec::new(),
        takes: None,
        combine: None,
        threshold: None,
        phrasings: Vec::new(),
        pick: None,
    });
    let mut models: Vec<String> = stack
        .models
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| choose_decision_model(cfg, &s))
        .collect();
    if models.is_empty() {
        models.push(choose_decision_model(cfg, &req.model));
    }
    let takes = stack.takes.unwrap_or(cfg.decision_takes).clamp(1, DECISION_TAKES_CAP);
    let combine = match stack.combine.as_deref().unwrap_or(cfg.decision_combine.as_str()) {
        "vote" => Combine::Vote,
        _ => Combine::Mean,
    };
    DecisionPlan {
        models,
        takes,
        combine,
        threshold: stack.threshold.unwrap_or(cfg.decision_threshold),
        phrasings: stack.phrasings,
    }
}

fn choose_decision_model(cfg: &Config, requested: &str) -> String {
    let requested = requested.trim();
    // `jev` is the noul fallback name, not an OpenRouter slug.
    if requested.is_empty() || requested == "mock" || requested.eq_ignore_ascii_case("jev") {
        cfg.decision_model.clone()
    } else {
        requested.to_string()
    }
}

fn decision_scored(
    server: &Server,
    tenant: &str,
    operation: Operation,
    req: &BatchRequest,
    item: &ItemRequest,
    memo: bool,
) -> Result<Scored, ItemError> {
    let plan = decision_plan(&server.cfg, req);
    let phrasings = plan_phrasings(&plan);
    let mut scores = Vec::new();
    let mut calls = Vec::new();
    let mut prints: Vec<(String, String)> = Vec::new();
    for model in &plan.models {
        for phrasing in &phrasings {
            let mut asked = item.clone();
            if !phrasing.is_empty() {
                asked.criterion = Some(phrasing.clone());
                asked.prompt = Some(phrasing.clone());
            }
            for take in 0..plan.takes {
                let (text, meter) = decision_http(
                    server,
                    tenant,
                    operation,
                    DecisionKind::Noul,
                    model,
                    take,
                    &asked,
                    memo,
                )?;
                calls.push(meter);
                let score = parse_noul(&text).map_err(map_decision_err)?;
                prints.push((format!("{model}\n{phrasing}"), format!("{score:.6}")));
                scores.push(score);
            }
        }
    }
    let identical = identical_prints(&prints);
    if prints.len() >= 2 {
        for call in &mut calls {
            call.identical = Some(identical);
        }
        // `decision_http` already wrote these meters, with `identical`
        // still absent. Stamp the log so `rvbbit.calls` stores false
        // when the takes disagree, not null.
        stamp_logged_identical(server, &calls);
    }
    let (matched, mean) = combine_noul(plan.combine, plan.threshold, &scores);
    let value = if operation == Operation::Rerank {
        serde_json::json!(mean)
    } else {
        serde_json::Value::Bool(matched)
    };
    let mut scored = Scored::from_calls(value, Some(mean), None, calls);
    if prints.len() >= 2 {
        scored.identical_takes = Some(identical);
    }
    Ok(scored)
}

fn plan_phrasings(plan: &DecisionPlan) -> Vec<String> {
    if plan.phrasings.is_empty() {
        vec![String::new()]
    } else {
        plan.phrasings.clone()
    }
}

/// True when every model+phrasing that was asked more than once returned
/// the same fingerprint. A single take does not count, and the caller
/// leaves the flag absent. A live Jev run of one phrasing varied by about
/// 0.03 across takes, so this was false. A second phrasing moved UFO row 2
/// from about 0.67 to about 0.83, more than the take-to-take spread.
/// A take that disagrees with its siblings clears the flag.
fn identical_prints(prints: &[(String, String)]) -> bool {
    let mut repeated = false;
    for (i, (model, fp)) in prints.iter().enumerate() {
        let group: Vec<_> = prints.iter().filter(|(m, _)| m == model).map(|(_, f)| f).collect();
        if group.len() < 2 {
            continue;
        }
        if i > 0 && prints[..i].iter().any(|(m, _)| m == model) {
            continue;
        }
        repeated = true;
        if group.iter().any(|f| *f != fp) {
            return false;
        }
    }
    repeated
}

/// One `choice` question per call. Stacked calls average `probabilities`
/// over the declared labels and the winner is the highest mean. A label
/// the model invents cannot win. Ties keep the earlier declared label.
///
/// Jev is not binary-only, so this is one Decisions request per item per
/// take per stacked model, not one request per label. A binary-only model
/// would need N `noul` calls (one-vs-rest) and then argmax. That fallback
/// is not implemented.
fn choice_scored(
    server: &Server,
    tenant: &str,
    operation: Operation,
    req: &BatchRequest,
    item: &ItemRequest,
    memo: bool,
) -> Result<Scored, ItemError> {
    let labels = item.labels.clone().unwrap_or_default();
    if labels.len() < 2 {
        return Err(item_err(
            "invalid",
            "classify label set needs at least two labels",
        ));
    }
    let plan = decision_plan(&server.cfg, req);
    let phrasings = plan_phrasings(&plan);
    let mut sums: HashMap<String, f64> = HashMap::new();
    let mut calls = 0f64;
    let mut meters = Vec::new();
    let mut prints: Vec<(String, String)> = Vec::new();
    for model in &plan.models {
        for phrasing in &phrasings {
            let mut asked = item.clone();
            if !phrasing.is_empty() {
                asked.prompt = Some(phrasing.clone());
            }
            for take in 0..plan.takes {
                let (text, meter) = decision_http(
                    server,
                    tenant,
                    operation,
                    DecisionKind::Choice,
                    model,
                    take,
                    &asked,
                    memo,
                )?;
                meters.push(meter);
                let hit = parse_choice(&text).map_err(map_decision_err)?;
                let mut fp = String::new();
                for label in &labels {
                    let p = hit.probabilities.get(label).copied().unwrap_or(0.0);
                    *sums.entry(label.clone()).or_insert(0.0) += p;
                    fp.push_str(&format!("{label}={p:.6};"));
                }
                prints.push((format!("{model}\n{phrasing}"), fp));
                calls += 1.0;
            }
        }
    }
    // One extra choice when a single take is a toss-up. Stacked takes,
    // models, or phrasings already asked more than once, so they are left
    // alone. The retry prompt is a different body, so it is not a repeat
    // of the first take and does not set `identical`.
    let single = plan.models.len() == 1 && plan.takes == 1 && plan.phrasings.is_empty();
    if single {
        let (_, best) = choice_winner(&labels, &sums, calls);
        if best < CHOICE_RETRY_BELOW {
            let model = &plan.models[0];
            let mut asked = item.clone();
            asked.prompt = Some(CHOICE_RETRY_PROMPT.to_string());
            let (text, mut meter) = decision_http(
                server,
                tenant,
                operation,
                DecisionKind::Choice,
                model,
                1,
                &asked,
                memo,
            )?;
            // EXPLAIN reads this response meter. The copy already logged
            // inside `decision_http` stays unmarked.
            meter.retake = Some(true);
            meters.push(meter);
            let hit = parse_choice(&text).map_err(map_decision_err)?;
            let mut fp = String::new();
            for label in &labels {
                let p = hit.probabilities.get(label).copied().unwrap_or(0.0);
                *sums.entry(label.clone()).or_insert(0.0) += p;
                fp.push_str(&format!("{label}={p:.6};"));
            }
            prints.push((format!("{model}\n{CHOICE_RETRY_PROMPT}"), fp));
            calls += 1.0;
        }
    }
    let identical = identical_prints(&prints);
    let repeated = prints
        .iter()
        .any(|(key, _)| prints.iter().filter(|(other, _)| other == key).count() >= 2);
    if repeated {
        for meter in &mut meters {
            meter.identical = Some(identical);
        }
        stamp_logged_identical(server, &meters);
    }
    let mut scores = serde_json::Map::new();
    let (best_label, best) = choice_winner(&labels, &sums, calls);
    for label in &labels {
        let mean = sums.get(label).copied().unwrap_or(0.0) / calls;
        scores.insert(label.clone(), serde_json::json!(mean));
    }
    let mut scored = Scored::from_calls(
        serde_json::Value::String(best_label),
        Some(best),
        Some(serde_json::Value::Object(scores)),
        meters,
    );
    if repeated {
        scored.identical_takes = Some(identical);
    }
    Ok(scored)
}

fn choice_winner(labels: &[String], sums: &HashMap<String, f64>, calls: f64) -> (String, f64) {
    let mut best_label = labels[0].clone();
    let mut best = f64::NEG_INFINITY;
    let denom = if calls > 0.0 { calls } else { 1.0 };
    for label in labels {
        let mean = sums.get(label).copied().unwrap_or(0.0) / denom;
        if mean > best {
            best = mean;
            best_label = label.clone();
        }
    }
    (best_label, best)
}

fn combine_noul(combine: Combine, threshold: f64, scores: &[f64]) -> (bool, f64) {
    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    let matched = match combine {
        Combine::Mean => mean >= threshold,
        Combine::Vote => {
            let yes = scores.iter().filter(|s| **s >= threshold).count();
            let no = scores.len() - yes;
            if yes > no {
                true
            } else if yes < no {
                false
            } else {
                mean >= threshold
            }
        }
    };
    (matched, mean)
}

#[derive(Clone, Copy)]
enum DecisionKind {
    Noul,
    Choice,
}

fn decision_http(
    server: &Server,
    tenant: &str,
    operation: Operation,
    kind: DecisionKind,
    model: &str,
    take: u32,
    item: &ItemRequest,
    memo: bool,
) -> Result<(String, CallMeter), ItemError> {
    let body = match kind {
        DecisionKind::Noul => noul_body(model, item)?,
        DecisionKind::Choice => choice_body(model, item)?,
    };
    if !memo {
        return fetch_decision(server, operation, tenant, model, kind, &body, item.node.clone());
    }
    // Take index is part of the memo key so stacked repeats each call the
    // provider. It is not sent: the Decisions API has no take field.
    // A live Jev run still varied by about 0.03 across same-model takes.
    // Temperature and max_tokens are also not sent and are not part of
    // this key.
    let key = format!("{tenant}\n{take}\n{body}");
    let started = Instant::now();
    let flight = {
        let mut cache = lock(&server.cache);
        if let Some(text) = cache.decision_memo.get(&key).cloned() {
            // Same event as a contract-cache hit: `/stats` hits, the call
            // log, and `latency_by_op_model.hits` all count it. The saved
            // cost stays on this meter. A later contract-cache hit of a
            // value produced only from the memo does not add it again.
            cache.hits += 1;
            drop(cache);
            let usage = usage_from_body(&text);
            let meter = saved_meter(
                operation,
                tenant,
                model,
                &usage,
                "hit",
                started,
                item.node.clone(),
                decision_score(kind, &text),
            );
            record_call(server, &meter);
            return Ok((text, meter));
        }
        if let Some(existing) = cache.decision_inflight.get(&key).cloned() {
            cache.single_flight_waits += 1;
            drop(cache);
            let text = wait_decision(&existing)?;
            let usage = usage_from_body(&text);
            let meter = saved_meter(
                operation,
                tenant,
                model,
                &usage,
                "single-flight",
                started,
                item.node.clone(),
                decision_score(kind, &text),
            );
            record_call(server, &meter);
            return Ok((text, meter));
        }
        let flight = Arc::new(DecisionFlight {
            lock: Mutex::new(None),
            cv: Condvar::new(),
        });
        cache.decision_inflight.insert(key.clone(), Arc::clone(&flight));
        flight
    };
    let result = fetch_decision(server, operation, tenant, model, kind, &body, item.node.clone()).map(|(text, meter)| {
        let mut cache = lock(&server.cache);
        cache.decision_memo.insert(key.clone(), text.clone());
        (text, meter)
    });
    {
        let mut cache = lock(&server.cache);
        cache.decision_inflight.remove(&key);
    }
    {
        let mut slot = flight.lock.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(match &result {
            Ok((text, _)) => Ok(text.clone()),
            Err(e) => Err(e.clone()),
        });
        flight.cv.notify_all();
    }
    result
}

fn fetch_decision(
    server: &Server,
    operation: Operation,
    tenant: &str,
    model: &str,
    kind: DecisionKind,
    body: &str,
    node: Option<String>,
) -> Result<(String, CallMeter), ItemError> {
    {
        let mut cache = lock(&server.cache);
        // One miss per provider call. Takes and summary chunks each call
        // the provider, so this stays equal to provider_calls. A decision
        // memo hit never reaches here.
        cache.provider_calls += 1;
        cache.misses += 1;
    }
    let started = Instant::now();
    let text = match post_json(&server.cfg, &server.agent, &server.cfg.decisions_url, body) {
        Ok(text) => text,
        Err(message) => {
            let ms = elapsed_ms(started);
            let meter = miss_meter(operation, tenant, model, &ProviderUsage::default(), ms, ms, node.clone(), None);
            record_call(server, &meter);
            return Err(item_err("provider", &message));
        }
    };
    let provider_ms = elapsed_ms(started);
    let mut usage = usage_from_body(&text);
    if usage.model.is_none() {
        usage.model = Some(model.to_string());
    }
    if usage.cost_usd.is_none() {
        fill_generation(server, &mut usage);
    }
    let latency_ms = elapsed_ms(started);
    let meter = miss_meter(
        operation,
        tenant,
        model,
        &usage,
        provider_ms,
        latency_ms,
        node,
        decision_score(kind, &text),
    );
    record_call(server, &meter);
    let parsed = match kind {
        DecisionKind::Noul => parse_noul(&text).map(|_| ()),
        DecisionKind::Choice => parse_choice(&text).map(|_| ()),
    };
    parsed.map_err(map_decision_err)?;
    Ok((text, meter))
}

fn wait_decision(flight: &DecisionFlight) -> Result<String, ItemError> {
    let mut slot = flight.lock.lock().unwrap_or_else(|e| e.into_inner());
    while slot.is_none() {
        slot = flight.cv.wait(slot).unwrap_or_else(|e| e.into_inner());
    }
    slot.clone().expect("decision flight finished")
}

fn map_decision_err(message: String) -> ItemError {
    if message.starts_with("decisions error:") {
        item_err("provider", &message)
    } else {
        item_err("parse", &message)
    }
}

fn noul_body(model: &str, item: &ItemRequest) -> Result<String, ItemError> {
    let input = json_str(&item.inputs, "input").unwrap_or_else(|| item.inputs.to_string());
    let criterion = item
        .criterion
        .clone()
        .or_else(|| json_str(&item.inputs, "criterion"))
        .unwrap_or_default();
    let instructions = decision_instructions(item, &criterion);
    // `state` is the input string. The criterion is in `instructions`
    // once. `noul.criteria` is optional and, when present, must be
    // exactly true/false. Omitting it keeps that object from repeating
    // the question in the provider's prompt.
    let body = serde_json::json!({
        "model": model,
        "state": input,
        "questions": {
            "matches": {
                "type": "noul",
                "instructions": instructions
            }
        }
    });
    serde_json::to_string(&body).map_err(|e| item_err("provider", &e.to_string()))
}

fn choice_body(model: &str, item: &ItemRequest) -> Result<String, ItemError> {
    let labels = item.labels.clone().unwrap_or_default();
    if labels.len() < 2 {
        return Err(item_err(
            "invalid",
            "classify label set needs at least two labels",
        ));
    }
    let input = json_str(&item.inputs, "input").unwrap_or_else(|| item.inputs.to_string());
    let criteria = choice_criteria(&labels, item.label_descriptions.as_deref());
    let body = serde_json::json!({
        "model": model,
        "state": input,
        "questions": {
            "label": {
                "type": "choice",
                "instructions": choice_instructions(item, &labels),
                "criteria": criteria,
            }
        }
    });
    serde_json::to_string(&body).map_err(|e| item_err("provider", &e.to_string()))
}

/// Winning probability below this gets one extra `choice` call.
/// Measured against the mock: a 0.46/0.42 split is a toss-up, and a
/// 0.75 winner is not. Not a live calibration.
const CHOICE_RETRY_BELOW: f64 = 0.60;

/// Second-pass instruction. Short on purpose: the criteria object already
/// carries the descriptions, and a long prompt is billed input tokens.
const CHOICE_RETRY_PROMPT: &str = "Scores were close. Match the description, not a nearby topic.";

/// Decisions `choice.criteria` is an object of label to description.
/// An array is HTTP 400. A blank description is the label itself, except
/// the bigfoot trio, which gets a fixed gloss so a sighting, a known
/// species, and tracks-or-sounds are not the same string. SQL
/// `LABELS ('bigfoot' AS '...')` still sends the custom text.
fn choice_criteria(labels: &[String], descriptions: Option<&[String]>) -> serde_json::Map<String, serde_json::Value> {
    let gloss = bare_bigfoot_gloss(labels, descriptions);
    labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let custom = descriptions
                .and_then(|d| d.get(i))
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            let text = match custom {
                Some(s) => s.to_string(),
                None => gloss
                    .as_ref()
                    .and_then(|rows| rows.get(i))
                    .cloned()
                    .unwrap_or_else(|| label.clone()),
            };
            (label.clone(), serde_json::Value::String(text))
        })
        .collect()
}

/// Gloss for `CLASSIFY(... INTO 'bigfoot', 'known animal', 'unclear')`
/// when every description is blank. Any other label set stays as the
/// label text. Order follows `labels`.
fn bare_bigfoot_gloss(labels: &[String], descriptions: Option<&[String]>) -> Option<Vec<String>> {
    let filled = descriptions.is_some_and(|rows| rows.iter().any(|s| !s.trim().is_empty()));
    if filled || labels.len() != 3 {
        return None;
    }
    let mut names: Vec<String> = labels.iter().map(|s| s.trim().to_ascii_lowercase()).collect();
    names.sort();
    if names != ["bigfoot", "known animal", "unclear"] {
        return None;
    }
    Some(
        labels
            .iter()
            .map(|label| match label.trim().to_ascii_lowercase().as_str() {
                "bigfoot" => "an unidentified large primate or sasquatch",
                "known animal" => "a recognized species such as a deer, bear, or owl",
                _ => "no creature is identified; tracks, smells, or sounds alone",
            })
            .map(str::to_string)
            .collect(),
    )
}

fn choice_instructions(item: &ItemRequest, _labels: &[String]) -> String {
    let prompt = item
        .prompt
        .as_deref()
        .map(|p| substitute(p, &item.inputs))
        .filter(|s| !s.is_empty());
    let system = item.system.clone().filter(|s| !s.is_empty());
    match (system, prompt) {
        (Some(system), Some(prompt)) => format!("{system}\n{prompt}"),
        (Some(system), None) => system,
        (None, Some(prompt)) => prompt,
        // The label list is the criteria object. Repeating it here
        // only adds input tokens.
        (None, None) => "Which label fits?".into(),
    }
}

fn decision_instructions(item: &ItemRequest, criterion: &str) -> String {
    let prompt = item
        .prompt
        .as_deref()
        .map(|p| substitute(p, &item.inputs))
        .filter(|s| !s.is_empty());
    let system = item.system.clone().filter(|s| !s.is_empty());
    let base = match (system, prompt) {
        (Some(system), Some(prompt)) => format!("{system}\n{prompt}"),
        (Some(system), None) => system,
        (None, Some(prompt)) => prompt,
        (None, None) => String::new(),
    };
    if base.is_empty() {
        if criterion.is_empty() {
            "yes or no".into()
        } else {
            criterion.to_string()
        }
    } else if !criterion.is_empty() && !base.contains(criterion) {
        format!("{base}\n{criterion}")
    } else {
        base
    }
}

/// Whitespace-delimited words, with each punctuation mark counted alone.
/// Used to compare prompt scaffolds. Not the provider's tokenizer.
#[cfg(test)]
fn rough_tokens(text: &str) -> usize {
    let mut n = 0usize;
    let mut in_word = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if !in_word {
                n += 1;
                in_word = true;
            }
        } else {
            in_word = false;
            if !c.is_whitespace() {
                n += 1;
            }
        }
    }
    n
}

/// `answers.matches.noul` is P(yes), from 0 to 1. Published noul answers
/// do not include a separate confidence field.
pub fn parse_noul(body: &str) -> Result<f64, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("decisions json: {e}"))?;
    if let Some(msg) = v.pointer("/error/message").and_then(|m| m.as_str()) {
        return Err(format!("decisions error: {msg}"));
    }
    let answer = v
        .pointer("/answers/matches")
        .ok_or("decisions response has no answers.matches")?;
    if let Some(kind) = answer.get("type").and_then(|t| t.as_str()) {
        if kind != "noul" {
            return Err(format!("decisions answers.matches type is {kind}, expected noul"));
        }
    }
    let noul = answer
        .get("noul")
        .and_then(|n| n.as_f64())
        .ok_or("decisions answers.matches.noul is missing")?;
    if !noul.is_finite() {
        return Err("decisions noul is not a finite number".into());
    }
    Ok(noul)
}

/// `answers.label.choice` plus `probabilities`. `confidence` is not
/// P(winning label). It is used only when the probability map omits the
/// published choice.
pub fn parse_choice(body: &str) -> Result<ChoiceAnswer, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("decisions json: {e}"))?;
    if let Some(msg) = v.pointer("/error/message").and_then(|m| m.as_str()) {
        return Err(format!("decisions error: {msg}"));
    }
    let answer = v
        .pointer("/answers/label")
        .ok_or("decisions response has no answers.label")?;
    if let Some(kind) = answer.get("type").and_then(|t| t.as_str()) {
        if kind != "choice" {
            return Err(format!("decisions answers.label type is {kind}, expected choice"));
        }
    }
    let choice = answer.get("choice").and_then(|c| c.as_str()).map(|s| s.to_string());
    let confidence = answer
        .get("confidence")
        .and_then(|n| n.as_f64())
        .filter(|n| n.is_finite());
    let mut probabilities = HashMap::new();
    if let Some(map) = answer.get("probabilities").and_then(|p| p.as_object()) {
        for (key, val) in map {
            if let Some(n) = val.as_f64().filter(|n| n.is_finite()) {
                probabilities.insert(key.clone(), n);
            }
        }
    }
    if let Some(choice) = choice.as_ref() {
        if !probabilities.contains_key(choice) {
            if let Some(conf) = confidence {
                probabilities.insert(choice.clone(), conf);
            }
        }
    }
    if probabilities.is_empty() {
        return Err("decisions answers.label has no probabilities".into());
    }
    Ok(ChoiceAnswer { probabilities })
}

pub struct ChoiceAnswer {
    pub probabilities: HashMap<String, f64>,
}

/// EXTRACT MANY is sampled at seed 0. Temperature 0 alone still moved
/// between `black dog` and `dog` across two cache-off passes.
fn chat_seed(extract_mode: Option<&str>) -> Option<u32> {
    if extract_mode == Some("many") {
        Some(0)
    } else {
        None
    }
}

fn chat(
    server: &Server,
    operation: Operation,
    tenant: &str,
    model: &str,
    temperature: f64,
    max_tokens: Option<u32>,
    system: &str,
    user: &str,
    node: Option<String>,
    response_schema: Option<&serde_json::Value>,
    seed: Option<u32>,
) -> Result<(String, CallMeter), String> {
    match chat_once(
        server,
        operation,
        tenant,
        model,
        temperature,
        max_tokens,
        system,
        user,
        node.clone(),
        response_schema,
        seed,
    ) {
        Err(message) if response_schema.is_some() && schema_rejected(&message) => chat_once(
            server,
            operation,
            tenant,
            model,
            temperature,
            max_tokens,
            system,
            user,
            node,
            None,
            seed,
        ),
        other => other,
    }
}

/// A model that cannot do strict JSON Schema still has the schema in the
/// prompt. One retry without `response_format` keeps the row from failing
/// closed into an empty extract.
fn schema_rejected(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("response_format")
        || message.contains("json_schema")
        || message.contains("structured output")
        || message.contains("invalid schema")
}

fn chat_payload(
    model: &str,
    temperature: f64,
    max_tokens: Option<u32>,
    system: &str,
    user: &str,
    response_schema: Option<&serde_json::Value>,
    seed: Option<u32>,
) -> serde_json::Value {
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
    if let Some(seed) = seed {
        body["seed"] = serde_json::json!(seed);
    }
    if let Some(schema) = response_schema {
        body["response_format"] = serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": "extract_many",
                "strict": true,
                "schema": schema
            }
        });
    }
    body
}

fn chat_once(
    server: &Server,
    operation: Operation,
    tenant: &str,
    model: &str,
    temperature: f64,
    max_tokens: Option<u32>,
    system: &str,
    user: &str,
    node: Option<String>,
    response_schema: Option<&serde_json::Value>,
    seed: Option<u32>,
) -> Result<(String, CallMeter), String> {
    let cfg = &server.cfg;
    let key = cfg.api_key.as_deref().unwrap_or("");
    let url = format!(
        "{}/chat/completions",
        cfg.openai_base.trim_end_matches('/')
    );
    let body = chat_payload(model, temperature, max_tokens, system, user, response_schema, seed);
    let payload = match serde_json::to_string(&body) {
        Ok(s) => s,
        Err(e) => return Err(redact(&e.to_string(), key)),
    };
    {
        let mut cache = lock(&server.cache);
        // Chat takes and summary chunks each increment both counters.
        cache.provider_calls += 1;
        cache.misses += 1;
    }
    let started = Instant::now();
    let text = match post_json(cfg, &server.agent, &url, &payload) {
        Ok(text) => text,
        Err(message) => {
            let ms = elapsed_ms(started);
            let meter = miss_meter(operation, tenant, model, &ProviderUsage::default(), ms, ms, node.clone(), None);
            record_call(server, &meter);
            return Err(message);
        }
    };
    let provider_ms = elapsed_ms(started);
    let mut usage = usage_from_body(&text);
    if usage.model.is_none() {
        usage.model = Some(model.to_string());
    }
    if usage.cost_usd.is_none() {
        fill_generation(server, &mut usage);
    }
    let latency_ms = elapsed_ms(started);
    let meter = miss_meter(operation, tenant, model, &usage, provider_ms, latency_ms, node, None);
    record_call(server, &meter);
    let content = completion_text(&text).map_err(|e| redact(&e, key))?;
    Ok((content, meter))
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn miss_meter(
    operation: Operation,
    tenant: &str,
    requested_model: &str,
    usage: &ProviderUsage,
    provider_ms: u64,
    latency_ms: u64,
    node: Option<String>,
    score: Option<f64>,
) -> CallMeter {
    CallMeter {
        op: op_name(operation).into(),
        model: usage
            .model
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| requested_model.to_string()),
        tokens_in: usage.tokens_in,
        tokens_out: usage.tokens_out,
        cost_usd: usage.cost_usd,
        cost_saved_usd: Some(0.0),
        provider_latency_ms: Some(provider_ms),
        latency_ms,
        cache: "miss".into(),
        items: 1,
        tenant: tenant.to_string(),
        timestamp: rfc3339_now(),
        generation_id: usage.generation_id.clone(),
        node,
        score,
        identical: None,
        payload: audit_payload(),
        retake: None,
    }
}

/// A decision-memo hit or single-flight waiter. Cost is 0. The saved
/// amount is the original `usage.cost`. Tokens are 0 because the miss
/// already recorded the billed tokens.
fn saved_meter(
    operation: Operation,
    tenant: &str,
    requested_model: &str,
    usage: &ProviderUsage,
    cache: &str,
    started: Instant,
    node: Option<String>,
    score: Option<f64>,
) -> CallMeter {
    CallMeter {
        op: op_name(operation).into(),
        model: usage
            .model
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| requested_model.to_string()),
        tokens_in: Some(0),
        tokens_out: Some(0),
        cost_usd: Some(0.0),
        cost_saved_usd: usage.cost_usd,
        provider_latency_ms: None,
        latency_ms: elapsed_ms(started),
        cache: cache.into(),
        items: 1,
        tenant: tenant.to_string(),
        timestamp: rfc3339_now(),
        generation_id: usage.generation_id.clone(),
        node,
        score,
        identical: None,
        payload: audit_payload(),
        retake: None,
    }
}

/// Contract-cache hit or single-flight. `cost_saved_usd` is the USD the
/// misses charged while producing the stored value. A value that was
/// itself served from the decision memo has no produced cost, so the
/// savings stay on the memo hit and are not counted again.
fn meter_cached(
    server: &Server,
    operation: Operation,
    tenant: &str,
    stored: &Scored,
    cache: &str,
    started: Instant,
    node: Option<String>,
) -> Scored {
    let meter = CallMeter {
        op: op_name(operation).into(),
        model: stored.model.clone(),
        tokens_in: Some(0),
        tokens_out: Some(0),
        cost_usd: Some(0.0),
        cost_saved_usd: stored.produced_cost_usd,
        provider_latency_ms: None,
        latency_ms: elapsed_ms(started),
        cache: cache.into(),
        items: 1,
        tenant: tenant.to_string(),
        timestamp: rfc3339_now(),
        generation_id: stored.generation_id.clone(),
        node,
        score: stored.score,
        identical: stored.identical_takes,
        payload: audit_payload(),
        retake: None,
    };
    record_call(server, &meter);
    let mut scored = stored.clone();
    scored.calls = vec![meter];
    scored
}

fn record_call(server: &Server, meter: &CallMeter) {
    let key = server.cfg.api_key.as_deref().unwrap_or("");
    eprintln!("{}", meter_log_line(meter, key));
    let mut cache = lock(&server.cache);
    if let Some(cost) = meter.cost_usd.filter(|n| n.is_finite()) {
        cache.total_cost_usd += cost;
    }
    if let Some(saved) = meter.cost_saved_usd.filter(|n| n.is_finite()) {
        cache.cost_saved_usd += saved;
    }
    if meter.cache == "miss" {
        *cache
            .latency_misses
            .entry((meter.op.clone(), meter.model.clone()))
            .or_default() += 1;
        if let Some(ms) = meter.provider_latency_ms {
            let samples = cache
                .latency_samples
                .entry((meter.op.clone(), meter.model.clone()))
                .or_default();
            if samples.len() == LATENCY_SAMPLE_CAP {
                samples.pop_front();
            }
            samples.push_back(ms);
        }
    } else if meter.cache == "hit" {
        *cache
            .latency_hits
            .entry((meter.op.clone(), meter.model.clone()))
            .or_default() += 1;
    }
    if cache.calls.len() == rvbbit_contract::CALL_LOG_CAP {
        cache.calls.pop_front();
    }
    cache.calls.push_back(meter.clone());
}

/// The call log is written inside the provider fetch, before a stack
/// knows whether its takes agreed. Copy `identical` onto those rows.
/// `Some(false)` serializes; leaving the field absent is what made the
/// SQL column null.
fn stamp_logged_identical(server: &Server, calls: &[CallMeter]) {
    let mut cache = lock(&server.cache);
    for logged in cache.calls.iter_mut() {
        if let Some(src) = calls.iter().find(|c| meter_identity(logged, c)) {
            logged.identical = src.identical;
        }
    }
}

fn meter_identity(logged: &CallMeter, src: &CallMeter) -> bool {
    logged.timestamp == src.timestamp
        && logged.op == src.op
        && logged.model == src.model
        && logged.cache == src.cache
        && logged.latency_ms == src.latency_ms
        && logged.generation_id == src.generation_id
        && logged.cost_usd == src.cost_usd
        && logged.score == src.score
        && logged.node == src.node
        && logged.tenant == src.tenant
}

fn fill_generation(server: &Server, usage: &mut ProviderUsage) {
    let Some(id) = usage.generation_id.clone() else {
        return;
    };
    if !generation_id_ok(&id) {
        return;
    }
    let url = format!(
        "{}/generation?id={id}",
        server.cfg.openai_base.trim_end_matches('/')
    );
    if let Ok(body) = get_json(&server.cfg, &server.agent, &url) {
        merge_generation(usage, &body);
    }
}

fn post_json(cfg: &Config, agent: &ureq::Agent, url: &str, payload: &str) -> Result<String, String> {
    with_transport_retry(|| post_json_once(cfg, agent, url, payload))
}

fn get_json(cfg: &Config, agent: &ureq::Agent, url: &str) -> Result<String, String> {
    with_transport_retry(|| get_json_once(cfg, agent, url))
}

/// Retry a dropped TLS session or a reset connection. An HTTP status,
/// including 4xx, is the provider's answer and is not retried.
fn with_transport_retry(mut once: impl FnMut() -> Result<String, String>) -> Result<String, String> {
    let mut attempt = 0u32;
    loop {
        match once() {
            Ok(body) => return Ok(body),
            Err(message) if retryable_transport(&message) && attempt + 1 < TRANSPORT_ATTEMPTS => {
                let pause = TRANSPORT_BACKOFF_MS[attempt as usize];
                attempt += 1;
                thread::sleep(Duration::from_millis(pause));
            }
            Err(message) => return Err(message),
        }
    }
}

fn retryable_transport(message: &str) -> bool {
    message.starts_with("upstream transport")
}

fn post_json_once(cfg: &Config, agent: &ureq::Agent, url: &str, payload: &str) -> Result<String, String> {
    let key = cfg.api_key.as_deref().unwrap_or("");
    let send = agent
        .post(url)
        .timeout(cfg.timeout)
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {key}"))
        .send_string(payload);
    match send {
        Ok(resp) => resp.into_string().map_err(|e| redact(&e.to_string(), key)),
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp.into_string().unwrap_or_default();
            Err(redact(&format!("upstream HTTP {code}: {detail}"), key))
        }
        Err(e) => Err(redact(&format!("upstream transport: {e}"), key)),
    }
}

fn get_json_once(cfg: &Config, agent: &ureq::Agent, url: &str) -> Result<String, String> {
    let key = cfg.api_key.as_deref().unwrap_or("");
    let send = agent
        .get(url)
        .timeout(cfg.timeout)
        .set("Authorization", &format!("Bearer {key}"))
        .call();
    match send {
        Ok(resp) => resp.into_string().map_err(|e| redact(&e.to_string(), key)),
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

/// A bare array is the mock and any model that ignores the schema.
/// Strict structured output is an object with `mentions`.
fn many_mentions(value: &serde_json::Value) -> Result<&Vec<serde_json::Value>, String> {
    if let Some(arr) = value.as_array() {
        return Ok(arr);
    }
    if let Some(arr) = value.get("mentions").and_then(|v| v.as_array()) {
        return Ok(arr);
    }
    Err("extract many expected a JSON array of strings, or an object with a mentions array".into())
}

fn parse_extract(text: &str, mode: Option<&str>) -> Result<serde_json::Value, String> {
    match mode.unwrap_or("json") {
        "json" => parse_json_value(text),
        "scalar" => parse_scalar_value(text),
        "summary" => {
            let stripped = strip_fences(text.trim());
            if stripped.is_empty() {
                Err("summary was empty".into())
            } else {
                Ok(serde_json::Value::String(stripped.to_string()))
            }
        }
        "many" => {
            let value = parse_json_value(text)?;
            let arr = many_mentions(&value)?;
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                match item {
                    serde_json::Value::String(s) => {
                        let s = s.trim();
                        if !s.is_empty() {
                            out.push(serde_json::Value::String(s.to_string()));
                        }
                    }
                    serde_json::Value::Object(map) => {
                        let mention = map
                            .get("mention")
                            .and_then(|t| t.as_str())
                            .or_else(|| map.get("text").and_then(|t| t.as_str()))
                            .unwrap_or("")
                            .trim();
                        if mention.is_empty() {
                            continue;
                        }
                        match map.get("category").and_then(|c| c.as_str()).map(str::trim) {
                            Some(category) if !category.is_empty() => {
                                out.push(serde_json::json!({ "mention": mention, "category": category }));
                            }
                            _ => out.push(serde_json::Value::String(mention.to_string())),
                        }
                    }
                    _ => {
                        return Err(
                            "extract many items must be strings or {mention, category} objects".into(),
                        )
                    }
                }
            }
            Ok(serde_json::Value::Array(out))
        }
        other => Err(format!("extract mode {other} is not json, scalar, summary, or many")),
    }
}

fn plan_is_wide(req: &BatchRequest) -> bool {
    let Some(stack) = &req.decision else {
        return false;
    };
    stack.takes.unwrap_or(1) > 1 || stack.models.len() > 1 || !stack.phrasings.is_empty()
}

/// Chat extract with TAKES, stacked models, or phrasings. The winner is
/// the majority text. Same-model takes that disagree log `identical` false.
fn extract_fanout(
    server: &Server,
    tenant: &str,
    operation: Operation,
    req: &BatchRequest,
    item: &ItemRequest,
    memo: bool,
) -> Result<Scored, ItemError> {
    let stack = req.decision.clone().unwrap_or(DecisionStack {
        models: Vec::new(),
        takes: None,
        combine: None,
        threshold: None,
        phrasings: Vec::new(),
        pick: None,
    });
    let mut models: Vec<String> = stack
        .models
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| choose_model(&server.cfg, &s))
        .collect();
    if models.is_empty() {
        models.push(choose_model(&server.cfg, &req.model));
    }
    let takes = stack.takes.unwrap_or(1).clamp(1, DECISION_TAKES_CAP);
    let phrasings = if stack.phrasings.is_empty() {
        vec![String::new()]
    } else {
        stack.phrasings
    };
    let mut texts = Vec::new();
    let mut calls = Vec::new();
    let mut prints: Vec<(String, String)> = Vec::new();
    for model in &models {
        for phrasing in &phrasings {
            let mut asked = item.clone();
            if !phrasing.is_empty() {
                asked.prompt = Some(phrasing.clone());
            }
            for _take in 0..takes {
                let (system, user) = prompts(operation, &asked);
                let max_tokens = req.max_tokens.or(Some(256));
                let (text, meter) = chat(
                    server,
                    operation,
                    tenant,
                    model,
                    req.temperature,
                    max_tokens,
                    &system,
                    &user,
                    item.node.clone(),
                    item.response_schema.as_ref(),
                    chat_seed(item.extract_mode.as_deref()),
                )
                .map_err(|message| item_err("provider", &message))?;
                let _ = memo;
                prints.push((format!("{model}\n{phrasing}"), text.trim().to_string()));
                texts.push(text);
                calls.push(meter);
            }
        }
    }
    let identical = identical_prints(&prints);
    if prints.len() >= 2 {
        for call in &mut calls {
            call.identical = Some(identical);
        }
        stamp_logged_identical(server, &calls);
    }
    let winner = majority_text(&texts);
    let value = parse_extract(&winner, item.extract_mode.as_deref())
        .map_err(|message| item_err("parse", &message))?;
    let mut scored = Scored::from_calls(value, None, None, calls);
    if prints.len() >= 2 {
        scored.identical_takes = Some(identical);
    }
    Ok(scored)
}

fn majority_text(texts: &[String]) -> String {
    if texts.is_empty() {
        return String::new();
    }
    let mut best = &texts[0];
    let mut best_n = 0usize;
    for candidate in texts {
        let n = texts.iter().filter(|t| *t == candidate).count();
        if n > best_n {
            best = candidate;
            best_n = n;
        }
    }
    best.clone()
}

/// Typed scalar extract. A JSON string, number, bool, or null is that
/// value. Any other non-empty answer is the trimmed text, so a model
/// that returns `sasquatch` instead of a JSON string still caches.
/// A JSON object or array is an error: that is the json mode's job.
pub fn parse_scalar_value(text: &str) -> Result<serde_json::Value, String> {
    let stripped = strip_fences(text.trim());
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stripped) {
        return match v {
            serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                Err("scalar extract got a JSON object or array".into())
            }
            other => Ok(other),
        };
    }
    let trimmed = stripped.trim();
    if trimmed.is_empty() {
        return Err("scalar extract was empty".into());
    }
    Ok(serde_json::Value::String(unwrap_quotes(trimmed).to_string()))
}

fn unwrap_quotes(text: &str) -> &str {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 {
        let quote = bytes[0];
        if (quote == b'"' || quote == b'\'') && bytes[bytes.len() - 1] == quote {
            return &text[1..text.len() - 1];
        }
    }
    text
}

/// Noul value, or the highest choice probability. Absent when the body
/// is not a decision answer.
fn decision_score(kind: DecisionKind, text: &str) -> Option<f64> {
    match kind {
        DecisionKind::Noul => parse_noul(text).ok(),
        DecisionKind::Choice => parse_choice(text).ok().and_then(|hit| {
            hit.probabilities
                .values()
                .copied()
                .filter(|n| n.is_finite())
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        }),
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
    // `record_call` takes the cache lock. Collect meters, drop the lock,
    // then record so a lookup hit cannot deadlock on itself.
    let mut credited = Vec::new();
    {
        let mut cache = lock(&server.cache);
        for key in &req.keys {
            let storage = storage_of(key, tenant);
            if let Some(hit) = fresh(server, &mut cache, &storage) {
                cache.hits += 1;
                if hit.op.is_empty() {
                    // A put, or a file from before op was stored. Credit
                    // the dollars when we have them, and do not invent a
                    // blank latency row.
                    if let Some(saved) = hit.produced_cost_usd.filter(|n| n.is_finite()) {
                        cache.cost_saved_usd += saved;
                    }
                } else {
                    credited.push(lookup_hit_meter(tenant, &hit));
                }
                hits.push(CacheHit {
                    key: key.clone(),
                    output: hit.value,
                    score: hit.score,
                });
            } else {
                // The key is absent. The follow-up batch counts the miss
                // if it actually calls the provider. Counting it here made
                // 32 calls look like 64 misses.
                misses.push(key.clone());
            }
        }
    }
    for meter in &credited {
        record_call(server, meter);
    }
    serde_json::to_string(&CacheLookupResponse { hits, misses }).unwrap_or_else(|_| "{}".into())
}

fn lookup_hit_meter(tenant: &str, stored: &Scored) -> CallMeter {
    CallMeter {
        op: stored.op.clone(),
        model: stored.model.clone(),
        tokens_in: Some(0),
        tokens_out: Some(0),
        cost_usd: Some(0.0),
        cost_saved_usd: stored.produced_cost_usd,
        provider_latency_ms: None,
        latency_ms: 0,
        cache: "hit".into(),
        items: 1,
        tenant: tenant.to_string(),
        timestamp: rfc3339_now(),
        generation_id: stored.generation_id.clone(),
        node: None,
        score: stored.score,
        identical: stored.identical_takes,
        payload: audit_payload(),
        retake: None,
    }
}

fn put(server: &Server, tenant: &str, req: &CachePutRequest) -> String {
    let mut stored = 0u32;
    for entry in &req.entries {
        let storage = storage_of(&entry.key, tenant);
        let ttl = entry
            .ttl_secs
            .map(Duration::from_secs)
            .unwrap_or(server.cfg.default_ttl);
        store(
            server,
            &storage,
            &Scored::bare(entry.output.clone(), None, None),
            ttl,
        );
        stored += 1;
    }
    serde_json::to_string(&CachePutResponse { stored }).unwrap_or_else(|_| "{}".into())
}

fn purge(server: &Server, tenant: &str, req: &CachePurgeRequest) -> String {
    let mut cache = lock(&server.cache);
    let before = cache.entries.len();
    let purged = if req.all {
        cache.entries.clear();
        cache.decision_memo.clear();
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

fn store(server: &Server, storage: &str, scored: &Scored, ttl: Duration) {
    if server.cfg.cache_store == "off" {
        return;
    }
    let expires = if ttl.is_zero() { None } else { Some(Instant::now() + ttl) };
    let mut kept = scored.clone();
    kept.calls.clear();
    if server.cfg.cache_store == "persistent" {
        let ttl_ms = if ttl.is_zero() { None } else { Some(ttl.as_millis()) };
        persist::save(
            std::path::Path::new(&server.cfg.cache_dir),
            storage,
            &persist::DiskEntry {
                value: kept.value.clone(),
                score: kept.score,
                op: kept.op.clone(),
                model: kept.model.clone(),
                produced_cost_usd: kept.produced_cost_usd,
            },
            ttl_ms,
        );
    }
    let mut cache = lock(&server.cache);
    cache.entries.insert(storage.to_string(), CacheEntry { scored: kept, expires });
}

fn fresh(server: &Server, cache: &mut Cache, storage: &str) -> Option<Scored> {
    if server.cfg.cache_store == "off" {
        return None;
    }
    let memory = cache.entries.get(storage).map(|entry| {
        let expired = entry.expires.is_some_and(|exp| Instant::now() >= exp);
        (expired, entry.scored.clone())
    });
    if let Some((expired, scored)) = memory {
        if !expired {
            return Some(scored);
        }
        cache.entries.remove(storage);
    }
    if server.cfg.cache_store != "persistent" {
        return None;
    }
    let loaded = persist::load(std::path::Path::new(&server.cfg.cache_dir), storage)?;
    let scored = Scored::from_disk(loaded);
    cache.entries.insert(
        storage.to_string(),
        CacheEntry {
            scored: scored.clone(),
            expires: Some(Instant::now() + server.cfg.default_ttl),
        },
    );
    Some(scored)
}

fn storage_for(
    client_hex: &str,
    tenant: &str,
    plan: Option<&DecisionPlan>,
    labels: &[String],
    descriptions: &[String],
) -> String {
    // Cache key v2 already carries threshold, extract mode, labels, and
    // descriptions. Lookup addresses that client key. A change to the
    // server-side default `HUTCH_DECISION_THRESHOLD` needs `POST /reset`
    // (or `HUTCH_CACHE=off`) before it applies to stored booleans.
    let _ = (plan, labels, descriptions);
    storage_of(client_hex, tenant)
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
        let mut latency = Vec::new();
        let mut keys = BTreeSet::new();
        keys.extend(cache.latency_samples.keys().cloned());
        keys.extend(cache.latency_hits.keys().cloned());
        keys.extend(cache.latency_misses.keys().cloned());
        for (op, model) in keys {
            let values: Vec<u64> = cache
                .latency_samples
                .get(&(op.clone(), model.clone()))
                .map(|samples| samples.iter().copied().collect())
                .unwrap_or_default();
            let hits = cache
                .latency_hits
                .get(&(op.clone(), model.clone()))
                .copied()
                .unwrap_or(0);
            let calls = cache
                .latency_misses
                .get(&(op.clone(), model.clone()))
                .copied()
                .unwrap_or(0);
            latency.push(serde_json::json!({
                "op": op,
                "model": model,
                "p50_ms": percentile(&values, 0.50),
                "p95_ms": percentile(&values, 0.95),
                "calls": calls,
                "hits": hits,
            }));
        }
        latency.sort_by(|a, b| {
            (a["op"].as_str(), a["model"].as_str()).cmp(&(b["op"].as_str(), b["model"].as_str()))
        });
        return serde_json::json!({
            "entries": entries,
            "hits": cache.hits,
            "misses": cache.misses,
            "single_flight_waits": cache.single_flight_waits,
            "provider_calls": cache.provider_calls,
            "requests": cache.requests,
            "calls": cache.requests,
            "rows": cache.rows,
            "max_batch": cache.max_batch,
            "total_cost_usd": cache.total_cost_usd,
            "cost_saved_usd": cache.cost_saved_usd,
            "latency_by_op_model": latency,
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

fn calls_limit(query: &str) -> usize {
    let mut limit = 50usize;
    for part in query.split('&') {
        let Some(value) = part.strip_prefix("limit=") else {
            continue;
        };
        if let Ok(n) = value.parse::<usize>() {
            limit = n.clamp(1, rvbbit_contract::CALL_LOG_CAP);
        }
    }
    limit
}

fn calls_json(server: &Server, limit: usize) -> String {
    let cache = lock(&server.cache);
    let start = cache.calls.len().saturating_sub(limit);
    let calls: Vec<&CallMeter> = cache.calls.iter().skip(start).collect();
    serde_json::json!({ "calls": calls }).to_string()
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
    fn transport_errors_retry_and_http_status_does_not() {
        assert!(retryable_transport(
            "upstream transport: connection closed via TLS"
        ));
        assert!(retryable_transport("upstream transport: tls handshake eof"));
        assert!(!retryable_transport("upstream HTTP 400: bad request"));
        assert!(!retryable_transport("upstream HTTP 429: rate limit"));
        assert!(!retryable_transport("upstream error: connection closed"));
    }

    #[test]
    fn extract_many_keeps_only_strings_and_allows_an_empty_list() {
        assert_eq!(parse_extract("[]", Some("many")).unwrap(), serde_json::json!([]));
        assert_eq!(
            parse_extract("[\"sasquatch\", \"\", \"  \"]", Some("many")).unwrap(),
            serde_json::json!(["sasquatch"])
        );
        assert!(parse_extract("[\"sasquatch\", 1]", Some("many")).is_err());
        assert!(parse_extract("{\"creature\":\"sasquatch\"}", Some("many")).is_err());
        assert!(parse_extract("a truck in the woods", Some("many")).is_err());
        assert_eq!(
            parse_extract("{\"mentions\":[\"Bigfoot\",\"gray\"]}", Some("many")).unwrap(),
            serde_json::json!(["Bigfoot", "gray"])
        );
        assert_eq!(
            parse_extract("{\"mentions\":[]}", Some("many")).unwrap(),
            serde_json::json!([])
        );
        assert_eq!(
            parse_extract(
                "{\"mentions\":[{\"text\":\"Bigfoot\",\"proper\":true},{\"text\":\"animal\",\"proper\":false}]}",
                Some("many")
            )
            .unwrap(),
            serde_json::json!(["Bigfoot", "animal"])
        );
        assert_eq!(
            parse_extract(
                "{\"mentions\":[\
                    {\"mention\":\"Bigfoot\",\"category\":\"specific_instance_of_kind\"},\
                    {\"mention\":\"hair\",\"category\":\"attribute_or_part\"},\
                    {\"mention\":\"Alabama\",\"category\":\"place\"},\
                    {\"mention\":\"growl type high pitch yell\",\"category\":\"sound_or_event\"}\
                ]}",
                Some("many")
            )
            .unwrap(),
            serde_json::json!([
                {"mention":"Bigfoot","category":"specific_instance_of_kind"},
                {"mention":"hair","category":"attribute_or_part"},
                {"mention":"Alabama","category":"place"},
                {"mention":"growl type high pitch yell","category":"sound_or_event"}
            ])
        );
    }

    #[test]
    fn extract_many_chat_sends_temperature_zero_and_a_strict_schema() {
        let schema = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["mentions"],
            "properties": { "mentions": { "type": "array", "items": { "type": "string" } } }
        });
        let body = chat_payload(
            "openai/gpt-4o-mini",
            0.0,
            Some(256),
            "extract",
            "text",
            Some(&schema),
            Some(0),
        );
        assert_eq!(body["temperature"].as_f64(), Some(0.0));
        assert_eq!(body["seed"].as_u64(), Some(0));
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(body["response_format"]["json_schema"]["name"], "extract_many");
        assert_eq!(body["response_format"]["json_schema"]["schema"]["required"][0], "mentions");
        let plain = chat_payload("openai/gpt-4o-mini", 0.0, None, "extract", "text", None, None);
        assert!(plain.get("response_format").is_none());
        assert!(plain.get("seed").is_none());
        assert_eq!(chat_seed(Some("many")), Some(0));
        assert_eq!(chat_seed(Some("summary")), None);
    }

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

    #[test]
    fn decisions_url_strips_the_chat_suffix() {
        assert_eq!(
            decisions_url_from_base("https://openrouter.ai/api/v1"),
            "https://openrouter.ai/api/alpha/decisions"
        );
        assert_eq!(
            decisions_url_from_base("http://127.0.0.1:9/v1"),
            "http://127.0.0.1:9/alpha/decisions"
        );
    }

    #[test]
    fn choice_criteria_is_an_object_of_descriptions() {
        let labels = vec!["bigfoot".into(), "unclear".into()];
        let map = choice_criteria(&labels, Some(&["an unidentified large primate".into(), " ".into()]));
        assert_eq!(map["bigfoot"], serde_json::json!("an unidentified large primate"));
        assert_eq!(map["unclear"], serde_json::json!("unclear"));
        let plain = choice_criteria(&labels, None);
        assert_eq!(plain["bigfoot"], serde_json::json!("bigfoot"));
        let trio = vec!["bigfoot".into(), "known animal".into(), "unclear".into()];
        let gloss = choice_criteria(&trio, None);
        assert_eq!(gloss["bigfoot"], serde_json::json!("an unidentified large primate or sasquatch"));
        assert_eq!(
            gloss["known animal"],
            serde_json::json!("a recognized species such as a deer, bear, or owl")
        );
        assert_eq!(
            gloss["unclear"],
            serde_json::json!("no creature is identified; tracks, smells, or sounds alone")
        );
        let custom = choice_criteria(&trio, Some(&["CUSTOM".into(), String::new(), String::new()]));
        assert_eq!(custom["bigfoot"], serde_json::json!("CUSTOM"));
        assert_eq!(custom["unclear"], serde_json::json!("unclear"));
    }

    fn demo_item(input: &str, criterion: &str) -> ItemRequest {
        ItemRequest {
            key: "0".into(),
            cache_key: "00".into(),
            inputs: serde_json::json!({"input": input, "criterion": criterion}),
            criterion: Some(criterion.into()),
            system: None,
            prompt: Some("Does the text match the criterion?".into()),
            labels: None,
            label_descriptions: None,
            node: None,
            extract_mode: None,
            response_schema: None,
        }
    }

    /// Frozen body from before the scaffold trim. State repeated the
    /// criterion, and `criteria.true` repeated it again.
    fn legacy_noul_body(model: &str, input: &str, criterion: &str, instructions: &str) -> String {
        serde_json::json!({
            "model": model,
            "state": { "text": input, "criterion": criterion },
            "questions": {
                "matches": {
                    "type": "noul",
                    "instructions": instructions,
                    "criteria": {
                        "true": criterion,
                        "false": "The text does not match the criterion."
                    }
                }
            }
        })
        .to_string()
    }

    fn legacy_choice_body(model: &str, input: &str, labels: &[&str]) -> String {
        let mut criteria = serde_json::Map::new();
        for label in labels {
            criteria.insert(
                (*label).to_string(),
                serde_json::json!(format!("The text is best described as {label}.")),
            );
        }
        serde_json::json!({
            "model": model,
            "state": { "text": input },
            "questions": {
                "label": {
                    "type": "choice",
                    "instructions": "Which one label fits the text? Labels: bigfoot, known animal, unclear.",
                    "criteria": criteria,
                }
            }
        })
        .to_string()
    }

    fn scaffold_tokens(body: &str, input: &str) -> usize {
        rough_tokens(body).saturating_sub(rough_tokens(input))
    }

    #[test]
    fn noul_scaffold_is_smaller_than_the_legacy_body() {
        let model = "typesafe/jev-1.14";
        let input = "A tall hairy figure crossed the logging road on two legs and left huge prints.";
        let criterion = "a large hairy biped was seen";
        let item = demo_item(input, criterion);
        let new_body = noul_body(model, &item).unwrap();
        let old_body = legacy_noul_body(
            model,
            input,
            criterion,
            "Does the text match the criterion?",
        );
        let old_scaffold = scaffold_tokens(&old_body, input);
        let new_scaffold = scaffold_tokens(&new_body, input);
        assert_eq!(old_scaffold, 109, "{old_body}");
        assert_eq!(new_scaffold, 63, "{new_body}");
        assert_eq!(rough_tokens(&old_body), 125, "{old_body}");
        assert_eq!(rough_tokens(&new_body), 79, "{new_body}");
        assert!(
            new_scaffold * 10 <= old_scaffold * 6,
            "noul scaffold tokens {old_scaffold} -> {new_scaffold}; full {old} -> {new}",
            old = rough_tokens(&old_body),
            new = rough_tokens(&new_body),
        );
        assert_eq!(new_body.matches(criterion).count(), 1, "{new_body}");
        assert_eq!(old_body.matches(criterion).count(), 2, "{old_body}");
        assert!(new_body.contains(r#""state":"A tall hairy figure"#), "{new_body}");
        assert!(!new_body.contains("criteria"), "{new_body}");
        assert!(!new_body.contains("does not match"), "{new_body}");

        let mut already = item.clone();
        already.prompt = Some(format!("Criterion: {criterion}"));
        let once = noul_body(model, &already).unwrap();
        assert_eq!(once.matches(criterion).count(), 1, "{once}");

        let mut bare = item.clone();
        bare.prompt = None;
        bare.criterion = Some(criterion.into());
        let only = noul_body(model, &bare).unwrap();
        assert_eq!(only.matches(criterion).count(), 1, "{only}");

        let labels = ["bigfoot", "known animal", "unclear"];
        let mut choice = item.clone();
        choice.prompt = None;
        choice.labels = Some(labels.iter().map(|s| (*s).to_string()).collect());
        let new_choice = choice_body(model, &choice).unwrap();
        let old_choice = legacy_choice_body(model, input, &labels);
        let old_choice_scaffold = scaffold_tokens(&old_choice, input);
        let new_choice_scaffold = scaffold_tokens(&new_choice, input);
        assert_eq!(old_choice_scaffold, 124, "{old_choice}");
        // Bare labels were 85 scaffold tokens. The gloss measured 111,
        // still under the legacy 124 that repeated "best described as".
        assert_eq!(new_choice_scaffold, 111, "{new_choice}");
        assert!(
            new_choice.contains(r#""bigfoot":"an unidentified large primate or sasquatch""#),
            "{new_choice}"
        );
        assert!(
            new_choice.contains("no creature is identified; tracks, smells, or sounds alone"),
            "{new_choice}"
        );
        assert!(!new_choice.contains("best described"), "{new_choice}");
    }

    /// The fixed six-row sighting set, with the live MEANS prompt.
    /// `noul_body` is the JSON `decision_http` posts for a noul question.
    /// Provider `input_tokens` also include Jev's own wrapper, so a live
    /// count near 300 is this body plus that wrapper.
    #[test]
    fn six_row_fixture_decisions_body_is_trimmed() {
        let model = "typesafe/jev-1.14";
        let criterion = "a large hairy biped was seen";
        let rows = [
            "A tall hairy figure crossed the logging road on two legs and left huge prints.",
            "We heard wood knocks, then a dark biped stepped over the creek.",
            "A deer bolted through the ferns at dusk.",
            "An owl called from the ridge and nothing else moved.",
            "Something ape-like stood at the tree line and walked upright into the timber.",
            "Campers reported a black bear turning over a cooler.",
        ];
        let expected = [(79, 240), (77, 225), (72, 202), (74, 214), (79, 239), (73, 214)];
        for (input, (tokens, bytes)) in rows.iter().zip(expected) {
            let item = demo_item(input, criterion);
            let body = noul_body(model, &item).unwrap();
            assert_eq!(body.matches(criterion).count(), 1, "{body}");
            assert!(!body.contains("criteria"), "{body}");
            assert!(body.contains(&format!("\"state\":\"{input}\"")), "{body}");
            assert_eq!(rough_tokens(&body), tokens, "{body}");
            assert_eq!(body.len(), bytes, "{body}");
            let posted: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert!(posted["state"].is_string(), "one text per decision call: {body}");
        }
    }

    /// The ~300 billed input tokens are not this JSON. OpenRouter's
    /// decisions reference prints a three-question example and, next to
    /// it, `usage.input_tokens` 476. This counts that example. It does
    /// not call the provider: the key in this environment is a dummy.
    /// Packing eight texts into one `state` shrinks the JSON we would
    /// post, but every question would see every text, and the schema
    /// returns one answer map for that single state. We still post one
    /// string state per row.
    #[test]
    fn jev_billed_gap_is_outside_the_posted_body() {
        let published = serde_json::json!({
            "model": "typesafe/jev-1.13",
            "state": {
                "customer_tier": "enterprise",
                "ticket": "My checkout page shows a blank screen after I click Pay. I have tried two browsers."
            },
            "questions": {
                "is_bug": {
                    "type": "noul",
                    "instructions": "Is the customer reporting a software defect?",
                    "criteria": {
                        "true": "The customer describes broken or unexpected product behavior.",
                        "false": "The customer is asking a question or requesting a feature."
                    }
                },
                "team": {
                    "type": "choice",
                    "instructions": "Which team should own this ticket?",
                    "criteria": {
                        "payments": "Checkout, billing, or payment processing issues.",
                        "frontend": "Rendering, layout, or browser compatibility issues.",
                        "account": "Login, permissions, or profile issues.",
                        "none": "None of the teams above."
                    }
                },
                "urgency": {
                    "type": "score",
                    "instructions": "How urgent is this ticket?",
                    "criteria": [
                        "Can wait for the next release",
                        "Should be fixed this week",
                        "Blocking revenue right now"
                    ]
                }
            }
        });
        let published_body = serde_json::to_string(&published).unwrap();
        assert_eq!(rough_tokens(&published_body), 284, "{published_body}");
        assert_eq!(published_body.len(), 892, "{published_body}");
        // Printed on the same OpenRouter page. Not remeasured here.
        let published_billed = 476;
        assert!(published_billed > rough_tokens(&published_body));

        let model = "typesafe/jev-1.14";
        let criterion = "a large hairy biped was seen";
        let row = "A tall hairy figure crossed the logging road on two legs and left huge prints.";
        let one = noul_body(model, &demo_item(row, criterion)).unwrap();
        assert_eq!(rough_tokens(&one), 79, "{one}");
        let mut separate = 0;
        let mut questions = serde_json::Map::new();
        let mut states = Vec::new();
        for i in 0..8 {
            let text = format!("{row} {i}");
            let body = noul_body(model, &demo_item(&text, criterion)).unwrap();
            separate += rough_tokens(&body);
            states.push(serde_json::Value::String(text));
            questions.insert(
                format!("q{i}"),
                serde_json::json!({
                    "type": "noul",
                    "instructions": format!("Does the text match the criterion?\n{criterion}")
                }),
            );
        }
        let packed = serde_json::json!({
            "model": model,
            "state": states,
            "questions": questions
        });
        let packed_body = serde_json::to_string(&packed).unwrap();
        let packed_tokens = rough_tokens(&packed_body);
        assert_eq!(separate, 640, "eight posted bodies");
        assert_eq!(packed_tokens, 467, "{packed_body}");
        assert!(packed_tokens < separate, "packing shrinks JSON, not the decision");
        assert!(packed["state"].is_array());
        assert!(serde_json::from_str::<serde_json::Value>(&one).unwrap()["state"].is_string());
    }

    #[test]
    fn scalar_extract_accepts_plain_text_and_json_scalars() {
        assert_eq!(parse_scalar_value("sasquatch").unwrap(), serde_json::json!("sasquatch"));
        assert_eq!(parse_scalar_value("  \"sasquatch\"  ").unwrap(), serde_json::json!("sasquatch"));
        assert_eq!(parse_scalar_value("12").unwrap(), serde_json::json!(12));
        assert_eq!(parse_scalar_value("true").unwrap(), serde_json::json!(true));
        assert!(parse_scalar_value("{\"creature\":\"sasquatch\"}").is_err());
        assert!(parse_json_value("sasquatch").is_err());
        assert_eq!(
            parse_json_value("{\"creature\":\"sasquatch\"}").unwrap()["creature"],
            "sasquatch"
        );
    }

    #[test]
    fn stats_calls_follow_requests_and_misses_are_uncapped() {
        let mut cache = Cache::new();
        note_batch(&mut cache, 32);
        note_batch(&mut cache, 8);
        assert_eq!(cache.requests, 2);
        assert_eq!(cache.rows, 40);
        assert_eq!(cache.max_batch, 32);
        let server = Server {
            cfg: Config::from_env(),
            agent: ureq::AgentBuilder::new().build(),
            cache: Mutex::new(cache),
        };
        let n = LATENCY_SAMPLE_CAP + 100;
        let usage = ProviderUsage {
            model: Some("openai/gpt-4o-mini".into()),
            ..ProviderUsage::default()
        };
        for _ in 0..n {
            let meter = miss_meter(
                Operation::Extract,
                "local",
                "openai/gpt-4o-mini",
                &usage,
                10,
                11,
                None,
                None,
            );
            record_call(&server, &meter);
        }
        let stats: serde_json::Value = serde_json::from_str(&stats_json(&server, true)).unwrap();
        assert_eq!(stats["calls"], 2);
        assert_eq!(stats["requests"], 2);
        assert_eq!(stats["rows"], 40);
        assert_eq!(stats["max_batch"], 32);
        let row = &stats["latency_by_op_model"][0];
        assert_eq!(row["calls"], n);
        assert_eq!(row["op"], "extract");
        let samples = lock(&server.cache)
            .latency_samples
            .values()
            .next()
            .map(|s| s.len())
            .unwrap_or(0);
        assert_eq!(samples, LATENCY_SAMPLE_CAP);
    }

    #[test]
    fn choice_score_is_the_probability_not_confidence() {
        let body = r#"{"answers":{"label":{"type":"choice","choice":"bigfoot","probabilities":{"bigfoot":0.75,"known animal":0.25},"confidence":0.5}}}"#;
        let hit = parse_choice(body).unwrap();
        assert_eq!(hit.probabilities["bigfoot"], 0.75);
        assert_eq!(hit.probabilities["known animal"], 0.25);
    }
}
