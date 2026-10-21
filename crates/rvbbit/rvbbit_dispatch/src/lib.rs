//! Blocking HTTP dispatch from backend threads to a hutch service.
//!
//! A fixed OS thread pool is started at server boot. Backend threads hand
//! work over a bounded queue and wait on a reply channel, polling
//! `InterruptPending` so query cancel and `statement_timeout` abort the
//! wait. Workers use `ureq` (no tokio). Each backend URL has its own
//! concurrency cap, taken from the catalog row (4 for the default mock).
//!
//! The wire format is `rvbbit-hutch-v1` (`rvbbit_contract`). MEANS posts
//! `/v1/predict` once per batch and does not call `/v1/cache/lookup`.
//! The cache header mode comes from the catalog (`memoize` → `read-write`,
//! `off` → `off`). A 10k batch stays one HTTP call per 32 rows.
//! In-flight HTTP is not aborted; the wait returns at the next 20ms poll.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use rvbbit_contract::{
    cache_header, client_cache_key_hex, BatchRequest, BatchResponse, CacheKeyParts,
    CacheLookupRequest, CacheLookupResponse, CacheMode, CallMeter, ItemRequest, Operation,
    CONTRACT_VERSION, HEADER_AUTHORIZATION, HEADER_CACHE, HEADER_CONTRACT,
};
use rvbbit_oids::POOL_THREADS;
use rvbbit_resolve::Endpoint;
use types_error::{PgError, PgResult};

const WAIT_SLICE: Duration = Duration::from_millis(20);
const RETRY_CAP: Duration = Duration::from_millis(2_000);

struct Job {
    url: String,
    auth: Option<String>,
    timeout_ms: u64,
    retries: u32,
    limiter: Arc<Limiter>,
    body: String,
    cache_header: String,
    reply: mpsc::Sender<Result<RawReply, String>>,
}

struct RawReply {
    body: String,
    http_calls: u32,
}

struct Limiter {
    free: Mutex<usize>,
    cv: Condvar,
}

impl Limiter {
    fn new(n: usize) -> Self {
        Self {
            free: Mutex::new(n.max(1)),
            cv: Condvar::new(),
        }
    }

    fn acquire(&self) {
        let mut guard = self.free.lock().unwrap_or_else(|e| e.into_inner());
        while *guard == 0 {
            guard = self.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
        *guard -= 1;
    }

    fn release(&self) {
        let mut guard = self.free.lock().unwrap_or_else(|e| e.into_inner());
        *guard += 1;
        self.cv.notify_one();
    }
}

struct Pool {
    tx: SyncSender<Job>,
}

fn pool() -> &'static Pool {
    static POOL: std::sync::OnceLock<Pool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<Job>(64);
        let rx = Arc::new(Mutex::new(rx));
        let agent = ureq::AgentBuilder::new().build();
        for i in 0..POOL_THREADS {
            let rx = Arc::clone(&rx);
            let agent = agent.clone();
            thread::Builder::new()
                .name(format!("rvbbit-hutch-{i}"))
                .spawn(move || worker(rx, agent))
                .expect("spawn rvbbit dispatch worker");
        }
        Pool { tx }
    })
}

fn limiters() -> &'static Mutex<HashMap<String, Arc<Limiter>>> {
    static LIMITERS: std::sync::OnceLock<Mutex<HashMap<String, Arc<Limiter>>>> =
        std::sync::OnceLock::new();
    LIMITERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn limiter_for(url: &str, n: usize) -> Arc<Limiter> {
    let mut map = limiters().lock().unwrap_or_else(|e| e.into_inner());
    map.entry(format!("{url}#{n}"))
        .or_insert_with(|| Arc::new(Limiter::new(n)))
        .clone()
}

fn worker(rx: Arc<Mutex<Receiver<Job>>>, agent: ureq::Agent) {
    loop {
        let job = {
            let guard = rx.lock().unwrap_or_else(|e| e.into_inner());
            guard.recv()
        };
        let Ok(job) = job else {
            break;
        };
        job.limiter.acquire();
        let result = post_with_retry(&agent, &job);
        job.limiter.release();
        let _ = job.reply.send(result);
    }
}

