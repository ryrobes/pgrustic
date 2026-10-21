//! Early hutch that speaks `rvbbit-hutch-v1`.
//!
//! Predict and decide call OpenRouter's Decisions API (Jev by default) and
//! return a boolean plus the yes-probability. Fixed-label classify uses
//! one `choice` question on that same API and returns the winning label,
//! its probability, and the per-label map. Extract, and classify with no
//! label set, still call an OpenAI-compatible chat endpoint. Embed and
//! rerank are rejected per item. `TAKES` / `PICK` are not parsed in SQL;
//! a request may still send `decision` to stack models or repeats.
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
    DecisionStack, ItemError, ItemRequest, ItemResponse, Operation, CONTRACT_VERSION,
    PATH_CACHE_LOOKUP, PATH_CACHE_PURGE, PATH_CACHE_PUT, PATH_CACHE_STATS,
};

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
    pub decision_model: String,
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
            decision_takes,
            decision_combine: if combine == "vote" { "vote".into() } else { "mean".into() },
            decision_threshold,
            timeout: Duration::from_millis(timeout_ms),
            concurrency: concurrency.max(1),
            tenant: std::env::var("HUTCH_TENANT").unwrap_or_else(|_| "local".into()),
            default_ttl: Duration::from_secs(ttl_secs),
            expect_auth: std::env::var("HUTCH_EXPECT_AUTH").ok().filter(|s| !s.is_empty()),
        }
    }
}

#[derive(Clone)]
struct Scored {
    value: serde_json::Value,
    score: Option<f64>,
    scores: Option<serde_json::Value>,
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
            score: None,
            scores: None,
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
                let result = run_item(server, tenant, operation, mode, req, item);
                release(&gate);
                let mut slot = outputs.lock().unwrap_or_else(|e| e.into_inner());
                slot[i] = match result {
                    Ok(scored) => ItemResponse {
                        key: item.key.clone(),
                        value: Some(scored.value),
                        score: scored.score,
                        scores: scored.scores,
                        error: None,
                    },
                    Err(err) => ItemResponse {
                        key: item.key.clone(),
                        value: None,
                        score: None,
                        scores: None,
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
    mode: ParsedCache,
    req: &BatchRequest,
    item: &rvbbit_contract::ItemRequest,
) -> Result<Scored, ItemError> {
    match operation {
        Operation::Embed | Operation::Rerank => {
            return Err(item_err(
                "unsupported",
                match operation {
                    Operation::Embed => "embed is not implemented in hutch-lite",
                    _ => "rerank is not implemented in hutch-lite",
                },
            ));
        }
        Operation::Predict | Operation::Decide | Operation::Extract | Operation::Classify => {}
    }
    let labels = item.labels.clone().unwrap_or_default();
    let fixed_classify = operation == Operation::Classify && labels.len() >= 2;
    let plan = (matches!(operation, Operation::Predict | Operation::Decide) || fixed_classify)
        .then(|| decision_plan(&server.cfg, req));
    let storage = storage_for(&item.cache_key, tenant, plan.as_ref(), &labels);
    let ttl = mode.ttl.unwrap_or(server.cfg.default_ttl);
    let compute = || provider_call(server, tenant, operation, req, item, mode.mode);
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
    compute: impl FnOnce() -> Result<Scored, ItemError>,
) -> Result<Scored, ItemError> {
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
    if matches!(operation, Operation::Predict | Operation::Decide) {
        return decision_scored(server, tenant, req, item, memo);
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
            return choice_scored(server, tenant, req, item, memo);
        }
    }
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
    let value = match operation {
        Operation::Extract => parse_json_value(&text).map_err(|message| item_err("parse", &message))?,
        Operation::Classify => serde_json::Value::String(text.trim().to_string()),
        _ => return Err(item_err("unsupported", "operation is not implemented in hutch-lite")),
    };
    Ok(Scored { value, score: None, scores: None })
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

#[derive(Clone)]
struct DecisionPlan {
    models: Vec<String>,
    takes: u32,
    combine: Combine,
    threshold: f64,
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
    });
    let mut models: Vec<String> = stack
        .models
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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
    }
}

