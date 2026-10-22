//! Blocking HTTP dispatch from backend threads to a hutch service.
//!
//! A fixed OS thread pool is started at server boot. Backend threads hand
//! work over a bounded queue and wait on a reply channel, polling
//! `InterruptPending` so query cancel and `statement_timeout` abort the
//! wait. Workers use `ureq` (no tokio). Each backend URL has its own
//! concurrency cap, taken from the catalog row (4 for the default mock).
//!
//! The wire format is `rvbbit-hutch-v1` (`rvbbit_contract`). A read-write
//! batch looks up `/v1/cache/lookup` and the process memo first, then
//! posts only the misses. The cache header mode comes from the catalog
//! (`memoize` → `read-write`, `off` → `off`) unless `semantic.cache`
//! forces `off` or `refresh`. Lookup is not an operation post, so a
//! 10k miss run still counts one predict per 32 rows.
//! In-flight HTTP is not aborted; the wait returns at the next 20ms poll.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use rvbbit_contract::{
    cache_header, client_cache_key_hex, BatchRequest, BatchResponse, CacheKeyParts,
    CacheLookupRequest, CacheLookupResponse, CacheMode, CallMeter, DecisionStack, ItemRequest,
    Operation, ABOUT_THRESHOLD, CONTRACT_VERSION, EMBED_MODEL, HEADER_AUTHORIZATION, HEADER_CACHE,
    HEADER_CONTRACT,
};
use rvbbit_tree::{AskAns, AskReq, ChoiceAns, ChoiceReq, Decisions};
use rvbbit_oids::POOL_THREADS;
use rvbbit_resolve::Endpoint;
use types_error::{PgError, PgResult};

pub mod clauses;
mod memo;

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
    audit: bool,
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

/// Register `semantic.*` and point the memo at `GetConfigOption`.
/// Called from backend startup, not from the pool unit test.
pub fn define_gucs() {
    use types_guc::PGC_USERSET;
    let _ = guc::DefineCustomStringVariable(
        "semantic.cache",
        Some("Semantic cache mode: on, off, or refresh."),
        None,
        Some("on"),
        PGC_USERSET,
        0,
    );
    let _ = guc::DefineCustomStringVariable(
        "semantic.memo_size",
        Some("Process semantic memo capacity, in entries."),
        None,
        Some("1024"),
        PGC_USERSET,
        0,
    );
    let _ = guc::DefineCustomStringVariable(
        "semantic.memo_ttl",
        Some("Process semantic memo lifetime, in seconds. 0 disables expiry."),
        None,
        Some("3600"),
        PGC_USERSET,
        0,
    );
    let _ = guc::DefineCustomStringVariable(
        "semantic.audit_payloads",
        Some("When on, hutch call rows include a short input snippet."),
        None,
        Some("off"),
        PGC_USERSET,
        0,
    );
    // Call clause wins, then the function DEFAULT CLAUSES, then these.
    // Empty string means "unset" so a higher tier can fill the field.
    for (name, desc) in [
        ("semantic.model", "Default decision model slug."),
        ("semantic.takes", "Default takes per model."),
        ("semantic.temperature", "Default sampling temperature."),
        ("semantic.max_tokens", "Default max tokens."),
        ("semantic.pick", "Default reducer: vote, mean score, consensus, judge, or first valid."),
        ("semantic.timeout", "Default clause timeout, in seconds."),
        ("semantic.budget", "Session USD cap. A wave that would pass it aborts."),
        (
            "semantic.max_cost",
            "Pre-flight USD cap. An estimate above this errors before any provider call. Empty disables the cap.",
        ),
        ("semantic.retry", "Default RETRY count."),
        ("semantic.threshold", "Default noul cutoff."),
        (
            "semantic.rerank_model",
            "Default RERANK model. Empty keeps jev noul. voyageai/rerank-3 is the recommended slug for pure ranking and calls OpenRouter /rerank.",
        ),
    ] {
        let _ = guc::DefineCustomStringVariable(name, Some(desc), None, Some(""), PGC_USERSET, 0);
    }
    let _ = guc::DefineCustomStringVariable(
        "semantic.cost_check",
        Some("When off, skip the pre-flight semantic budget check."),
        None,
        Some("on"),
        PGC_USERSET,
        0,
    );
    let _ = guc::DefineCustomStringVariable(
        "semantic.hutch_timeout",
        Some("HTTP read timeout for a hutch request, in seconds."),
        None,
        Some("120"),
        PGC_USERSET,
        0,
    );
    guc::MarkGUCPrefixReserved("semantic");
    let read = |name: &str| guc::GetConfigOption(name, true, false).ok().flatten();
    memo::set_guc_reader(read);
    clauses::set_guc_reader(read);
    rvbbit_cost::set_guc_reader(read);
    rvbbit_cost::set_history_refresher(pull_remote_history);
}

/// Nested SPI plans must not wipe the scan-row counts EXPLAIN recorded.
pub fn set_nested_plan_probe(probe: fn() -> bool) {
    rvbbit_cost::set_nested_plan_probe(probe);
}

/// `semantic.hutch_timeout` is seconds (`120` or `120s`). Empty, missing,
/// or unreadable falls back to 120s. The catalog `timeout_ms` of 30s is
/// not the client read timeout.
fn hutch_timeout_ms() -> u64 {
    let text = std::panic::catch_unwind(|| {
        guc::GetConfigOption("semantic.hutch_timeout", true, false)
            .ok()
            .flatten()
    })
    .ok()
    .flatten()
    .unwrap_or_default();
    parse_hutch_timeout_ms(&text)
}

fn parse_hutch_timeout_ms(raw: &str) -> u64 {
    let s = raw.trim();
    if s.is_empty() {
        return 120_000;
    }
    let s = s.strip_suffix('s').or_else(|| s.strip_suffix('S')).unwrap_or(s).trim();
    match s.parse::<f64>() {
        Ok(n) if n.is_finite() && n > 0.0 => (n * 1000.0).round() as u64,
        _ => 120_000,
    }
}

/// Same split `summarize` and the other batched ops use: `batch_rows`
/// items per POST (default 32). The pool limiter is the backend
/// concurrency (default 4).
fn extract_chunk_sizes(n: usize, width: usize) -> Vec<usize> {
    let width = width.max(1);
    if n == 0 {
        return Vec::new();
    }
    (0..n).step_by(width).map(|start| (n - start).min(width)).collect()
}