/// Start the pool. Safe to call more than once; later calls are no-ops.
pub fn start() {
    let _ = pool();
}

pub struct Ticket {
    rx: Receiver<Result<RawReply, String>>,
}

/// One hutch round-trip's worth of answers. `http_calls` counts attempts,
/// including 429 retries, so EXPLAIN lines up with the mock's `/stats`.
pub struct BatchReply {
    pub matched: Vec<bool>,
    pub http_calls: u32,
}

/// Queue one predict batch. The caller waits with [`wait`], which honors
/// query cancel. `func_oid` selects the catalog backend (URL, auth, model).
pub fn submit(func_oid: u32, items: &[(String, String)]) -> PgResult<Ticket> {
    let ep = rvbbit_resolve::endpoint_for(func_oid);
    if items.is_empty() {
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(Ok(RawReply {
            body: empty_predict(&ep),
            http_calls: 0,
        }));
        return Ok(Ticket { rx });
    }
    let body = encode_predict(&ep, items).map_err(pg)?;
    let auth = bearer(&ep).map_err(pg)?;
    enqueue(
        format!("{}/v1/predict", ep.base_url.trim_end_matches('/')),
        auth,
        &ep,
        body,
        cache_header(cache_mode_of(&ep), None, None),
    )
}

/// One extract call. `input` is the first text argument (`{{input}}`).
/// The catalog prompt and system text ride on the item. Not batched in SQL;
/// hutch still accepts a multi-item body, which the tests use directly.
pub fn extract_text(func_oid: u32, input: &str) -> PgResult<String> {
    let ep = rvbbit_resolve::endpoint_for(func_oid);
    let inputs = serde_json::json!({ "input": input });
    let body = encode_items(&ep, Operation::Extract, &[inputs]).map_err(pg)?;
    let auth = bearer(&ep).map_err(pg)?;
    let ticket = enqueue(
        format!("{}/v1/extract", ep.base_url.trim_end_matches('/')),
        auth,
        &ep,
        body,
        cache_header(cache_mode_of(&ep), None, None),
    )?;
    let raw = wait_raw(ticket)?;
    decode_json_text(&raw.body).map_err(pg)
}

/// Fixed-label classify. The SQL function's declared labels ride on the
/// endpoint. Hutch returns the winning label, its probability, and the
/// per-label map. This function packs those into one JSON text value.
pub fn classify_text(func_oid: u32, input: &str) -> PgResult<String> {
    let ep = rvbbit_resolve::endpoint_for(func_oid);
    let inputs = serde_json::json!({ "input": input });
    let body = encode_items(&ep, Operation::Classify, &[inputs]).map_err(pg)?;
    let auth = bearer(&ep).map_err(pg)?;
    let ticket = enqueue(
        format!("{}/v1/classify", ep.base_url.trim_end_matches('/')),
        auth,
        &ep,
        body,
        cache_header(cache_mode_of(&ep), None, None),
    )?;
    let raw = wait_raw(ticket)?;
    decode_classify(&raw.body).map_err(pg)
}

/// One predict call that returns the decision score (yes-probability).
/// Same wire body as MEANS. The boolean operator does not read `score`.
pub fn means_score(func_oid: u32, input: &str, criterion: &str) -> PgResult<f64> {
    let ep = rvbbit_resolve::endpoint_for(func_oid);
    let inputs = serde_json::json!({ "input": input, "criterion": criterion });
    let body = encode_items(&ep, Operation::Predict, &[inputs]).map_err(pg)?;
    let auth = bearer(&ep).map_err(pg)?;
    let ticket = enqueue(
        format!("{}/v1/predict", ep.base_url.trim_end_matches('/')),
        auth,
        &ep,
        body,
        cache_header(cache_mode_of(&ep), None, None),
    )?;
    let raw = wait_raw(ticket)?;
    decode_score(&raw.body).map_err(pg)
}

fn cache_mode_of(ep: &Endpoint) -> CacheMode {
    match ep.cache_mode.as_str() {
        "off" => CacheMode::Off,
        "refresh" => CacheMode::Refresh,
        _ => CacheMode::ReadWrite,
    }
}