fn choose_decision_model(cfg: &Config, requested: &str) -> String {
    let requested = requested.trim();
    if requested.is_empty() || requested == "mock" {
        cfg.decision_model.clone()
    } else {
        requested.to_string()
    }
}

fn decision_scored(
    server: &Server,
    tenant: &str,
    req: &BatchRequest,
    item: &ItemRequest,
    memo: bool,
) -> Result<Scored, ItemError> {
    let plan = decision_plan(&server.cfg, req);
    let mut scores = Vec::with_capacity(plan.models.len() * plan.takes as usize);
    for model in &plan.models {
        for take in 0..plan.takes {
            let text = decision_http(server, tenant, DecisionKind::Noul, model, take, item, memo)?;
            scores.push(parse_noul(&text).map_err(map_decision_err)?);
        }
    }
    let (matched, mean) = combine_noul(plan.combine, plan.threshold, &scores);
    Ok(Scored {
        value: serde_json::Value::Bool(matched),
        score: Some(mean),
        scores: None,
    })
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
    let mut sums: HashMap<String, f64> = HashMap::new();
    let mut calls = 0f64;
    for model in &plan.models {
        for take in 0..plan.takes {
            let text = decision_http(server, tenant, DecisionKind::Choice, model, take, item, memo)?;
            let hit = parse_choice(&text).map_err(map_decision_err)?;
            for label in &labels {
                let p = hit.probabilities.get(label).copied().unwrap_or(0.0);
                *sums.entry(label.clone()).or_insert(0.0) += p;
            }
            calls += 1.0;
        }
    }
    let mut scores = serde_json::Map::new();
    let mut best_label = labels[0].clone();
    let mut best = f64::NEG_INFINITY;
    for label in &labels {
        let mean = sums.get(label).copied().unwrap_or(0.0) / calls;
        scores.insert(label.clone(), serde_json::json!(mean));
        if mean > best {
            best = mean;
            best_label = label.clone();
        }
    }
    Ok(Scored {
        value: serde_json::Value::String(best_label),
        score: Some(best),
        scores: Some(serde_json::Value::Object(scores)),
    })
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
    kind: DecisionKind,
    model: &str,
    take: u32,
    item: &ItemRequest,
    memo: bool,
) -> Result<String, ItemError> {
    let body = match kind {
        DecisionKind::Noul => noul_body(model, item)?,
        DecisionKind::Choice => choice_body(model, item)?,
    };
    if !memo {
        return fetch_decision(server, kind, &body);
    }
    // Take index is part of the memo key so stacked repeats each call the
    // provider. It is not sent: the Decisions API has no take field, and
    // Jev is not documented as sampling. Temperature and max_tokens are
    // also not sent and are not part of this key.
    let key = format!("{tenant}\n{take}\n{body}");
    let flight = {
        let mut cache = lock(&server.cache);
        if let Some(text) = cache.decision_memo.get(&key).cloned() {
            return Ok(text);
        }
        if let Some(existing) = cache.decision_inflight.get(&key).cloned() {
            cache.single_flight_waits += 1;
            drop(cache);
            return wait_decision(&existing);
        }
        let flight = Arc::new(DecisionFlight {
            lock: Mutex::new(None),
            cv: Condvar::new(),
        });
        cache.decision_inflight.insert(key.clone(), Arc::clone(&flight));
        flight
    };
    let result = fetch_decision(server, kind, &body).map(|text| {
        let mut cache = lock(&server.cache);
        cache.decision_memo.insert(key.clone(), text.clone());
        text
    });
    {
        let mut cache = lock(&server.cache);
        cache.decision_inflight.remove(&key);
    }
    {
        let mut slot = flight.lock.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(match &result {
            Ok(text) => Ok(text.clone()),
            Err(e) => Err(e.clone()),
        });
        flight.cv.notify_all();
    }
    result
}