/// One short read of hutch `/stats` and `/v1/calls` so the next plan can
/// price from observed latency and USD. Later calls update the same table
/// as their responses arrive. Missing hutch is ignored.
fn pull_remote_history() {
    static LOADED: AtomicBool = AtomicBool::new(false);
    if LOADED.load(Ordering::Relaxed) {
        return;
    }
    let Ok(url) = std::env::var("RVBBIT_HUTCH_URL") else {
        return;
    };
    let base = url.trim().trim_end_matches('/');
    if base.is_empty() {
        return;
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(750))
        .build();
    let calls = match agent.get(&format!("{base}/v1/calls?limit=256")).call() {
        Ok(resp) => resp.into_string().map(|text| rvbbit_cost::ingest_calls_json(&text)).unwrap_or(0),
        Err(_) => 0,
    };
    if calls > 0 {
        LOADED.store(true, Ordering::Relaxed);
        return;
    }
    if let Ok(resp) = agent.get(&format!("{base}/stats")).call() {
        if let Ok(text) = resp.into_string() {
            if rvbbit_cost::ingest_stats_json(&text) > 0 {
                LOADED.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Clear the per-query memo. Executor start calls this.
pub fn begin_query() {
    memo::begin_query();
    let outer = !rvbbit_cost::execution_in_progress();
    rvbbit_cost::begin_query();
    if outer {
        clear_summary_cache();
    }
}

pub struct Ticket {
    rx: Receiver<Result<RawReply, String>>,
    /// Item keys in request order. Empty when nothing was peeled.
    order: Vec<String>,
    /// Cache hits that were not posted.
    ahead: Vec<rvbbit_contract::ItemResponse>,
    /// `(cache_key, item key)` for the items that were posted, so the
    /// reply can fill the memo.
    posted: Vec<(String, String)>,
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
        return Ok(Ticket {
            rx,
            order: Vec::new(),
            ahead: Vec::new(),
            posted: Vec::new(),
        });
    }
    let body = encode_predict(&ep, items).map_err(pg)?;
    let auth = bearer(&ep).map_err(pg)?;
    send(
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
    let ticket = send(
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
    let ticket = send(
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
    let ticket = send(
        format!("{}/v1/predict", ep.base_url.trim_end_matches('/')),
        auth,
        &ep,
        body,
        cache_header(cache_mode_of(&ep), None, None),
    )?;
    let raw = wait_raw(ticket)?;
    decode_score(&raw.body).map_err(pg)
}

/// ABOUT: embed the row and the criterion, then cosine against
/// [`ABOUT_THRESHOLD`] or the function threshold. One `/v1/embed` carries
/// every distinct text in the batch. The query embedding is cached with
/// the row embeddings, keyed by text and model.
pub fn about_batch(func_oid: u32, items: &[(String, String)]) -> PgResult<BatchReply> {
    if items.is_empty() {
        return Ok(BatchReply { matched: Vec::new(), http_calls: 0 });
    }
    let ep = rvbbit_resolve::endpoint_for(func_oid);
    let threshold = ep.threshold.unwrap_or(ABOUT_THRESHOLD);
    let (map, http_calls) = embed_map(&ep, pair_texts(items))?;
    let matched = items
        .iter()
        .map(|(left, right)| {
            let a = map.get(left).map(Vec::as_slice).unwrap_or(&[]);
            let b = map.get(right).map(Vec::as_slice).unwrap_or(&[]);
            rvbbit_contract::cosine(a, b) >= threshold
        })
        .collect();
    Ok(BatchReply { matched, http_calls })
}

/// Cosine of each pair. Texts are embedded once per distinct value.
pub fn similarity_many(pairs: &[(String, String)]) -> PgResult<ManyReply> {
    if pairs.is_empty() {
        return Ok(ManyReply { values: Vec::new(), http_calls: 0 });
    }
    let ep = embed_source_endpoint();
    let (map, http_calls) = embed_map(&ep, pair_texts(pairs))?;
    let mut values = Vec::with_capacity(pairs.len());
    for (left, right) in pairs {
        let a = map.get(left).map(Vec::as_slice).unwrap_or(&[]);
        let b = map.get(right).map(Vec::as_slice).unwrap_or(&[]);
        values.push(Ok(rvbbit_contract::cosine(a, b).to_string()));
    }
    Ok(ManyReply { values, http_calls })
}

pub fn scalar_key_similarity() -> String {
    let ep = embed_source_endpoint();
    format!("sim\u{1e}{}", embed_model_name(&ep.model))
}

fn pair_texts(items: &[(String, String)]) -> Vec<String> {
    let mut texts = Vec::new();
    for (left, right) in items {
        if !texts.iter().any(|t| t == left) {
            texts.push(left.clone());
        }
        if !texts.iter().any(|t| t == right) {
            texts.push(right.clone());
        }
    }
    texts
}

fn embed_map(ep: &Endpoint, texts: Vec<String>) -> PgResult<(HashMap<String, Vec<f64>>, u32)> {
    let (vectors, http_calls) = embed_vectors(ep, &texts)?;
    let mut map = HashMap::new();
    for (text, vector) in texts.into_iter().zip(vectors) {
        map.insert(text, vector);
    }
    Ok((map, http_calls))
}

fn embed_vectors(ep: &Endpoint, texts: &[String]) -> PgResult<(Vec<Vec<f64>>, u32)> {
    if texts.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let ep = embed_endpoint(ep);
    let inputs: Vec<serde_json::Value> = texts.iter().map(|text| serde_json::json!({ "input": text })).collect();
    let body = encode_items(&ep, Operation::Embed, &inputs).map_err(pg)?;
    let raw = post_op(&ep, "embed", body)?;
    let parsed = parsed_batch(&raw.body).map_err(pg)?;
    if parsed.outputs.len() != texts.len() {
        return Err(pg(format!(
            "hutch embed returned {} vectors for {} texts",
            parsed.outputs.len(),
            texts.len()
        )));
    }
    let mut out = Vec::with_capacity(texts.len());
    for (i, item) in parsed.outputs.iter().enumerate() {
        if let Some(err) = &item.error {
            return Err(pg(format!("hutch embed item {i}: {} {}", err.code, err.message)));
        }
        let Some(vector) = item.value.as_ref().and_then(json_vector) else {
            return Err(pg(format!("hutch embed item {i} has no vector")));
        };
        out.push(vector);
    }
    Ok((out, raw.http_calls))
}

fn json_vector(value: &serde_json::Value) -> Option<Vec<f64>> {
    let rows = value.as_array()?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(row.as_f64()?);
    }
    Some(out)
}

fn embed_source_endpoint() -> Endpoint {
    let oid = rvbbit_resolve::func_oid("about")
        .or_else(|| rvbbit_resolve::func_oid("means"))
        .unwrap_or(rvbbit_oids::MEANS_OID);
    rvbbit_resolve::endpoint_for(oid)
}

fn embed_endpoint(ep: &Endpoint) -> Endpoint {
    let mut out = ep.clone();
    out.fn_name = "embed".into();
    out.fn_version = 1;
    out.system_prompt.clear();
    out.user_prompt.clear();
    out.labels.clear();
    out.label_descriptions.clear();
    out.threshold = None;
    out.default_clauses = None;
    out.extract_mode = None;
    out.arg_names = vec!["input".into()];
    out.return_type = "vector".into();
    out.user_fn = false;
    out.model = embed_model_name(&ep.model);
    out.temperature = 0.0;
    out.max_tokens = None;
    out
}

fn embed_model_name(model: &str) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("mock")
        || trimmed.eq_ignore_ascii_case("default")
        || trimmed.eq_ignore_ascii_case("jev")
    {
        EMBED_MODEL.to_string()
    } else {
        trimmed.to_string()
    }
}

fn cache_mode_of(ep: &Endpoint) -> CacheMode {
    match memo::cache_mode().as_str() {
        "off" => return CacheMode::Off,
        "refresh" => return CacheMode::Refresh,
        _ => {}
    }
    match ep.cache_mode.as_str() {
        "off" => CacheMode::Off,
        "refresh" => CacheMode::Refresh,
        _ => CacheMode::ReadWrite,
    }
}

/// `POST /v1/cache/lookup`. The operation path also calls this before
/// posting misses. Exposed so a test can speak the contract directly.
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

/// Split a classify label list. U+001F is the native rewrite's separator
/// and is kept as written. A comma-separated list is the function-call
/// form; each label is trimmed.
pub fn split_classify_labels(raw: &str) -> Vec<String> {
    if raw.contains('\u{1f}') {
        raw.split('\u{1f}').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect()
    } else {
        raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).map(|s| s.to_string()).collect()
    }
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

/// Chunked HTTP extract. The model on the wire is `mock`.
/// Returned strings are still raw model text; [`coerce_extracted`] parses
/// `int` / `float` / `bool`. Each POST is at most `batch_rows` items.
pub fn extract_many(kind: &str, inputs: &[String]) -> PgResult<ManyReply> {
    if inputs.is_empty() {
        return Ok(ManyReply { values: Vec::new(), http_calls: 0 });
    }
    let ep = typed_extract_endpoint(kind);
    let payload: Vec<serde_json::Value> = inputs
        .iter()
        .map(|input| serde_json::json!({ "input": input }))
        .collect();
    let (values, http_calls) = post_chunked_extract(&ep, &payload)?;
    if values.len() != inputs.len() {
        return Err(pg(format!(
            "hutch extract returned {} items for {} inputs",
            values.len(),
            inputs.len()
        )));
    }
    let values = values
        .into_iter()
        .map(|item| item.map(|raw| scrub_scalar_value(kind, &raw)))
        .collect();
    Ok(ManyReply { values, http_calls })
}

/// Wire value for a scalar miss. Not an error, so `ON ERROR` does not fire.
pub const EXTRACT_ABSENT: &str = "\u{1}absent";

fn scrub_scalar_value(kind: &str, raw: &str) -> String {
    if raw == EXTRACT_ABSENT || rvbbit_contract::placeholder_extract(kind, raw) {
        EXTRACT_ABSENT.to_string()
    } else {
        raw.to_string()
    }
}

/// Chunked HTTP extract. Each value is a JSON array of strings
/// of `kind` only, or `[]`. The array is stored as text; the SQL lateral
/// join explodes it. Unlike [`extract_many`], the strings are not coerced.
pub fn extract_lists(kind: &str, inputs: &[String]) -> PgResult<ManyReply> {
    if inputs.is_empty() {
        return Ok(ManyReply { values: Vec::new(), http_calls: 0 });
    }
    let ep = many_endpoint(kind);
    let payload: Vec<serde_json::Value> = inputs
        .iter()
        .map(|input| serde_json::json!({ "input": input }))
        .collect();
    let (values, http_calls) = post_chunked_extract(&ep, &payload)?;
    if values.len() != inputs.len() {
        return Err(pg(format!(
            "hutch extract many returned {} items for {} inputs",
            values.len(),
            inputs.len()
        )));
    }
    let values = filter_listed(kind, values);
    Ok(ManyReply { values, http_calls })
}

fn post_chunked_extract(
    ep: &Endpoint,
    inputs: &[serde_json::Value],
) -> PgResult<(Vec<Result<String, String>>, u32)> {
    if inputs.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let width = rvbbit_resolve::batch_rows().max(1);
    let mut tickets = Vec::new();
    let mut start = 0;
    for size in extract_chunk_sizes(inputs.len(), width) {
        let chunk = &inputs[start..start + size];
        start += size;
        let body = encode_items(ep, Operation::Extract, chunk).map_err(pg)?;
        tickets.push(post_ticket(ep, "extract", body)?);
    }
    let mut values = Vec::with_capacity(inputs.len());
    let mut http_calls = 0u32;
    for ticket in tickets {
        let raw = wait_raw(ticket)?;
        http_calls = http_calls.saturating_add(raw.http_calls);
        values.extend(decode_text_items(&raw.body).map_err(pg)?);
    }
    Ok((values, http_calls))
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
    let ticket = send(
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

/// Memo key for an EXTRACT MANY list. Distinct from [`scalar_key_extract`]
/// so a one-noun extract and a JSON array of the same kind do not collide.
pub fn scalar_key_extract_many(kind: &str) -> String {
    let ep = decision_endpoint();
    format!("m\u{1e}{}\u{1e}mock\u{1e}{kind}", ep.base_url)
}

fn scalar_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["found", "value"],
        "properties": {
            "found": {
                "type": "boolean",
                "description": "False when the text does not name the requested kind."
            },
            "value": {
                "type": "string",
                "description": "The name copied from the text, or empty when found is false."
            }
        }
    })
}

fn typed_extract_endpoint(kind: &str) -> Endpoint {
    let mut ep = decision_endpoint();
    ep.model = "mock".into();
    ep.labels.clear();
    ep.label_descriptions.clear();
    ep.fn_name = "extract_typed".into();
    // Same lock as EXTRACT MANY: temperature 0, seed 0, strict object.
    // found=false is a successful miss (SQL NULL), not a placeholder and
    // not an error, so ON ERROR does not replace it.
    ep.temperature = 0.0;
    let schema_text = scalar_schema().to_string();
    ep.return_type = schema_text.clone();
    ep.extract_mode = Some("scalar".into());
    ep.system_prompt = format!(
        "Name the {kind} in at most three words. JSON only, no markdown, schema {schema_text}. \
         Set found to true and value to the name only when the text names a {kind}. \
         If it does not, set found to false and value to \"\". \
         Do not invent a placeholder such as None, Unknown, No person named, or No name provided. \
         Leave out anything that is not a {kind}."
    );
    ep.user_prompt = format!(
        "What {kind} does this text name? If none, found is false and value is \"\".\n{{{{input}}}}"
    );
    ep.max_tokens = Some(64);
    ep
}

fn summary_endpoint() -> Endpoint {
    let mut ep = decision_endpoint();
    ep.model = "mock".into();
    ep.labels.clear();
    ep.label_descriptions.clear();
    ep.fn_name = "summarize".into();
    ep.return_type = "text".into();
    ep.extract_mode = Some("summary".into());
    ep.system_prompt = "Summarize the notes in one or two sentences. No markdown.".into();
    ep.user_prompt = "{{input}}".into();
    ep.max_tokens = Some(128);
    ep
}

/// Object schema so a provider can lock the reply with structured output.
/// The root is an object because strict JSON Schema rejects a bare array.
/// Each mention carries a category. Hutch unwraps `mentions`; the filter
/// keeps only `specific_instance_of_kind`.
fn many_schema(kind: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["mentions"],
        "properties": {
            "mentions": {
                "type": "array",
                "description": format!("Each {kind} copied from the text, or empty."),
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["mention", "category"],
                    "properties": {
                        "mention": {
                            "type": "string",
                            "description": format!("The whole noun phrase for one {kind}.")
                        },
                        "category": {
                            "type": "string",
                            "enum": [
                                "specific_instance_of_kind",
                                "generic_kind_word",
                                "attribute_or_part",
                                "place",
                                "sound_or_event",
                                "other"
                            ]
                        }
                    }
                }
            }
        }
    })
}