/// `POST /v1/cache/lookup`. Not used by MEANS. The hot path sends the
/// catalog cache mode on the operation POST and does not add a lookup hop.
/// Exposed so the pool speaks the contract.
pub fn cache_lookup(base_url: &str, keys: &[String]) -> PgResult<CacheLookupResponse> {
    let ep = rvbbit_resolve::default_endpoint();
    let body = serde_json::to_string(&CacheLookupRequest { keys: keys.to_vec() })
        .map_err(|e| pg(format!("cache lookup encode: {e}")))?;
    let ticket = enqueue(
        format!("{}/v1/cache/lookup", base_url.trim_end_matches('/')),
        None,
        &ep,
        body,
        cache_header(CacheMode::ReadWrite, None, None),
    )?;
    let raw = wait_raw(ticket)?;
    serde_json::from_str(&raw.body).map_err(|e| pg(format!("cache lookup decode: {e}")))
}

/// One pair through the pool. Used by the per-row builtin fallback.
pub fn means_oid(func_oid: u32, input: &str, criterion: &str) -> PgResult<bool> {
    let ticket = submit(func_oid, &[(input.to_string(), criterion.to_string())])?;
    let got = wait(ticket)?;
    Ok(got.matched.first().copied().unwrap_or(false))
}

/// Block until the batch returns, or the backend is canceled.
pub fn wait(ticket: Ticket) -> PgResult<BatchReply> {
    let raw = wait_raw(ticket)?;
    decode_predict(&raw.body, raw.http_calls).map_err(pg)
}

fn enqueue(
    url: String,
    auth: Option<String>,
    ep: &Endpoint,
    body: String,
    cache_header: String,
) -> PgResult<Ticket> {
    let (reply, rx) = mpsc::channel();
    let job = Job {
        limiter: limiter_for(&url, ep.max_concurrent),
        url,
        auth,
        timeout_ms: ep.timeout_ms,
        retries: ep.retries.max(1),
        body,
        cache_header,
        reply,
    };
    let tx = &pool().tx;
    let mut job = Some(job);
    loop {
        check_cancel()?;
        match tx.try_send(job.take().expect("job")) {
            Ok(()) => break,
            Err(TrySendError::Full(returned)) => {
                job = Some(returned);
                thread::sleep(WAIT_SLICE);
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(pg("rvbbit dispatch pool is gone"));
            }
        }
    }
    Ok(Ticket { rx })
}

fn wait_raw(ticket: Ticket) -> PgResult<RawReply> {
    loop {
        check_cancel()?;
        match ticket.rx.recv_timeout(WAIT_SLICE) {
            Ok(Ok(v)) => return Ok(v),
            Ok(Err(e)) => return Err(pg(e)),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(pg("rvbbit dispatch worker dropped the reply"));
            }
        }
    }
}

fn encode_predict(ep: &Endpoint, items: &[(String, String)]) -> Result<String, String> {
    let inputs: Vec<serde_json::Value> = items
        .iter()
        .map(|(input, criterion)| serde_json::json!({ "criterion": criterion, "input": input }))
        .collect();
    encode_items(ep, Operation::Predict, &inputs)
}

fn encode_items(ep: &Endpoint, operation: Operation, inputs: &[serde_json::Value]) -> Result<String, String> {
    let system = if ep.system_prompt.is_empty() { None } else { Some(ep.system_prompt.clone()) };
    let prompt = if ep.user_prompt.is_empty() { None } else { Some(ep.user_prompt.clone()) };
    let mut req_items = Vec::with_capacity(inputs.len());
    for (i, inputs) in inputs.iter().enumerate() {
        let schema = return_schema(ep);
        let cache_key = client_cache_key_hex(&CacheKeyParts {
            fn_name: &ep.fn_name,
            fn_version: ep.fn_version,
            system_prompt: &ep.system_prompt,
            user_prompt: &ep.user_prompt,
            return_schema: &schema,
            model: &ep.model,
            temperature: ep.temperature,
            max_tokens: ep.max_tokens,
            take_index: 0,
            seed: "",
            inputs,
        });
        let criterion = inputs.get("criterion").and_then(|v| v.as_str()).map(|s| s.to_string());
        req_items.push(ItemRequest {
            key: i.to_string(),
            cache_key,
            inputs: inputs.clone(),
            criterion,
            system: system.clone(),
            prompt: prompt.clone(),
            labels: if ep.labels.is_empty() { None } else { Some(ep.labels.clone()) },
            label_descriptions: {
                if ep.label_descriptions.iter().all(|d| d.is_empty()) {
                    None
                } else {
                    Some(ep.label_descriptions.clone())
                }
            },
        });
    }
    let req = BatchRequest {
        contract: CONTRACT_VERSION.to_string(),
        operation,
        model: ep.model.clone(),
        temperature: ep.temperature,
        max_tokens: ep.max_tokens,
        items: req_items,
        decision: None,
    };
    serde_json::to_string(&req).map_err(|e| format!("hutch encode: {e}"))
}