fn fetch_decision(server: &Server, kind: DecisionKind, body: &str) -> Result<String, ItemError> {
    {
        let mut cache = lock(&server.cache);
        cache.provider_calls += 1;
    }
    let text = post_json(&server.cfg, &server.agent, &server.cfg.decisions_url, body)
        .map_err(|message| item_err("provider", &message))?;
    let parsed = match kind {
        DecisionKind::Noul => parse_noul(&text).map(|_| ()),
        DecisionKind::Choice => parse_choice(&text).map(|_| ()),
    };
    parsed.map_err(map_decision_err)?;
    Ok(text)
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
    let true_criteria = if criterion.is_empty() {
        "The text matches the criterion.".to_string()
    } else {
        criterion.clone()
    };
    let body = serde_json::json!({
        "model": model,
        "state": { "text": input, "criterion": criterion },
        "questions": {
            "matches": {
                "type": "noul",
                "instructions": instructions,
                "criteria": {
                    "true": true_criteria,
                    "false": "The text does not match the criterion."
                }
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
    let body = serde_json::json!({
        "model": model,
        "state": { "text": input },
        "questions": {
            "label": {
                "type": "choice",
                "instructions": choice_instructions(item, &labels),
                "criteria": labels,
            }
        }
    });
    serde_json::to_string(&body).map_err(|e| item_err("provider", &e.to_string()))
}

fn choice_instructions(item: &ItemRequest, labels: &[String]) -> String {
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
        (None, None) => format!("Which one label fits the text? Labels: {}.", labels.join(", ")),
    }
}

fn decision_instructions(item: &ItemRequest, criterion: &str) -> String {
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
        (None, None) => {
            if criterion.is_empty() {
                "Does the text match the criterion?".into()
            } else {
                format!("Does the text match this criterion? {criterion}")
            }
        }
    }
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
    let text = post_json(cfg, agent, &url, &payload)?;
    completion_text(&text).map_err(|e| redact(&e, key))
}

fn post_json(cfg: &Config, agent: &ureq::Agent, url: &str, payload: &str) -> Result<String, String> {
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
        if let Some(hit) = fresh(&cache, &storage) {
            cache.hits += 1;
            hits.push(CacheHit { key: key.clone(), output: hit.value });
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
        store(
            server,
            &storage,
            &Scored {
                value: entry.output.clone(),
                score: None,
                scores: None,
            },
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
    let expires = if ttl.is_zero() { None } else { Some(Instant::now() + ttl) };
    let mut cache = lock(&server.cache);
    cache.entries.insert(
        storage.to_string(),
        CacheEntry { scored: scored.clone(), expires },
    );
}

fn fresh(cache: &Cache, storage: &str) -> Option<Scored> {
    let entry = cache.entries.get(storage)?;
    if let Some(exp) = entry.expires {
        if Instant::now() >= exp {
            return None;
        }
    }
    Some(entry.scored.clone())
}

fn storage_for(
    client_hex: &str,
    tenant: &str,
    plan: Option<&DecisionPlan>,
    labels: &[String],
) -> String {
    let base = storage_of(client_hex, tenant);
    if plan.is_none() && labels.is_empty() {
        return base;
    }
    let mut tag = if let Some(plan) = plan {
        let threshold = serde_json::Number::from_f64(plan.threshold)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "null".into());
        format!(
            "{base}\n{}\n{}\n{}\n{threshold}",
            plan.models.join("\n"),
            plan.takes,
            plan.combine.as_str(),
        )
    } else {
        base
    };
    if !labels.is_empty() {
        tag.push('\n');
        tag.push_str(&labels.join("\n"));
    }
    hex32(&blake3::hash(tag.as_bytes()).into())
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
    fn choice_score_is_the_probability_not_confidence() {
        let body = r#"{"answers":{"label":{"type":"choice","choice":"bigfoot","probabilities":{"bigfoot":0.75,"known animal":0.25},"confidence":0.5}}}"#;
        let hit = parse_choice(body).unwrap();
        assert_eq!(hit.probabilities["bigfoot"], 0.75);
        assert_eq!(hit.probabilities["known animal"], 0.25);
    }
}