fn extract_cache_seed(mode: Option<&str>) -> &'static str {
    if matches!(mode, Some("many") | Some("scalar")) {
        "0"
    } else {
        ""
    }
}

fn response_schema(ep: &Endpoint) -> Option<serde_json::Value> {
    match ep.extract_mode.as_deref() {
        Some("many") | Some("scalar") => serde_json::from_str(&ep.return_type).ok(),
        _ => None,
    }
}

fn many_endpoint(kind: &str) -> Endpoint {
    let mut ep = decision_endpoint();
    ep.model = "mock".into();
    ep.labels.clear();
    ep.label_descriptions.clear();
    ep.fn_name = "extract_many".into();
    // The means row's temperature must not leak in. Sampling is why the
    // same note returned Bigfoot one run and [] the next.
    ep.temperature = 0.0;
    let schema_text = many_schema(kind).to_string();
    ep.return_type = schema_text.clone();
    ep.extract_mode = Some("many".into());
    ep.system_prompt = format!(
        "Extract every {kind}. JSON only, no markdown, schema {schema_text}. \
         Each item has mention and category. \
         category is specific_instance_of_kind, generic_kind_word, attribute_or_part, place, sound_or_event, or other. \
         Copy the whole noun phrase from the user text (\"black dog\", not \"dog\"). \
         {} \
         Copy only words from the user text. Leave out anything that is not a {kind}. \
         If there are none, mentions is [].",
        many_examples()
    );
    ep.user_prompt = format!(
        "List every {kind}. Copy the whole noun phrase. \
         Set category to specific_instance_of_kind, generic_kind_word, attribute_or_part, place, sound_or_event, or other. \
         Leave out anything that is not a {kind}. If there are none, mentions is [].\n{{{{input}}}}"
    );
    ep.max_tokens = Some(256);
    ep
}

/// Short recall cues. The category enum does the filtering; these only
/// show the object shape and the whole noun phrase.
fn many_examples() -> String {
    "Examples, do not copy: creature in \"My story of Bigfoot\" is \
     {\"mention\":\"Bigfoot\",\"category\":\"specific_instance_of_kind\"}; \
     color in \"Large gray triangular aircraft\" is \
     {\"mention\":\"gray\",\"category\":\"specific_instance_of_kind\"}; \
     creature in \"a sasquatch crossed the road\" is \
     {\"mention\":\"sasquatch\",\"category\":\"specific_instance_of_kind\"}."
        .to_string()
}

/// One mention from the chat list. `category` is absent when the model
/// returned a bare string (the schema-rejected fallback). Those are
/// treated as specific, then the stoplist and phrase rule still apply.
#[derive(Clone, Debug)]
struct Mention {
    text: String,
    category: Option<String>,
}

const CATEGORY_SPECIFIC: &str = "specific_instance_of_kind";

fn generic_kind_word(kind: &str, text: &str) -> bool {
    let folded = fold_words(text);
    if folded.is_empty() {
        return true;
    }
    if folded == fold_words(kind) {
        return true;
    }
    const GENERIC: &[&str] = &[
        "animal",
        "animals",
        "man",
        "men",
        "person",
        "people",
        "human",
        "humans",
        "creature",
        "creatures",
        "thing",
        "being",
        "entity",
        "shellfish",
        "sea life",
        "human like figure",
        "humanlike figure",
        "life form",
        "critter",
    ];
    GENERIC.contains(&folded.as_str())
}

/// Tokens that are not a species, breed, or color by themselves.
/// A mention made only of these is junk (`night`, `human like figure`)
/// and is dropped before jev. A mention that also has a concrete word
/// (`big buck`, `black dog`) is kept whole.
fn is_stop_or_vague(word: &str) -> bool {
    const VAGUE: &[&str] = &[
        "a",
        "an",
        "and",
        "animal",
        "animals",
        "being",
        "big",
        "bright",
        "creature",
        "creatures",
        "critter",
        "crossed",
        "dark",
        "dim",
        "entity",
        "figure",
        "figures",
        "giant",
        "huge",
        "human",
        "humans",
        "life",
        "light",
        "like",
        "little",
        "large",
        "man",
        "men",
        "my",
        "night",
        "of",
        "people",
        "person",
        "road",
        "round",
        "sea",
        "shellfish",
        "small",
        "some",
        "story",
        "the",
        "thing",
        "things",
        "triangle",
        "triangular",
        "tiny",
        "very",
    ];
    VAGUE.contains(&word)
}

fn is_color_word(word: &str) -> bool {
    const COLORS: &[&str] = &[
        "auburn", "beige", "black", "blond", "blonde", "blue", "brown", "chestnut", "cream",
        "gold", "golden", "gray", "green", "grey", "ivory", "khaki", "maroon", "navy", "olive",
        "orange", "pink", "purple", "red", "roan", "russet", "silver", "tan", "teal", "white",
        "yellow",
    ];
    COLORS.contains(&word)
}

fn is_color_kind(kind: &str) -> bool {
    let kind = fold_words(kind);
    kind == "color" || kind == "colour"
}

fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// Every token is a generic or a modifier, so the mention is not a
/// creature or a color (`night`, `bright`, `human like figure`).
fn vague_only(text: &str) -> bool {
    let words = tokens(text);
    !words.is_empty() && words.iter().all(|w| is_stop_or_vague(w))
}

fn fold_words(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase()
}

/// A color mention is the color word itself. `gray aircraft` is not a color.
fn color_words_only(text: &str) -> bool {
    let words = tokens(text);
    !words.is_empty() && words.iter().all(|w| is_color_word(w))
}

/// Keep a specific instance. Drop every other category, the generic
/// stoplist, and a color mention that is not itself a color word.
/// A bare string (no category) counts as specific so a schema fallback
/// still returns Bigfoot, then the stoplist applies.
fn keep_mention(kind: &str, mention: &Mention) -> bool {
    let text = mention.text.trim();
    if text.is_empty() {
        return false;
    }
    match mention.category.as_deref() {
        None | Some(CATEGORY_SPECIFIC) => {}
        Some(_) => return false,
    }
    if generic_kind_word(kind, text) || vague_only(text) {
        return false;
    }
    if is_color_kind(kind) && !color_words_only(text) {
        return false;
    }
    true
}

/// `dog` is a contiguous subslice of `black dog`, so the shorter one goes.
/// `deer` is not a subslice of `big buck`, so both stay.
fn is_contiguous_subslice(shorter: &[String], longer: &[String]) -> bool {
    if shorter.is_empty() || shorter.len() >= longer.len() {
        return false;
    }
    longer.windows(shorter.len()).any(|window| window == shorter)
}

/// Category filter, then the stoplist, then the whole-phrase rule.
/// Case-insensitive duplicates collapse to the first spelling.
fn select_mentions(kind: &str, mentions: &[Mention]) -> String {
    let kept: Vec<&Mention> = mentions.iter().filter(|m| keep_mention(kind, m)).collect();
    let tokenized: Vec<Vec<String>> = kept.iter().map(|m| tokens(&m.text)).collect();
    let mut covered = vec![false; kept.len()];
    for i in 0..kept.len() {
        for j in 0..kept.len() {
            if i != j && is_contiguous_subslice(&tokenized[i], &tokenized[j]) {
                covered[i] = true;
                break;
            }
        }
    }
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for (index, mention) in kept.iter().enumerate() {
        if covered[index] {
            continue;
        }
        let key = tokens(&mention.text).join(" ");
        if key.is_empty() || seen.iter().any(|s: &String| s == &key) {
            continue;
        }
        seen.push(key);
        out.push(mention.text.trim());
    }
    serde_json::to_string(&out).unwrap_or_else(|_| "[]".into())
}

fn filter_mentions(kind: &str, text: &str) -> String {
    match parse_mentions(text) {
        Some(mentions) => select_mentions(kind, &mentions),
        None => text.to_string(),
    }
}

fn filter_listed(kind: &str, values: Vec<Result<String, String>>) -> Vec<Result<String, String>> {
    values
        .into_iter()
        .map(|value| value.map(|text| filter_mentions(kind, &text)))
        .collect()
}

fn parse_mentions(text: &str) -> Option<Vec<Mention>> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let arr = value.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        if let Some(s) = item.as_str() {
            let s = s.trim();
            if s.is_empty() {
                continue;
            }
            out.push(Mention { text: s.to_string(), category: None });
            continue;
        }
        let obj = item.as_object()?;
        let text = obj
            .get("mention")
            .and_then(|t| t.as_str())
            .or_else(|| obj.get("text").and_then(|t| t.as_str()))
            .unwrap_or("")
            .trim();
        if text.is_empty() {
            continue;
        }
        let category = obj
            .get("category")
            .and_then(|c| c.as_str())
            .map(|c| c.trim().to_ascii_lowercase())
            .filter(|c| !c.is_empty());
        out.push(Mention { text: text.to_string(), category });
    }
    Some(out)
}

fn empty_agg() -> serde_json::Value {
    serde_json::json!({"clauses": "", "extra": "", "parts": []})
}