/// Fold a non-empty label set into the schema string that the cache key
/// already hashes. Empty labels leave the schema unchanged.
fn return_schema(ep: &Endpoint) -> String {
    if ep.labels.is_empty() {
        return ep.return_type.clone();
    }
    if ep.label_descriptions.iter().all(|d| d.is_empty()) {
        return format!("{} labels={}", ep.return_type, ep.labels.join("\u{1f}"));
    }
    let parts = ep
        .labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let desc = ep.label_descriptions.get(i).map(|s| s.as_str()).unwrap_or("");
            if desc.is_empty() {
                label.clone()
            } else {
                format!("{label}\u{1e}{desc}")
            }
        })
        .collect::<Vec<_>>()
        .join("\u{1f}");
    format!("{} labels={}", ep.return_type, parts)
}

fn parsed_batch(text: &str) -> Result<BatchResponse, String> {
    serde_json::from_str(text).map_err(|e| format!("hutch decode: {e}: {text}"))
}

/// `GET {base}/v1/calls?limit=1024`. The `rvbbit.calls` view reads this.
/// It does not use the dispatch pool, so a stats read cannot wait behind
/// a predict batch.
pub fn recent_calls_json(base_url: &str) -> Result<String, String> {
    let url = format!(
        "{}/v1/calls?limit=1024",
        base_url.trim_end_matches('/')
    );
    let resp = ureq::get(&url)
        .timeout(Duration::from_secs(5))
        .call()
        .map_err(|e| format!("hutch calls: {e}"))?;
    let text = resp.into_string().map_err(|e| format!("hutch calls body: {e}"))?;
    let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("hutch calls json: {e}"))?;
    let calls = parsed
        .get("calls")
        .and_then(|v| v.as_array())
        .ok_or("hutch calls response has no calls array")?;
    let mut rows = Vec::with_capacity(calls.len());
    for call in calls {
        let meter: CallMeter = serde_json::from_value(call.clone()).map_err(|e| format!("hutch calls row: {e}"))?;
        rows.push(serde_json::json!({
            "op": meter.op,
            "model": meter.model,
            "tokens_in": meter.tokens_in,
            "tokens_out": meter.tokens_out,
            "cost": meter.cost_usd,
            "cost_saved": meter.cost_saved_usd,
            "latency_ms": meter.latency_ms,
            "provider_latency_ms": meter.provider_latency_ms,
            "cache": meter.cache,
            "items": meter.items,
            "tenant": meter.tenant,
            "generation_id": meter.generation_id,
            "ts": meter.timestamp,
        }));
    }
    serde_json::to_string(&rows).map_err(|e| format!("hutch calls encode: {e}"))
}

fn decode_classify(text: &str) -> Result<String, String> {
    let parsed = parsed_batch(text)?;
    let item = parsed.outputs.first().ok_or_else(|| "hutch classify returned no items".to_string())?;
    if let Some(err) = &item.error {
        return Err(format!("hutch item 0: {} {}", err.code, err.message));
    }
    let label = item
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .ok_or_else(|| "hutch classify item has no label".to_string())?;
    let packed = serde_json::json!({
        "label": label,
        "score": item.score,
        "scores": item.scores,
    });
    Ok(packed.to_string())
}

fn decode_score(text: &str) -> Result<f64, String> {
    let parsed = parsed_batch(text)?;
    let item = parsed.outputs.first().ok_or_else(|| "hutch predict returned no items".to_string())?;
    if let Some(err) = &item.error {
        return Err(format!("hutch item 0: {} {}", err.code, err.message));
    }
    item.score.ok_or_else(|| "hutch predict item has no score".into())
}

