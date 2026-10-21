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
    CacheLookupRequest, CacheLookupResponse, CacheMode, CallMeter, DecisionStack, ItemRequest,
    Operation, CONTRACT_VERSION, HEADER_AUTHORIZATION, HEADER_CACHE, HEADER_CONTRACT,
};
use rvbbit_tree::{AskAns, AskReq, ChoiceAns, ChoiceReq, Decisions};
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

/// Native `CLASSIFY(x INTO ...)`. Labels replace the catalog label set.
/// The backend and decision model are the `means` function's, so a tree
/// or MEANS model already pointed at hutch is reused. The result is the
/// winning label, not the JSON pack.
pub fn classify_labels(input: &str, labels: &[String]) -> PgResult<String> {
    let reply = classify_many(labels, &[input.to_string()])?;
    one_item(reply)
}

/// One HTTP classify for every input. Duplicate inputs are the caller's
/// job to drop. Item errors stay in the vector; a transport failure is
/// the `Err`. Order matches `inputs`.
pub fn classify_many(labels: &[String], inputs: &[String]) -> PgResult<ManyReply> {
    if inputs.is_empty() {
        return Ok(ManyReply { values: Vec::new(), http_calls: 0 });
    }
    if labels.len() < 2 {
        return Err(pg("CLASSIFY needs at least two labels"));
    }
    let mut ep = decision_endpoint();
    ep.labels = labels.to_vec();
    ep.label_descriptions.clear();
    ep.fn_name = "classify".into();
    ep.return_type = "text".into();
    ep.user_prompt.clear();
    ep.system_prompt.clear();
    let payload: Vec<serde_json::Value> = inputs
        .iter()
        .map(|input| serde_json::json!({ "input": input }))
        .collect();
    let body = encode_items(&ep, Operation::Classify, &payload).map_err(pg)?;
    let raw = post_op(&ep, "classify", body)?;
    let values = decode_text_items(&raw.body).map_err(pg)?;
    if values.len() != inputs.len() {
        return Err(pg(format!(
            "hutch classify returned {} items for {} inputs",
            values.len(),
            inputs.len()
        )));
    }
    Ok(ManyReply { values, http_calls: raw.http_calls })
}

/// Native `EXTRACT(type FROM x)` for a non-datetime type. Chat, not Jev:
/// a decision-model slug is replaced with `mock`, which hutch maps to the
/// chat default. The URL still comes from the means backend.
pub fn extract_typed(kind: &str, input: &str) -> PgResult<String> {
    let reply = extract_many(kind, &[input.to_string()])?;
    one_item(reply)
}

/// One HTTP extract for every input. The model on the wire is `mock`.
/// Returned strings are still raw model text; [`coerce_extracted`] parses
/// `int` / `float` / `bool`.
pub fn extract_many(kind: &str, inputs: &[String]) -> PgResult<ManyReply> {
    if inputs.is_empty() {
        return Ok(ManyReply { values: Vec::new(), http_calls: 0 });
    }
    let ep = typed_extract_endpoint(kind);
    let payload: Vec<serde_json::Value> = inputs
        .iter()
        .map(|input| serde_json::json!({ "input": input }))
        .collect();
    let body = encode_items(&ep, Operation::Extract, &payload).map_err(pg)?;
    let raw = post_op(&ep, "extract", body)?;
    let values = decode_text_items(&raw.body).map_err(pg)?;
    if values.len() != inputs.len() {
        return Err(pg(format!(
            "hutch extract returned {} items for {} inputs",
            values.len(),
            inputs.len()
        )));
    }
    Ok(ManyReply { values, http_calls: raw.http_calls })
}

pub struct ManyReply {
    pub values: Vec<Result<String, String>>,
    pub http_calls: u32,
}

fn one_item(reply: ManyReply) -> PgResult<String> {
    match reply.values.into_iter().next() {
        Some(Ok(value)) => Ok(value),
        Some(Err(message)) => Err(pg(message)),
        None => Err(pg("hutch returned no items")),
    }
}

fn post_op(ep: &Endpoint, op: &str, body: String) -> PgResult<RawReply> {
    let auth = bearer(ep).map_err(pg)?;
    let ticket = enqueue(
        format!("{}/v1/{op}", ep.base_url.trim_end_matches('/')),
        auth,
        ep,
        body,
        cache_header(cache_mode_of(ep), None, None),
    )?;
    wait_raw(ticket)
}