fn read_agg(state: &str) -> (String, String, Vec<String>) {
    let v = serde_json::from_str::<serde_json::Value>(state).unwrap_or_else(|_| empty_agg());
    let clauses = v.get("clauses").and_then(|s| s.as_str()).unwrap_or("").to_string();
    let extra = v.get("extra").and_then(|s| s.as_str()).unwrap_or("").to_string();
    let parts = v
        .get("parts")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    (clauses, extra, parts)
}

/// Append one group row. `extra` is the consensus criterion. `clauses` is
/// the M5 tail, stored once from the first row.
pub fn push_agg(state: &str, value: &str, extra: &str, clauses: &str) -> String {
    let mut v = serde_json::from_str::<serde_json::Value>(state).unwrap_or_else(|_| empty_agg());
    if !v.is_object() {
        v = empty_agg();
    }
    if v.get("clauses").and_then(|s| s.as_str()).unwrap_or("").is_empty() && !clauses.is_empty() {
        v["clauses"] = serde_json::json!(clauses);
    }
    if v.get("extra").and_then(|s| s.as_str()).unwrap_or("").is_empty() && !extra.is_empty() {
        v["extra"] = serde_json::json!(extra);
    }
    if !v.get("parts").is_some_and(|p| p.is_array()) {
        v["parts"] = serde_json::json!([]);
    }
    v["parts"].as_array_mut().expect("parts").push(serde_json::json!(value));
    v.to_string()
}

pub fn summarize_state(state: &str) -> PgResult<Option<String>> {
    if let Some(hit) = cached_summary(state) {
        return Ok(Some(hit));
    }
    let (clauses, _extra, parts) = read_agg(state);
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(summarize_text(&parts.join("\n\n"), &clauses)?))
}

/// Hash aggregation calls this once per filled batch, before any final
/// function runs. Groups with no clause tail share one extract body, and
/// hutch runs those chat calls under its concurrency cap. The finals then
/// read the cache instead of posting one group at a time.
pub fn prefetch_summaries(states: &[String]) {
    let _ = prefetch_summaries_inner(states);
}

fn prefetch_summaries_inner(states: &[String]) -> Result<(), String> {
    let mut simple_state = Vec::new();
    let mut simple_input = Vec::new();
    let mut complex: Vec<(String, String, String)> = Vec::new();
    for state in states {
        let (clauses, _extra, parts) = read_agg(state);
        if parts.is_empty() {
            continue;
        }
        let input = parts.join("\n\n");
        if clauses.trim().is_empty() {
            simple_state.push(state.to_string());
            simple_input.push(input);
        } else {
            complex.push((state.to_string(), input, clauses));
        }
    }
    if !simple_input.is_empty() {
        let reply = summarize_many(&simple_input).map_err(|e| e.to_string())?;
        if reply.values.len() == simple_state.len() {
            let mut cache = summary_cache();
            for (state, value) in simple_state.into_iter().zip(reply.values) {
                if let Ok(text) = value {
                    cache.insert(state, text);
                }
            }
        }
    }
    for (state, input, clauses) in complex {
        if let Ok(text) = summarize_text(&input, &clauses) {
            summary_cache().insert(state, text);
        }
    }
    Ok(())
}

fn summarize_many(inputs: &[String]) -> PgResult<ManyReply> {
    if inputs.is_empty() {
        return Ok(ManyReply { values: Vec::new(), http_calls: 0 });
    }
    let ep = summary_endpoint();
    let payload: Vec<serde_json::Value> = inputs
        .iter()
        .map(|input| serde_json::json!({ "input": input }))
        .collect();
    let (values, http_calls) = post_chunked_extract(&ep, &payload)?;
    if values.len() != inputs.len() {
        return Err(pg(format!(
            "hutch summarize returned {} items for {} groups",
            values.len(),
            inputs.len()
        )));
    }
    Ok(ManyReply { values, http_calls })
}

fn post_ticket(ep: &Endpoint, op: &str, body: String) -> PgResult<Ticket> {
    let auth = bearer(ep).map_err(pg)?;
    send(
        format!("{}/v1/{op}", ep.base_url.trim_end_matches('/')),
        auth,
        ep,
        body,
        cache_header(cache_mode_of(ep), None, None),
    )
}

static SUMMARY_CACHE: std::sync::LazyLock<Mutex<std::collections::HashMap<String, String>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

fn summary_cache() -> std::sync::MutexGuard<'static, std::collections::HashMap<String, String>> {
    SUMMARY_CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

fn cached_summary(state: &str) -> Option<String> {
    summary_cache().get(state).cloned()
}

fn clear_summary_cache() {
    summary_cache().clear();
}

pub fn summarize_text(input: &str, clauses: &str) -> PgResult<String> {
    if clauses.trim().is_empty() {
        let ep = summary_endpoint();
        let body = encode_items(&ep, Operation::Extract, &[serde_json::json!({ "input": input })])
            .map_err(pg)?;
        let raw = post_op(&ep, "extract", body)?;
        let hit = decode_clause(&raw.body).map_err(pg)?;
        if let Some(err) = hit.error {
            return Err(pg(err));
        }
        return Ok(hit.value);
    }
    winner_of(&apply_clauses_text("summarize", input, "", clauses)?)
}

pub fn consensus_state(state: &str) -> PgResult<Option<bool>> {
    let (clauses, extra, parts) = read_agg(state);
    if parts.is_empty() {
        return Ok(None);
    }
    let input = parts.join("\n\n");
    if clauses.trim().is_empty() {
        return Ok(Some(means_oid(rvbbit_oids::MEANS_OID, &input, &extra)?));
    }
    let winner = winner_of(&apply_clauses_text("consensus", &input, &extra, &clauses)?)?;
    Ok(Some(winner == "true"))
}

pub fn rerank_score(document: &str, query: &str) -> PgResult<f64> {
    let reply = rerank_many("", query, &[document.to_string()])?;
    let text = one_item(reply)?;
    text.parse::<f64>().map_err(|_| pg("rerank returned no score"))
}

/// One hutch POST for every document. Hutch turns a dedicated model into
/// one OpenRouter `/rerank` call and leaves `mock` / `jev` on noul.
/// Scores come back as decimal text, in `documents` order.
pub fn rerank_many(model: &str, query: &str, documents: &[String]) -> PgResult<ManyReply> {
    if documents.is_empty() {
        return Ok(ManyReply { values: Vec::new(), http_calls: 0 });
    }
    let mut ep = decision_endpoint();
    ep.fn_name = "rerank".into();
    ep.model = rerank_model_for(model);
    ep.system_prompt = "Is this document relevant to the query?".into();
    ep.user_prompt.clear();
    let payload: Vec<serde_json::Value> = documents
        .iter()
        .map(|document| serde_json::json!({ "input": document, "criterion": query }))
        .collect();
    let body = encode_items(&ep, Operation::Rerank, &payload).map_err(pg)?;
    let raw = post_op(&ep, "rerank", body)?;
    let values = decode_score_items(&raw.body).map_err(pg)?;
    if values.len() != documents.len() {
        return Err(pg(format!(
            "hutch rerank returned {} scores for {} documents",
            values.len(),
            documents.len()
        )));
    }
    Ok(ManyReply { values, http_calls: raw.http_calls })
}

/// `USING MODEL` wins. An explicit `jev` stays on noul. Otherwise the
/// `semantic.rerank_model` GUC, then `mock` so hutch can apply
/// `HUTCH_RERANK_MODEL` or the jev fallback.
pub fn rerank_model_for(explicit: &str) -> String {
    let explicit = explicit.trim();
    if explicit.eq_ignore_ascii_case("jev") {
        return "jev".into();
    }
    if !explicit.is_empty() && explicit != "mock" && explicit != "default" {
        return explicit.to_string();
    }
    clauses::guc_rerank_model().unwrap_or_else(|| "mock".into())
}

pub fn scalar_key_rerank(model: &str, query: &str) -> String {
    let ep = decision_endpoint();
    format!(
        "r\u{1e}{}\u{1e}{}\u{1e}{query}",
        ep.base_url,
        rerank_model_for(model)
    )
}

pub fn extract_many_json(kind: &str, input: &str, clauses: &str) -> PgResult<String> {
    let listed = if clauses.trim().is_empty() {
        let ep = many_endpoint(kind);
        let body = encode_items(&ep, Operation::Extract, &[serde_json::json!({ "input": input })])
            .map_err(pg)?;
        let raw = post_op(&ep, "extract", body)?;
        let hit = decode_clause(&raw.body).map_err(pg)?;
        if let Some(err) = hit.error {
            return Err(pg(err));
        }
        hit.value
    } else {
        winner_of(&apply_clauses_text("extract_many", input, kind, clauses)?)?
    };
    match filter_listed(kind, vec![Ok(listed)]).into_iter().next() {
        Some(Ok(value)) => Ok(value),
        Some(Err(message)) => Err(pg(message)),
        None => Ok("[]".into()),
    }
}

fn winner_of(report: &str) -> PgResult<String> {
    let v: serde_json::Value = serde_json::from_str(report).map_err(|e| pg(e.to_string()))?;
    Ok(match v.get("winner") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    })
}

fn decision_endpoint() -> Endpoint {
    let oid = rvbbit_resolve::func_oid("means").unwrap_or(rvbbit_oids::MEANS_OID);
    rvbbit_resolve::endpoint_for(oid)
}

fn decode_score_items(text: &str) -> Result<Vec<Result<String, String>>, String> {
    let parsed = parsed_batch(text)?;
    let mut out = Vec::with_capacity(parsed.outputs.len());
    for (i, item) in parsed.outputs.iter().enumerate() {
        if let Some(err) = &item.error {
            out.push(Err(format!("hutch item {i}: {} {}", err.code, err.message)));
            continue;
        }
        match item.score {
            Some(score) => out.push(Ok(score.to_string())),
            None => out.push(Err(format!("hutch item {i} has no rerank score"))),
        }
    }
    Ok(out)
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
    let ep = rvbbit_resolve::endpoint_for(func_oid);
    if ep.fn_name.eq_ignore_ascii_case("about") {
        let reply = about_batch(func_oid, &[(input.to_string(), criterion.to_string())])?;
        return Ok(reply.matched.first().copied().unwrap_or(false));
    }
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
        timeout_ms: hutch_timeout_ms(),
        retries: ep.retries.max(1),
        body,
        cache_header,
        audit: memo::audit_payloads(),
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
    Ok(Ticket {
        rx,
        order: Vec::new(),
        ahead: Vec::new(),
        posted: Vec::new(),
    })
}