fn decode_json_text(text: &str) -> Result<String, String> {
    let parsed = parsed_batch(text)?;
    let item = parsed.outputs.first().ok_or_else(|| "hutch extract returned no items".to_string())?;
    if let Some(err) = &item.error {
        return Err(format!("hutch item 0: {} {}", err.code, err.message));
    }
    match &item.value {
        Some(value) => Ok(value.to_string()),
        None => Err("hutch extract item has no value".into()),
    }
}

fn empty_predict(ep: &Endpoint) -> String {
    serde_json::json!({
        "contract": CONTRACT_VERSION,
        "outputs": [],
        "model": ep.model,
    })
    .to_string()
}

fn decode_predict(text: &str, http_calls: u32) -> Result<BatchReply, String> {
    let parsed = parsed_batch(text)?;
    let mut matched = Vec::with_capacity(parsed.outputs.len());
    for (i, item) in parsed.outputs.iter().enumerate() {
        if let Some(err) = &item.error {
            return Err(format!("hutch item {i}: {} {}", err.code, err.message));
        }
        let Some(value) = &item.value else {
            return Err(format!("hutch item {i} has no value"));
        };
        let flag = value.as_bool().ok_or_else(|| format!("hutch item {i} is not a bool"))?;
        matched.push(flag);
    }
    Ok(BatchReply { matched, http_calls })
}

fn bearer(ep: &Endpoint) -> Result<Option<String>, String> {
    let Some(name) = &ep.auth_env else {
        return Ok(None);
    };
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Ok(Some(v)),
        _ => Err(format!("backend auth env {name} is not set")),
    }
}

fn check_cancel() -> PgResult<()> {
    if init_small::globals::InterruptPending() {
        postgres_seams::check_for_interrupts::call()?;
    }
    Ok(())
}

fn post_with_retry(agent: &ureq::Agent, job: &Job) -> Result<RawReply, String> {
    let mut last = String::from("hutch: no attempt");
    let attempts = job.retries.max(1);
    for attempt in 1..=attempts {
        match post_once(agent, job) {
            Ok(body) => {
                return Ok(RawReply {
                    body,
                    http_calls: attempt,
                })
            }
            Err(PostErr::Retry { after, detail }) if attempt < attempts => {
                last = detail;
                thread::sleep(after.min(RETRY_CAP));
            }
            Err(PostErr::Retry { detail, .. }) => return Err(detail),
            Err(PostErr::Fatal(e)) => return Err(e),
        }
    }
    Err(last)
}

enum PostErr {
    Retry { after: Duration, detail: String },
    Fatal(String),
}

fn post_once(agent: &ureq::Agent, job: &Job) -> Result<String, PostErr> {
    let mut req = agent
        .post(&job.url)
        .timeout(Duration::from_millis(job.timeout_ms.max(1)))
        .set("Content-Type", "application/json")
        .set(HEADER_CONTRACT, CONTRACT_VERSION)
        .set(HEADER_CACHE, &job.cache_header);
    if let Some(token) = &job.auth {
        req = req.set(HEADER_AUTHORIZATION, &format!("Bearer {token}"));
    }
    match req.send_string(&job.body) {
        Ok(resp) => resp.into_string().map_err(|e| PostErr::Fatal(e.to_string())),
        Err(ureq::Error::Status(code, resp)) => {
            let after = resp
                .header("retry-after")
                .and_then(|s| s.parse::<u64>().ok())
                .map(|secs| Duration::from_secs(secs))
                .unwrap_or(Duration::from_millis(50));
            let detail = resp.into_string().unwrap_or_default();
            if code == 429 {
                Err(PostErr::Retry {
                    after,
                    detail: format!("429 {detail}"),
                })
            } else {
                Err(PostErr::Fatal(format!("hutch HTTP {code}: {detail}")))
            }
        }
        Err(e) => Err(PostErr::Fatal(format!("hutch transport: {e}"))),
    }
}

fn pg(message: impl Into<String>) -> Box<PgError> {
    Box::new(PgError::error(message.into()))
}