/// Parse a model string into the SQL text for `kind`. Other kinds are
/// returned trimmed. Non-finite floats are an error so `ON ERROR` can run.
pub fn coerce_extracted(kind: &str, value: &str) -> Result<String, String> {
    let value = value.trim();
    match kind.to_ascii_lowercase().as_str() {
        "int" | "integer" | "bigint" | "int2" | "int4" | "int8" | "smallint" => {
            let n: i64 = value.parse().map_err(|_| format!("EXTRACT {kind} got {value}"))?;
            Ok(n.to_string())
        }
        "float" | "float4" | "float8" | "double" | "real" | "numeric" => {
            let n: f64 = value.parse().map_err(|_| format!("EXTRACT {kind} got {value}"))?;
            if !n.is_finite() {
                return Err(format!("EXTRACT {kind} got {value}"));
            }
            Ok(n.to_string())
        }
        "bool" | "boolean" => match value.to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "1" => Ok("true".into()),
            "false" | "f" | "no" | "0" => Ok("false".into()),
            _ => Err(format!("EXTRACT {kind} got {value}")),
        },
        _ => Ok(value.to_string()),
    }
}

/// Memo key for a classify batch. Includes the means backend URL and the
/// decision model, so two label sets or two backends do not share a result.
pub fn scalar_key_classify(labels: &[String]) -> String {
    let ep = decision_endpoint();
    format!(
        "c\u{1e}{}\u{1e}{}\u{1e}{}",
        ep.base_url,
        ep.model,
        labels.join("\u{1f}")
    )
}

/// Memo key for an extract batch. The chat model on the wire is always
/// `mock`; the URL is still the means backend.
pub fn scalar_key_extract(kind: &str) -> String {
    let ep = decision_endpoint();
    format!("e\u{1e}{}\u{1e}mock\u{1e}{kind}", ep.base_url)
}

fn typed_extract_endpoint(kind: &str) -> Endpoint {
    let mut ep = decision_endpoint();
    ep.model = "mock".into();
    ep.labels.clear();
    ep.label_descriptions.clear();
    ep.fn_name = "extract_typed".into();
    ep.return_type = "text".into();
    // Scalar mode caches a bare noun. JSON mode rejected every plain-text
    // answer and the row fell through ON ERROR.
    ep.extract_mode = Some("scalar".into());
    ep.system_prompt = format!(
        "Name the {kind} in at most three words. A short noun or literal only. \
         No sentence, no description, no markdown, no quotes."
    );
    ep.user_prompt = format!("What {kind} does this text name?\n{{{{input}}}}");
    ep.max_tokens = Some(16);
    ep
}

fn decision_endpoint() -> Endpoint {
    let oid = rvbbit_resolve::func_oid("means").unwrap_or(rvbbit_oids::MEANS_OID);
    rvbbit_resolve::endpoint_for(oid)
}

fn decode_text_items(text: &str) -> Result<Vec<Result<String, String>>, String> {
    let parsed = parsed_batch(text)?;
    let mut out = Vec::with_capacity(parsed.outputs.len());
    for (i, item) in parsed.outputs.iter().enumerate() {
        if let Some(err) = &item.error {
            out.push(Err(format!("hutch item {i}: {} {}", err.code, err.message)));
            continue;
        }
        match &item.value {
            Some(serde_json::Value::String(s)) => out.push(Ok(s.clone())),
            Some(other) => out.push(Ok(other.to_string())),
            None => out.push(Err(format!("hutch item {i} has no value"))),
        }
    }
    Ok(out)
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
            node: None,
            label_descriptions: {
                if ep.label_descriptions.iter().all(|d| d.is_empty()) {
                    None
                } else {
                    Some(ep.label_descriptions.clone())
                }
            },
            extract_mode: ep.extract_mode.clone(),
        });
    }
    let req = BatchRequest {
        contract: CONTRACT_VERSION.to_string(),
        operation,
        model: ep.model.clone(),
        temperature: ep.temperature,
        max_tokens: ep.max_tokens,
        items: req_items,
        decision: noul_stack(ep, operation),
    };
    serde_json::to_string(&req).map_err(|e| format!("hutch encode: {e}"))
}

/// Fold a non-empty label set into the schema string that the cache key
/// already hashes. Empty labels leave the schema unchanged.
/// A function or operator `THRESHOLD` becomes the noul cutoff. Absent
/// keeps the hutch server default. Classify and extract ignore it.
fn noul_stack(ep: &Endpoint, operation: Operation) -> Option<DecisionStack> {
    if !matches!(operation, Operation::Predict | Operation::Decide) {
        return None;
    }
    let threshold = ep.threshold?;
    Some(DecisionStack {
        models: Vec::new(),
        takes: None,
        combine: None,
        threshold: Some(threshold),
    })
}

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
            "node": meter.node,
            "score": meter.score,
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

pub struct DecideReply {
    pub values: Vec<String>,
    pub http_calls: u32,
    pub node_calls: u32,
    pub memo_hits: u32,
    /// Per row, `(node id, score)` in visit order.
    pub scores: Vec<Vec<(String, f64)>>,
}