fn send(
    url: String,
    auth: Option<String>,
    ep: &Endpoint,
    body: String,
    cache_header: String,
) -> PgResult<Ticket> {
    if let Err(message) = rvbbit_cost::preflight() {
        return Err(pg(message));
    }
    let prepared = prepare_batch(ep, &url, &body, &cache_header)?;
    match prepared {
        Prepared::Local(body) => {
            let (tx, rx) = mpsc::channel();
            let _ = tx.send(Ok(RawReply { body, http_calls: 0 }));
            Ok(Ticket {
                rx,
                order: Vec::new(),
                ahead: Vec::new(),
                posted: Vec::new(),
            })
        }
        Prepared::Remote(PreparedRemote { body, order, ahead, posted }) => {
            let mut ticket = enqueue(url, auth, ep, body, cache_header)?;
            ticket.order = order;
            ticket.ahead = ahead;
            ticket.posted = posted;
            Ok(ticket)
        }
    }
}

struct PreparedRemote {
    body: String,
    order: Vec<String>,
    ahead: Vec<rvbbit_contract::ItemResponse>,
    posted: Vec<(String, String)>,
}

enum Prepared {
    Local(String),
    Remote(PreparedRemote),
}

fn prepare_batch(ep: &Endpoint, url: &str, body: &str, header: &str) -> PgResult<Prepared> {
    if url.contains("/v1/cache/") || !header.contains("mode=read-write") {
        return Ok(remote_unchanged(body));
    }
    let Ok(mut req) = serde_json::from_str::<BatchRequest>(body) else {
        return Ok(remote_unchanged(body));
    };
    let order: Vec<String> = req.items.iter().map(|item| item.key.clone()).collect();
    let mut ahead = Vec::new();
    let mut unknown = Vec::new();
    for item in &req.items {
        if let Some(hit) = memo::recall(&item.cache_key) {
            let mut response = memo::hit_response(&item.key, &hit);
            response.key = item.key.clone();
            ahead.push(response);
        } else {
            unknown.push(item.cache_key.clone());
        }
    }
    if !unknown.is_empty() {
        match cache_lookup_ep(ep, &unknown) {
            Ok(found) => {
                let mut hit_keys = std::collections::HashSet::new();
                for hit in found.hits {
                    hit_keys.insert(hit.key.clone());
                    memo::remember(&hit.key, hit.output.clone(), hit.score);
                    if let Some(item) = req.items.iter().find(|item| item.cache_key == hit.key) {
                        ahead.push(rvbbit_contract::ItemResponse {
                            key: item.key.clone(),
                            value: Some(hit.output),
                            score: hit.score,
                            scores: None,
                            calls: None,
                            error: None,
                            identical_takes: None,
                        });
                    }
                }
                let _ = hit_keys;
            }
            Err(_) => {}
        }
    }
    let ahead_keys: std::collections::HashSet<String> =
        ahead.iter().map(|item| item.key.clone()).collect();
    if ahead_keys.len() == order.len() && !order.is_empty() {
        let outputs = order
            .iter()
            .filter_map(|key| ahead.iter().find(|item| &item.key == key).cloned())
            .collect();
        let response = BatchResponse {
            contract: CONTRACT_VERSION.to_string(),
            outputs,
        };
        let json = serde_json::to_string(&response).map_err(|e| pg(e.to_string()))?;
        return Ok(Prepared::Local(json));
    }
    let posted: Vec<(String, String)> = req
        .items
        .iter()
        .filter(|item| !ahead_keys.contains(&item.key))
        .map(|item| (item.cache_key.clone(), item.key.clone()))
        .collect();
    req.items.retain(|item| !ahead_keys.contains(&item.key));
    if req.items.len() == order.len() {
        return Ok(remote_unchanged(body));
    }
    let body = serde_json::to_string(&req).map_err(|e| pg(e.to_string()))?;
    Ok(Prepared::Remote(PreparedRemote {
        body,
        order,
        ahead,
        posted,
    }))
}

fn remote_unchanged(body: &str) -> Prepared {
    Prepared::Remote(PreparedRemote {
        body: body.to_string(),
        order: Vec::new(),
        ahead: Vec::new(),
        posted: Vec::new(),
    })
}

fn cache_lookup_ep(ep: &Endpoint, keys: &[String]) -> PgResult<CacheLookupResponse> {
    let body = serde_json::to_string(&CacheLookupRequest { keys: keys.to_vec() })
        .map_err(|e| pg(format!("cache lookup encode: {e}")))?;
    let ticket = enqueue(
        format!("{}/v1/cache/lookup", ep.base_url.trim_end_matches('/')),
        bearer(ep).map_err(pg)?,
        ep,
        body,
        cache_header(CacheMode::Off, None, None),
    )?;
    let raw = wait_raw(ticket)?;
    serde_json::from_str(&raw.body).map_err(|e| pg(format!("cache lookup decode: {e}")))
}

fn observe_cost(body: &str, http_calls: u32) {
    let conc = rvbbit_resolve::max_concurrent().max(1) as u32;
    rvbbit_cost::observe_response(body, http_calls, conc);
}

fn wait_raw(ticket: Ticket) -> PgResult<RawReply> {
    let Ticket { rx, order, ahead, posted } = ticket;
    let raw = loop {
        check_cancel()?;
        match rx.recv_timeout(WAIT_SLICE) {
            Ok(Ok(v)) => break v,
            Ok(Err(e)) => return Err(pg(e)),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(pg("rvbbit dispatch worker dropped the reply"));
            }
        }
    };
    if order.is_empty() {
        observe_cost(&raw.body, raw.http_calls);
        return Ok(raw);
    }
    let mut parsed: BatchResponse = serde_json::from_str(&raw.body).map_err(|e| pg(e.to_string()))?;
    for (cache_key, item_key) in &posted {
        if let Some(item) = parsed.outputs.iter().find(|item| &item.key == item_key) {
            if let Some(value) = &item.value {
                memo::remember(cache_key, value.clone(), item.score);
            }
        }
    }
    let mut merged = Vec::with_capacity(order.len());
    for key in &order {
        if let Some(hit) = ahead.iter().find(|item| &item.key == key) {
            merged.push(hit.clone());
        } else if let Some(hit) = parsed.outputs.iter().find(|item| &item.key == key) {
            merged.push(hit.clone());
        }
    }
    parsed.outputs = merged;
    let body = serde_json::to_string(&parsed).map_err(|e| pg(e.to_string()))?;
    observe_cost(&body, raw.http_calls);
    Ok(RawReply {
        body,
        http_calls: raw.http_calls,
    })
}

fn encode_predict(ep: &Endpoint, items: &[(String, String)]) -> Result<String, String> {
    let inputs: Vec<serde_json::Value> = items
        .iter()
        .map(|(input, criterion)| predict_inputs(ep, input, criterion))
        .collect();
    encode_items(ep, Operation::Predict, &inputs)
}

/// `{{arg}}` values for one predict item. `input` and `criterion` stay so
/// the mock and the means operator keep their old shape. Named arguments
/// are added beside them.
pub fn predict_inputs(ep: &Endpoint, input: &str, criterion: &str) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("input".into(), serde_json::Value::String(input.to_string()));
    map.insert("criterion".into(), serde_json::Value::String(criterion.to_string()));
    if let Some(name) = ep.arg_names.first() {
        if !name.is_empty() && name != "input" && name != "criterion" {
            map.insert(name.clone(), serde_json::Value::String(input.to_string()));
        }
    }
    if ep.arg_names.len() >= 2 {
        let name = &ep.arg_names[1];
        if !name.is_empty() && name != "input" && name != "criterion" {
            map.insert(name.clone(), serde_json::Value::String(criterion.to_string()));
        }
    }
    serde_json::Value::Object(map)
}

/// Criterion for a 1-argument template. A template with no `{{arg}}` is
/// the criterion itself, so the mock's substring check still works. A
/// template that embeds the argument is rendered before the call.
pub fn derive_criterion(ep: &Endpoint, input: &str) -> String {
    let prompt = ep.user_prompt.trim();
    if prompt.is_empty() || prompt.starts_with('\u{1e}') {
        return String::new();
    }
    if !prompt.contains("{{") {
        return prompt.to_string();
    }
    let mut rendered = prompt.replace("{{input}}", input);
    if let Some(name) = ep.arg_names.first() {
        rendered = rendered.replace(&format!("{{{{{name}}}}}"), input);
    }
    rendered
}

pub fn user_memo_key(oid: u32) -> String {
    format!("u\u{1e}{oid}")
}

pub struct UserReply {
    pub values: Vec<String>,
    pub http_calls: u32,
}

/// One batch for a `rvbbit_user` function. Bool and score are predict.
/// Labels are classify. Text is extract. `AS DECISION TREE` walks the tree.
pub fn user_eval_many(oid: u32, rows: &[Vec<String>]) -> PgResult<UserReply> {
    if rows.is_empty() {
        return Ok(UserReply { values: Vec::new(), http_calls: 0 });
    }
    let ep = rvbbit_resolve::endpoint_for(oid);
    if let Some(name) = ep.user_prompt.strip_prefix(rvbbit_resolve::TREE_PREFIX) {
        let tree = rvbbit_resolve::tree(name).ok_or_else(|| {
            pg(format!("decision tree \"{name}\" does not exist"))
        })?;
        let inputs: Vec<String> = rows.iter().map(|row| row.join("\n")).collect();
        let reply = decide_texts(&tree.endpoint, &tree.nodes, &inputs)?;
        return Ok(UserReply { values: reply.values, http_calls: reply.http_calls });
    }
    if ep.labels.len() >= 2 {
        return classify_user(&ep, rows);
    }
    let ret = ep.return_type.to_ascii_lowercase();
    if ret == "text" {
        return extract_user(&ep, rows);
    }
    if ret.contains("double") || ret == "float8" || ret == "float" {
        return score_user(&ep, rows);
    }
    predict_user(oid, &ep, rows)
}

fn row_input(row: &[String]) -> String {
    row.join("\n")
}

fn named_row(ep: &Endpoint, row: &[String]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("input".into(), serde_json::Value::String(row_input(row)));
    for (i, name) in ep.arg_names.iter().enumerate() {
        if name.is_empty() {
            continue;
        }
        if let Some(value) = row.get(i) {
            map.insert(name.clone(), serde_json::Value::String(value.clone()));
        }
    }
    serde_json::Value::Object(map)
}

fn predict_user(oid: u32, ep: &Endpoint, rows: &[Vec<String>]) -> PgResult<UserReply> {
    let pairs: Vec<(String, String)> = rows
        .iter()
        .map(|row| {
            let input = row.first().cloned().unwrap_or_default();
            let criterion = if row.len() >= 2 {
                row[1].clone()
            } else {
                derive_criterion(ep, &input)
            };
            (input, criterion)
        })
        .collect();
    let ticket = submit(oid, &pairs)?;
    let got = wait(ticket)?;
    let values = got.matched.into_iter().map(|bit| bit.to_string()).collect();
    Ok(UserReply { values, http_calls: got.http_calls })
}

fn score_user(ep: &Endpoint, rows: &[Vec<String>]) -> PgResult<UserReply> {
    let inputs: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let input = row.first().cloned().unwrap_or_default();
            let criterion = if row.len() >= 2 {
                row[1].clone()
            } else {
                derive_criterion(ep, &input)
            };
            predict_inputs(ep, &input, &criterion)
        })
        .collect();
    let body = encode_items(ep, Operation::Predict, &inputs).map_err(pg)?;
    let raw = post_op(ep, "predict", body)?;
    let scores = decode_scores(&raw.body).map_err(pg)?;
    Ok(UserReply {
        values: scores.into_iter().map(|n| n.to_string()).collect(),
        http_calls: raw.http_calls,
    })
}

fn extract_user(ep: &Endpoint, rows: &[Vec<String>]) -> PgResult<UserReply> {
    let inputs: Vec<serde_json::Value> = rows.iter().map(|row| named_row(ep, row)).collect();
    let (values, http_calls) = post_chunked_extract(ep, &inputs)?;
    if values.len() != rows.len() {
        return Err(pg(format!(
            "hutch extract returned {} items for {} inputs",
            values.len(),
            rows.len()
        )));
    }
    let mut out = Vec::with_capacity(values.len());
    for item in values {
        out.push(item.map_err(pg)?);
    }
    Ok(UserReply { values: out, http_calls })
}

fn classify_user(ep: &Endpoint, rows: &[Vec<String>]) -> PgResult<UserReply> {
    let inputs: Vec<serde_json::Value> = rows.iter().map(|row| named_row(ep, row)).collect();
    let body = encode_items(ep, Operation::Classify, &inputs).map_err(pg)?;
    let raw = post_op(ep, "classify", body)?;
    let parsed = parsed_batch(&raw.body).map_err(pg)?;
    let mut values = Vec::with_capacity(parsed.outputs.len());
    for (i, item) in parsed.outputs.iter().enumerate() {
        if let Some(err) = &item.error {
            return Err(pg(format!("hutch item {i}: {} {}", err.code, err.message)));
        }
        let label = item
            .value
            .as_ref()
            .and_then(|v| v.as_str())
            .ok_or_else(|| pg(format!("hutch classify item {i} has no label")))?;
        values.push(
            serde_json::json!({
                "label": label,
                "score": item.score,
                "scores": item.scores,
            })
            .to_string(),
        );
    }
    Ok(UserReply { values, http_calls: raw.http_calls })
}

fn decode_scores(text: &str) -> Result<Vec<f64>, String> {
    let parsed = parsed_batch(text)?;
    let mut out = Vec::with_capacity(parsed.outputs.len());
    for (i, item) in parsed.outputs.iter().enumerate() {
        if let Some(err) = &item.error {
            return Err(format!("hutch item {i}: {} {}", err.code, err.message));
        }
        let score = item.score.ok_or_else(|| format!("hutch item {i} has no score"))?;
        out.push(score);
    }
    Ok(out)
}

fn encode_items(ep: &Endpoint, operation: Operation, inputs: &[serde_json::Value]) -> Result<String, String> {
    let system = if ep.system_prompt.is_empty() || ep.system_prompt.starts_with('\u{1e}') {
        None
    } else {
        Some(ep.system_prompt.clone())
    };
    let defaults = ep
        .default_clauses
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| clauses::parse_tail(s).ok());
    let prompt = if let Some(first) = defaults.as_ref().and_then(|c| c.phrasings.first()) {
        Some(first.clone())
    } else if ep.user_prompt.is_empty() || ep.user_prompt.starts_with('\u{1e}') {
        None
    } else {
        Some(ep.user_prompt.clone())
    };
    let opts = cache_opts(ep);
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
            // EXTRACT MANY chats are seeded. The key has to change with that
            // seed, or a temperature-0 reply from an unseeded call is reused.
            seed: extract_cache_seed(ep.extract_mode.as_deref()),
            opts: &opts,
            threshold: ep.threshold,
            extract_mode: ep.extract_mode.as_deref().unwrap_or(""),
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
            response_schema: response_schema(ep),
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
fn cache_opts(ep: &Endpoint) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if !ep.labels.is_empty() {
        map.insert("labels".into(), serde_json::json!(ep.labels));
    }
    if ep.label_descriptions.iter().any(|d| !d.is_empty()) {
        map.insert(
            "label_descriptions".into(),
            serde_json::json!(ep.label_descriptions),
        );
    }
    serde_json::Value::Object(map)
}