/// Walk `nodes` for every input. One HTTP batch per node per level.
/// `USING` / `COMBINE` on a node become `decision.models` / `decision.combine`.
pub fn decide_texts(ep: &Endpoint, nodes: &str, inputs: &[String]) -> PgResult<DecideReply> {
    let root: serde_json::Value = serde_json::from_str(nodes).map_err(|e| pg(format!("decision tree json: {e}")))?;
    let mut backend = HutchTree { ep, http_calls: 0 };
    let report = rvbbit_tree::eval_batch(&root, inputs, &mut backend).map_err(pg)?;
    Ok(DecideReply {
        values: report.values,
        http_calls: backend.http_calls,
        node_calls: report.node_calls,
        memo_hits: report.memo_hits,
        scores: report.scores,
    })
}

struct HutchTree<'a> {
    ep: &'a Endpoint,
    http_calls: u32,
}

impl Decisions for HutchTree<'_> {
    fn ask(&mut self, req: &AskReq) -> Result<Vec<AskAns>, String> {
        let body = encode_tree_items(
            self.ep,
            Operation::Predict,
            &req.node,
            &req.question,
            &req.inputs,
            &[],
            &[],
            &req.models,
            &req.combine,
            Some(req.threshold),
        )?;
        let raw = post_tree(self.ep, "predict", body)?;
        self.http_calls = self.http_calls.saturating_add(raw.http_calls);
        decode_ask(&raw.body)
    }

    fn choose(&mut self, req: &ChoiceReq) -> Result<Vec<ChoiceAns>, String> {
        let body = encode_tree_items(
            self.ep,
            Operation::Classify,
            &req.node,
            &req.question,
            &req.inputs,
            &req.labels,
            &req.meanings,
            &req.models,
            &req.combine,
            None,
        )?;
        let raw = post_tree(self.ep, "classify", body)?;
        self.http_calls = self.http_calls.saturating_add(raw.http_calls);
        decode_choices(&raw.body)
    }
}

fn post_tree(ep: &Endpoint, op: &str, body: String) -> Result<RawReply, String> {
    let auth = bearer(ep)?;
    let ticket = enqueue(
        format!("{}/v1/{op}", ep.base_url.trim_end_matches('/')),
        auth,
        ep,
        body,
        cache_header(cache_mode_of(ep), None, None),
    )
    .map_err(|e| e.to_string())?;
    wait_raw(ticket).map_err(|e| e.to_string())
}

fn encode_tree_items(
    ep: &Endpoint,
    operation: Operation,
    node: &str,
    question: &str,
    inputs: &[String],
    labels: &[String],
    meanings: &[String],
    models: &[String],
    combine: &str,
    threshold: Option<f64>,
) -> Result<String, String> {
    let mut req_items = Vec::with_capacity(inputs.len());
    for (i, input) in inputs.iter().enumerate() {
        let inputs = if operation == Operation::Predict {
            serde_json::json!({ "input": input, "criterion": question })
        } else {
            serde_json::json!({ "input": input })
        };
        let mut schema_ep = ep.clone();
        schema_ep.user_prompt = question.to_string();
        schema_ep.labels = labels.to_vec();
        schema_ep.label_descriptions = meanings.to_vec();
        let schema = return_schema(&schema_ep);
        let cache_key = client_cache_key_hex(&CacheKeyParts {
            fn_name: &ep.fn_name,
            fn_version: ep.fn_version,
            system_prompt: "",
            user_prompt: question,
            return_schema: &schema,
            model: &ep.model,
            temperature: 0.0,
            max_tokens: None,
            take_index: 0,
            seed: node,
            inputs: &inputs,
        });
        req_items.push(ItemRequest {
            key: i.to_string(),
            cache_key,
            inputs: inputs.clone(),
            criterion: if operation == Operation::Predict { Some(question.to_string()) } else { None },
            system: None,
            prompt: Some(question.to_string()),
            labels: if labels.is_empty() { None } else { Some(labels.to_vec()) },
            label_descriptions: if meanings.iter().all(|m| m.is_empty()) {
                None
            } else {
                Some(meanings.to_vec())
            },
            node: Some(node_meter(node, question)),
            extract_mode: None,
        });
    }
    let stacked = !models.is_empty() || combine == "vote" || threshold.is_some_and(|t| (t - 0.5).abs() > 1e-9);
    let req = BatchRequest {
        contract: CONTRACT_VERSION.to_string(),
        operation,
        model: ep.model.clone(),
        temperature: 0.0,
        max_tokens: None,
        items: req_items,
        decision: stacked.then(|| DecisionStack {
            models: models.to_vec(),
            takes: None,
            combine: (combine == "vote").then(|| "vote".to_string()),
            threshold,
        }),
    };
    serde_json::to_string(&req).map_err(|e| format!("hutch encode: {e}"))
}