fn noul_stack(ep: &Endpoint, operation: Operation) -> Option<DecisionStack> {
    let parsed = ep
        .default_clauses
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| clauses::parse_tail(s).ok())
        .unwrap_or_default();
    let threshold = parsed.threshold.or(ep.threshold);
    let takes = parsed.takes;
    let phrasings = parsed.phrasings.clone();
    let models = parsed.across.clone();
    let combine = match parsed.pick {
        Some(clauses::Pick::Vote) => Some("vote".to_string()),
        Some(_) => Some("mean".to_string()),
        None => None,
    };
    let decision_op = matches!(operation, Operation::Predict | Operation::Decide);
    let has_tail = takes.is_some()
        || !phrasings.is_empty()
        || !models.is_empty()
        || parsed.threshold.is_some();
    if decision_op {
        if threshold.is_none() && !has_tail {
            return None;
        }
    } else if !has_tail {
        return None;
    }
    Some(DecisionStack {
        models,
        takes,
        combine,
        threshold: if decision_op || parsed.threshold.is_some() { threshold } else { None },
        phrasings,
        pick: None,
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

/// `GET {base}/v1/calls?limit={CALL_LOG_CAP}`. The `rvbbit.calls` view
/// reads this. It does not use the dispatch pool, so a stats read cannot
/// wait behind a predict batch. The same cap bounds the hutch ledger.
pub fn recent_calls_json(base_url: &str) -> Result<String, String> {
    let url = format!(
        "{}/v1/calls?limit={}",
        base_url.trim_end_matches('/'),
        rvbbit_contract::CALL_LOG_CAP,
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
            "identical": meter.identical,
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
    if job.audit {
        req = req.set("X-Rvbbit-Audit", "payloads");
    }
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
            req.takes,
            &req.phrasings,
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
            req.takes,
            &req.phrasings,
        )?;
        let raw = post_tree(self.ep, "classify", body)?;
        self.http_calls = self.http_calls.saturating_add(raw.http_calls);
        decode_choices(&raw.body)
    }
}

fn post_tree(ep: &Endpoint, op: &str, body: String) -> Result<RawReply, String> {
    let auth = bearer(ep)?;
    let ticket = send(
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
    takes: Option<u32>,
    phrasings: &[String],
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
        let mut opts = cache_opts(&schema_ep);
        if !models.is_empty() {
            opts["models"] = serde_json::json!(models);
        }
        if !combine.is_empty() {
            opts["combine"] = serde_json::json!(combine);
        }
        if let Some(n) = takes {
            opts["takes"] = serde_json::json!(n);
        }
        if !phrasings.is_empty() {
            opts["phrasings"] = serde_json::json!(phrasings);
        }
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
            opts: &opts,
            threshold,
            extract_mode: "",
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
            response_schema: None,
        });
    }
    let guc = clauses::guc_clauses();
    let takes = takes.or(guc.takes);
    let stacked = !models.is_empty()
        || combine == "vote"
        || threshold.is_some_and(|t| (t - 0.5).abs() > 1e-9)
        || takes.is_some_and(|n| n > 1)
        || !phrasings.is_empty();
    let req = BatchRequest {
        contract: CONTRACT_VERSION.to_string(),
        operation,
        model: ep.model.clone(),
        temperature: 0.0,
        max_tokens: None,
        items: req_items,
        decision: stacked.then(|| DecisionStack {
            models: models.to_vec(),
            takes,
            combine: (combine == "vote").then(|| "vote".to_string()),
            threshold,
            phrasings: phrasings.to_vec(),
            pick: None,
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

/// Clause tail. One hutch batch is a wave: takes, models, and phrasings
/// fan out inside that batch. `RETRY` sends another wave. `FALLBACK`
/// redispatches once. `BUDGET` aborts when this session's recorded USD
/// would pass the cap. The text is a JSON report.
pub fn apply_clauses_text(kind: &str, input: &str, extra: &str, tail: &str) -> PgResult<String> {
    let call = clauses::parse_tail(tail).map_err(pg)?;
    let call_model = call.model.clone();
    let ep = decision_endpoint();
    let function = ep
        .default_clauses
        .as_deref()
        .map(|s| clauses::parse_tail(s).unwrap_or_default())
        .unwrap_or_default();
    let mut merged = clauses::merge(call, &function, &clauses::guc_clauses());
    if kind == "rerank" {
        // `semantic.model` is the decision slug. RERANK uses its own GUC
        // unless the call wrote USING MODEL.
        merged.model = call_model.or_else(clauses::guc_rerank_model);
    }
    if let Some(cond) = &merged.check_input {
        let ok = match cond {
            clauses::Cond::LenGte(n) => input.chars().count() >= *n,
            clauses::Cond::NonEmpty => !input.is_empty(),
            clauses::Cond::ScoreGte(_) | clauses::Cond::ScoreLt(_) => true,
        };
        if !ok {
            return Err(pg("CHECK INPUT failed"));
        }
    }
    let mut retries = 0u32;
    let limit = merged.retry.unwrap_or(0);
    let mut fallback_used = false;
    loop {
        let model = if fallback_used { merged.fallback.clone() } else { merged.model.clone() };
        let hit = post_clause(&ep, kind, input, extra, &merged, model.as_deref())?;
        if let Some(cost) = hit.cost {
            clauses::add_spent(cost);
        }
        if let Some(budget) = merged.budget {
            if clauses::spent() > budget + 1e-12 {
                return Err(pg(format!(
                    "semantic budget exceeded ({:.6} > {budget})",
                    clauses::spent()
                )));
            }
        }
        if let Some(message) = hit.error {
            if merged.fallback.is_some() && !fallback_used {
                fallback_used = true;
                continue;
            }
            return Err(pg(message));
        }
        let score = hit.score.unwrap_or(0.0);
        let output_ok = merged.check_output.as_ref().is_none_or(|cond| match cond {
            clauses::Cond::ScoreGte(n) => score >= *n,
            clauses::Cond::ScoreLt(n) => score < *n,
            clauses::Cond::NonEmpty => !hit.value.is_empty(),
            clauses::Cond::LenGte(n) => hit.value.len() >= *n,
        });
        let until_ok = merged.until.as_ref().is_none_or(|cond| match cond {
            clauses::Cond::ScoreGte(n) => score >= *n,
            clauses::Cond::ScoreLt(n) => score < *n,
            clauses::Cond::NonEmpty => !hit.value.is_empty(),
            clauses::Cond::LenGte(_) => true,
        });
        let ok = output_ok && until_ok;
        if ok || retries >= limit {
            if !ok {
                return Err(pg("RETRY exhausted"));
            }
            let report = serde_json::json!({
                "winner": hit.value,
                "score": hit.score,
                "retries": retries,
                "identical": hit.identical,
                "fallback": fallback_used,
            });
            return Ok(report.to_string());
        }
        retries += 1;
    }
}

struct ClauseHit {
    value: String,
    score: Option<f64>,
    cost: Option<f64>,
    identical: Option<bool>,
    error: Option<String>,
}

/// `summary_endpoint` and `many_endpoint` replace the endpoint after the
/// clause knobs were copied. Put temperature, tokens, cache, and model back.
fn reapply_clause_knobs(ep: &mut Endpoint, merged: &clauses::Clauses, model: Option<&str>) {
    if let Some(model) = model {
        if !model.is_empty() && model != "default" {
            ep.model = model.to_string();
        }
    }
    if let Some(t) = merged.temperature {
        ep.temperature = t;
    }
    if let Some(n) = merged.max_tokens {
        ep.max_tokens = Some(n);
    }
    if let Some(t) = merged.threshold {
        ep.threshold = Some(t);
    }
    if let Some(cache) = merged.cache.as_deref() {
        ep.cache_mode = match cache {
            "off" => "off".into(),
            "refresh" => "refresh".into(),
            _ => "memoize".into(),
        };
    }
}

fn post_clause(
    ep: &Endpoint,
    kind: &str,
    input: &str,
    extra: &str,
    merged: &clauses::Clauses,
    model: Option<&str>,
) -> PgResult<ClauseHit> {
    let mut ep = ep.clone();
    if let Some(model) = model {
        if !model.is_empty() && model != "default" {
            ep.model = model.to_string();
        }
    }
    if let Some(t) = merged.temperature {
        ep.temperature = t;
    }
    if let Some(n) = merged.max_tokens {
        ep.max_tokens = Some(n);
    }
    if let Some(t) = merged.threshold {
        ep.threshold = Some(t);
    }
    if let Some(cache) = merged.cache.as_deref() {
        ep.cache_mode = match cache {
            "off" => "off".into(),
            "refresh" => "refresh".into(),
            _ => "memoize".into(),
        };
    }
    let (op, operation) = match kind {
        "classify" => ("classify", Operation::Classify),
        "extract" | "summarize" | "extract_many" => ("extract", Operation::Extract),
        "rerank" => ("rerank", Operation::Rerank),
        _ => ("predict", Operation::Predict),
    };
    if kind == "summarize" {
        ep = summary_endpoint();
        reapply_clause_knobs(&mut ep, merged, model);
    } else if kind == "extract_many" {
        ep = many_endpoint(extra);
        reapply_clause_knobs(&mut ep, merged, model);
    } else if kind == "rerank" {
        ep.fn_name = "rerank".into();
        ep.system_prompt = "Is this document relevant to the query?".into();
        ep.user_prompt.clear();
        if ep.model.is_empty() {
            ep.model = "mock".into();
        }
    }
    if kind == "classify" {
        let labels: Vec<String> = extra.split('\u{1f}').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
        if labels.len() >= 2 {
            ep.labels = labels;
        }
    }
    let inputs = match kind {
        "classify" | "summarize" | "extract_many" => serde_json::json!({ "input": input }),
        "extract" => serde_json::json!({ "input": input }),
        _ => serde_json::json!({ "input": input, "criterion": extra }),
    };
    if kind == "extract" {
        ep = typed_extract_endpoint(extra);
        if let Some(t) = merged.temperature {
            ep.temperature = t;
        }
    }
    let body = encode_items(&ep, operation, &[inputs]).map_err(pg)?;
    let mut req: BatchRequest = serde_json::from_str(&body).map_err(|e| pg(e.to_string()))?;
    let models = if merged.across.is_empty() {
        if ep.model.is_empty() { Vec::new() } else { vec![ep.model.clone()] }
    } else {
        merged.across.clone()
    };
    let combine = match merged.pick {
        Some(clauses::Pick::Vote) => Some("vote".to_string()),
        _ => merged.pick.map(|_| "mean".to_string()),
    };
    if merged.takes.is_some() || !models.is_empty() || !merged.phrasings.is_empty() || combine.is_some() {
        req.decision = Some(DecisionStack {
            models,
            takes: merged.takes,
            combine,
            threshold: merged.threshold.or(ep.threshold),
            phrasings: merged.phrasings.clone(),
            pick: None,
        });
    }
    if !merged.phrasings.is_empty() {
        for item in &mut req.items {
            item.prompt = merged.phrasings.first().cloned();
        }
    }
    let body = serde_json::to_string(&req).map_err(|e| pg(e.to_string()))?;
    let raw = post_op(&ep, op, body)?;
    decode_clause(&raw.body).map_err(pg)
}

fn decode_clause(text: &str) -> Result<ClauseHit, String> {
    let parsed = parsed_batch(text)?;
    let item = parsed.outputs.first().ok_or("hutch returned no items")?;
    if let Some(err) = &item.error {
        return Ok(ClauseHit {
            value: String::new(),
            score: None,
            cost: None,
            identical: None,
            error: Some(format!("{} {}", err.code, err.message)),
        });
    }
    let value = match &item.value {
        Some(serde_json::Value::Bool(v)) => v.to_string(),
        Some(serde_json::Value::String(v)) => v.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let mut cost = 0.0;
    let mut saw = false;
    if let Some(calls) = &item.calls {
        for call in calls {
            if let Some(usd) = call.cost_usd {
                cost += usd;
                saw = true;
            }
        }
    }
    Ok(ClauseHit {
        value,
        score: item.score,
        cost: saw.then_some(cost),
        identical: item.identical_takes,
        error: None,
    })
}

/// A value the project batcher already computed, or the item error the
/// builtin turns into `ON ERROR`.
#[derive(Clone, Debug)]
pub enum ScalarHit {
    Value(String),
    /// Scalar EXTRACT found nothing. SQL NULL, not `ON ERROR`.
    Missing,
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
    fn comma_labels_trim_and_unit_separator_does_not() {
        assert_eq!(
            super::split_classify_labels("biped, invoice"),
            vec!["biped".to_string(), "invoice".to_string()]
        );
        assert_eq!(
            super::split_classify_labels("bigfoot\u{1f}known animal\u{1f}unclear"),
            vec!["bigfoot".to_string(), "known animal".to_string(), "unclear".to_string()]
        );
    }

    #[test]
    fn coerce_extract_kinds() {
        assert_eq!(coerce_extracted("int", " 12 ").unwrap(), "12");
        assert!(coerce_extracted("int", "nope").is_err());
        assert_eq!(coerce_extracted("bool", "YES").unwrap(), "true");
        assert_eq!(coerce_extracted("creature", "  sasquatch ").unwrap(), "sasquatch");
        assert!(coerce_extracted("float", "inf").is_err());
    }

    #[test]
    fn agg_state_appends_parts_and_keeps_the_first_clause() {
        let once = super::push_agg("", "alpha", "a large biped", "WITH TAKES 2");
        let twice = super::push_agg(&once, "beta", "ignored", "");
        let (clauses, extra, parts) = super::read_agg(&twice);
        assert_eq!(clauses, "WITH TAKES 2");
        assert_eq!(extra, "a large biped");
        assert_eq!(parts, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn extract_many_asks_only_for_the_requested_kind() {
        let ep = super::many_endpoint("creature");
        assert_eq!(ep.extract_mode.as_deref(), Some("many"));
        assert_eq!(ep.temperature, 0.0);
        assert_eq!(ep.max_tokens, Some(256));
        assert!(ep.system_prompt.contains("Bigfoot"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("gray"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("sasquatch"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("do not copy"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("Copy only words from the user text"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("not a creature"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("mentions is []"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("specific_instance_of_kind"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("attribute_or_part"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("sound_or_event"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("\"black dog\", not \"dog\""), "{}", ep.system_prompt);
        assert!(!ep.system_prompt.contains("descriptive word or short phrase"), "{}", ep.system_prompt);
        assert!(!ep.system_prompt.contains("does not include the aircraft"), "{}", ep.system_prompt);
        assert!(!ep.system_prompt.contains("proper is true"), "{}", ep.system_prompt);
        assert!(ep.user_prompt.contains("whole noun phrase"));
        assert!(ep.user_prompt.contains("specific_instance_of_kind"));
        assert!(ep.user_prompt.contains("not a creature"));
        assert!(ep.user_prompt.contains("mentions is []"));
        let schema: serde_json::Value = serde_json::from_str(&ep.return_type).unwrap();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["mentions"]["type"], "array");
        let item = &schema["properties"]["mentions"]["items"];
        assert_eq!(item["type"], "object");
        assert_eq!(item["additionalProperties"], false);
        assert_eq!(item["required"][0], "mention");
        assert_eq!(item["required"][1], "category");
        assert_eq!(item["properties"]["mention"]["type"], "string");
        assert_eq!(item["properties"]["category"]["type"], "string");
        let categories: Vec<&str> = item["properties"]["category"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            categories,
            vec![
                "specific_instance_of_kind",
                "generic_kind_word",
                "attribute_or_part",
                "place",
                "sound_or_event",
                "other",
            ]
        );
        assert!(schema["properties"]["mentions"]["description"]
            .as_str()
            .unwrap()
            .contains("creature"));
        let color = super::many_endpoint("color");
        assert!(color.system_prompt.contains("Large gray triangular aircraft"));
        assert!(color.system_prompt.contains("not a color"));
        assert_eq!(
            super::filter_mentions("color", "[\"gray\",\"night\",\"bright\",\"aircraft\"]"),
            "[\"gray\"]"
        );
        assert!(super::parse_mentions("not json").is_none());
        assert!(!color.system_prompt.contains("or adjectives"));
        let body = encode_items(
            &ep,
            Operation::Extract,
            &[
                serde_json::json!({ "input": "a tall hairy figure" }),
                serde_json::json!({ "input": "lights over the lake" }),
            ],
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["temperature"].as_f64(), Some(0.0));
        assert_eq!(parsed["items"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["items"][0]["extract_mode"], "many");
        assert_eq!(parsed["items"][0]["response_schema"]["type"], "object");
        assert_eq!(
            parsed["items"][0]["response_schema"]["properties"]["mentions"]["items"]["type"],
            "object"
        );
        let system = parsed["items"][0]["system"].as_str().unwrap();
        assert!(system.contains("Bigfoot"));
        assert!(system.contains("specific_instance_of_kind"));
        assert!(system.contains("not a creature"));
        assert!(system.contains("mentions is []"));
        let item_schema = &parsed["items"][0]["response_schema"]["properties"]["mentions"]["items"];
        assert_eq!(item_schema["properties"]["category"]["enum"][0], "specific_instance_of_kind");
        assert_ne!(
            super::scalar_key_extract("creature"),
            super::scalar_key_extract_many("creature")
        );
        assert!(super::scalar_key_extract_many("creature").starts_with("m\u{1e}"));
    }

    #[test]
    fn typed_extract_asks_for_a_short_noun() {
        let ep = typed_extract_endpoint("person");
        assert_eq!(ep.extract_mode.as_deref(), Some("scalar"));
        assert_eq!(ep.temperature, 0.0);
        assert!(ep.system_prompt.contains("at most three words"));
        assert!(ep.system_prompt.contains("found to false"));
        assert!(ep.system_prompt.contains("No person named"));
        assert!(ep.system_prompt.contains("not a person"));
        let body = encode_items(
            &ep,
            Operation::Extract,
            &[serde_json::json!({ "input": "a tall hairy figure" })],
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["temperature"].as_f64(), Some(0.0));
        assert_eq!(parsed["items"][0]["extract_mode"], "scalar");
        assert_eq!(parsed["items"][0]["response_schema"]["required"][0], "found");
        assert_eq!(parsed["items"][0]["response_schema"]["required"][1], "value");
        assert!(body.contains("What person does this text name?"));
        assert_eq!(super::extract_cache_seed(Some("scalar")), "0");
        assert_eq!(super::extract_cache_seed(Some("many")), "0");
        assert_eq!(super::extract_cache_seed(Some("summary")), "");
        assert_eq!(super::scrub_scalar_value("person", "None"), super::EXTRACT_ABSENT);
        assert_eq!(super::scrub_scalar_value("person", "No person named"), super::EXTRACT_ABSENT);
        assert_eq!(super::scrub_scalar_value("person", "Unknown person"), super::EXTRACT_ABSENT);
        assert_eq!(
            super::scrub_scalar_value("person", "No name provided."),
            super::EXTRACT_ABSENT
        );
        assert_eq!(super::scrub_scalar_value("person", "person"), super::EXTRACT_ABSENT);
        assert_eq!(super::scrub_scalar_value("person", "Ken Lay"), "Ken Lay");
        assert_eq!(super::scrub_scalar_value("person", "Gas Basis"), "Gas Basis");
    }

    #[test]
    fn extract_many_keeps_specific_instances_and_whole_phrases() {
        let listed = r#"[
            {"mention":"Bigfoot","category":"specific_instance_of_kind"},
            {"mention":"animal","category":"generic_kind_word"},
            {"mention":"black dog","category":"specific_instance_of_kind"},
            {"mention":"dog","category":"specific_instance_of_kind"},
            {"mention":"deer","category":"specific_instance_of_kind"},
            {"mention":"buck","category":"specific_instance_of_kind"},
            {"mention":"big buck","category":"specific_instance_of_kind"},
            {"mention":"hunting dog","category":"specific_instance_of_kind"},
            {"mention":"bird dog","category":"specific_instance_of_kind"},
            {"mention":"sasquatch","category":"specific_instance_of_kind"},
            {"mention":"night","category":"specific_instance_of_kind"},
            {"mention":"bright","category":"other"},
            {"mention":"man","category":"specific_instance_of_kind"},
            {"mention":"shellfish","category":"specific_instance_of_kind"},
            {"mention":"sea life","category":"specific_instance_of_kind"},
            {"mention":"human like figure","category":"specific_instance_of_kind"},
            {"mention":"growl type high pitch yell","category":"sound_or_event"},
            {"mention":"two legs","category":"attribute_or_part"},
            {"mention":"hair","category":"attribute_or_part"},
            {"mention":"Alabama","category":"place"},
            {"mention":"Florida","category":"place"},
            {"mention":"Deer","category":"specific_instance_of_kind"}
        ]"#;
        assert_eq!(
            super::filter_mentions("creature", listed),
            "[\"Bigfoot\",\"black dog\",\"deer\",\"big buck\",\"hunting dog\",\"bird dog\",\"sasquatch\"]"
        );
        // A schema fallback is a string list. Stoplist and the phrase rule still run.
        assert_eq!(
            super::filter_mentions("creature", "[\"Bigfoot\",\"animal\",\"dog\",\"black dog\",\"Deer\",\"deer\"]"),
            "[\"Bigfoot\",\"black dog\",\"Deer\"]"
        );
        // A head noun alone is kept. The recall script accepts it for "black dog".
        assert_eq!(super::filter_mentions("creature", "[\"dog\"]"), "[\"dog\"]");
        assert_eq!(
            super::filter_mentions(
                "color",
                r#"[
                    {"mention":"gray","category":"specific_instance_of_kind"},
                    {"mention":"night","category":"specific_instance_of_kind"},
                    {"mention":"bright","category":"other"},
                    {"mention":"aircraft","category":"specific_instance_of_kind"},
                    {"mention":"large gray","category":"specific_instance_of_kind"}
                ]"#
            ),
            "[\"gray\"]"
        );
        let body = encode_items(
            &super::summary_endpoint(),
            Operation::Extract,
            &[
                serde_json::json!({ "input": "one" }),
                serde_json::json!({ "input": "two" }),
                serde_json::json!({ "input": "three" }),
            ],
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["items"].as_array().unwrap().len(), 3);
        assert_eq!(parsed["items"][0]["extract_mode"], "summary");
        assert_eq!(parsed["items"][2]["inputs"]["input"], "three");
    }

    #[test]
    fn extract_chunks_match_the_batch_width() {
        assert_eq!(super::extract_chunk_sizes(70, 32), vec![32, 32, 6]);
        assert_eq!(super::extract_chunk_sizes(32, 32), vec![32]);
        assert_eq!(super::extract_chunk_sizes(0, 32), Vec::<usize>::new());
        assert_eq!(super::extract_chunk_sizes(5, 0), vec![1, 1, 1, 1, 1]);
        let items: Vec<u8> = (0..70).collect();
        let got: Vec<usize> = items.chunks(32).map(|c| c.len()).collect();
        assert_eq!(got, super::extract_chunk_sizes(70, 32));
    }

    #[test]
    fn hutch_timeout_parses_seconds() {
        assert_eq!(super::parse_hutch_timeout_ms(""), 120_000);
        assert_eq!(super::parse_hutch_timeout_ms("120"), 120_000);
        assert_eq!(super::parse_hutch_timeout_ms("120s"), 120_000);
        assert_eq!(super::parse_hutch_timeout_ms("30"), 30_000);
        assert_eq!(super::parse_hutch_timeout_ms("nope"), 120_000);
        assert_eq!(super::hutch_timeout_ms(), 120_000);
    }

    #[test]
    fn user_template_names_the_argument_and_folds_default_takes() {
        let mut ep = rvbbit_resolve::default_endpoint();
        ep.arg_names = vec!["notes".into()];
        ep.user_prompt = "Does {{notes}} mention a biped?".into();
        ep.default_clauses = Some("WITH TAKES 1".into());
        ep.user_fn = true;
        let input = "a large hairy biped";
        let criterion = super::derive_criterion(&ep, input);
        assert!(criterion.contains("a large hairy biped"));
        assert!(criterion.contains("biped"));
        let inputs = super::predict_inputs(&ep, input, &criterion);
        assert_eq!(inputs["notes"], input);
        let body = encode_items(&ep, Operation::Predict, &[inputs]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["decision"]["takes"], 1);
        assert_eq!(parsed["items"][0]["inputs"]["notes"], input);
        assert!(parsed["items"][0]["prompt"].as_str().unwrap().contains("{{notes}}"));
        let bare = {
            let mut ep = ep.clone();
            ep.user_prompt = "large hairy biped".into();
            super::derive_criterion(&ep, input)
        };
        assert_eq!(bare, "large hairy biped");
    }
}

fn pg(message: impl Into<String>) -> Box<PgError> {
    Box::new(PgError::error(message.into()))
}