fn node_meter(id: &str, question: &str) -> String {
    let mut s = format!("{id}:{question}");
    if s.chars().count() > 120 {
        s = s.chars().take(120).collect();
    }
    s
}

fn decode_ask(text: &str) -> Result<Vec<AskAns>, String> {
    let parsed = parsed_batch(text)?;
    let mut out = Vec::with_capacity(parsed.outputs.len());
    for (i, item) in parsed.outputs.iter().enumerate() {
        if let Some(err) = &item.error {
            return Err(format!("hutch item {i}: {} {}", err.code, err.message));
        }
        let yes = item
            .value
            .as_ref()
            .and_then(|v| v.as_bool())
            .ok_or_else(|| format!("hutch item {i} is not a bool"))?;
        let score = item.score.unwrap_or(if yes { 1.0 } else { 0.0 });
        out.push(AskAns { yes, score });
    }
    Ok(out)
}

fn decode_choices(text: &str) -> Result<Vec<ChoiceAns>, String> {
    let parsed = parsed_batch(text)?;
    let mut out = Vec::with_capacity(parsed.outputs.len());
    for (i, item) in parsed.outputs.iter().enumerate() {
        if let Some(err) = &item.error {
            return Err(format!("hutch item {i}: {} {}", err.code, err.message));
        }
        let label = item
            .value
            .as_ref()
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("hutch item {i} has no label"))?;
        let score = item.score.unwrap_or(0.0);
        out.push(ChoiceAns {
            label: label.to_string(),
            score,
        });
    }
    Ok(out)
}

static DECIDE_MEMO: std::sync::LazyLock<Mutex<HashMap<(String, String), String>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Answer computed for this tree and input by the batch executor.
/// The row-at-a-time builtin returns it instead of calling hutch again.
pub fn remember_decide(tree: &str, input: &str, value: &str) {
    DECIDE_MEMO
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((tree.to_string(), input.to_string()), value.to_string());
}

pub fn recall_decide(tree: &str, input: &str) -> Option<String> {
    DECIDE_MEMO
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(tree.to_string(), input.to_string()))
        .cloned()
}

pub fn forget_decide(keys: &[(String, String)]) {
    let mut memo = DECIDE_MEMO.lock().unwrap_or_else(|e| e.into_inner());
    for key in keys {
        memo.remove(key);
    }
}

/// A value the project batcher already computed, or the item error the
/// builtin turns into `ON ERROR`.
#[derive(Clone, Debug)]
pub enum ScalarHit {
    Value(String),
    Failed(String),
}

static SCALAR_MEMO: std::sync::LazyLock<Mutex<HashMap<(String, String), ScalarHit>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn remember_scalar(key: &str, input: &str, hit: ScalarHit) {
    SCALAR_MEMO
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((key.to_string(), input.to_string()), hit);
}

pub fn recall_scalar(key: &str, input: &str) -> Option<ScalarHit> {
    SCALAR_MEMO
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(key.to_string(), input.to_string()))
        .cloned()
}

pub fn forget_scalar(keys: &[(String, String)]) {
    let mut memo = SCALAR_MEMO.lock().unwrap_or_else(|e| e.into_inner());
    for key in keys {
        memo.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::{coerce_extracted, encode_items, typed_extract_endpoint};
    use rvbbit_contract::Operation;

    #[test]
    fn coerce_extract_kinds() {
        assert_eq!(coerce_extracted("int", " 12 ").unwrap(), "12");
        assert!(coerce_extracted("int", "nope").is_err());
        assert_eq!(coerce_extracted("bool", "YES").unwrap(), "true");
        assert_eq!(coerce_extracted("creature", "  sasquatch ").unwrap(), "sasquatch");
        assert!(coerce_extracted("float", "inf").is_err());
    }

    #[test]
    fn typed_extract_asks_for_a_short_noun() {
        let ep = typed_extract_endpoint("creature");
        assert_eq!(ep.extract_mode.as_deref(), Some("scalar"));
        assert!(ep.system_prompt.contains("at most three words"));
        assert!(ep.system_prompt.contains("No sentence"));
        let body = encode_items(
            &ep,
            Operation::Extract,
            &[serde_json::json!({ "input": "a tall hairy figure" })],
        )
        .unwrap();
        assert!(body.contains("\"extract_mode\":\"scalar\""));
        assert!(body.contains("What creature does this text name?"));
        assert!(!body.contains("large, black-brown"));
    }
}

fn pg(message: impl Into<String>) -> Box<PgError> {
    Box::new(PgError::error(message.into()))
}
