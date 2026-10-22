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
    Operation, CONTRACT_VERSION, HEADER_AUTHORIZATION, HEADER_CACHE, HEADER_CONTRACT,
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
    guc::MarkGUCPrefixReserved("semantic");
    let read = |name: &str| guc::GetConfigOption(name, true, false).ok().flatten();
    memo::set_guc_reader(read);
    clauses::set_guc_reader(read);
    rvbbit_cost::set_guc_reader(read);
    rvbbit_cost::set_history_refresher(pull_remote_history);
}

/// One short read of hutch `/stats` and `/v1/calls` so the next plan can
/// price from observed latency and USD. Later calls update the same table
/// as their responses arrive. Missing hutch is ignored.
fn pull_remote_history() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::Relaxed) {
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
        .timeout(Duration::from_millis(250))
        .build();
    let calls = match agent.get(&format!("{base}/v1/calls?limit=256")).call() {
        Ok(resp) => resp.into_string().map(|text| rvbbit_cost::ingest_calls_json(&text)).unwrap_or(0),
        Err(_) => 0,
    };
    if calls == 0 {
        if let Ok(resp) = agent.get(&format!("{base}/stats")).call() {
            if let Ok(text) = resp.into_string() {
                rvbbit_cost::ingest_stats_json(&text);
            }
        }
    }
}

/// Clear the per-query memo. Executor start calls this.
pub fn begin_query() {
    memo::begin_query();
    rvbbit_cost::begin_query();
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

/// One HTTP extract for every input. Each value is a JSON array of strings
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
    let body = encode_items(&ep, Operation::Extract, &payload).map_err(pg)?;
    let raw = post_op(&ep, "extract", body)?;
    let values = decode_text_items(&raw.body).map_err(pg)?;
    if values.len() != inputs.len() {
        return Err(pg(format!(
            "hutch extract many returned {} items for {} inputs",
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
/// Hutch unwraps `mentions` back into the array SQL explodes.
fn many_schema(kind: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["mentions"],
        "properties": {
            "mentions": {
                "type": "array",
                "description": format!(
                    "One string per {kind}. A proper noun, a short descriptive phrase, or a synonym. Empty when the text has none."
                ),
                "items": { "type": "string" }
            }
        }
    })
}

fn response_schema(ep: &Endpoint) -> Option<serde_json::Value> {
    if ep.extract_mode.as_deref() != Some("many") {
        return None;
    }
    serde_json::from_str(&ep.return_type).ok()
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
        "Extract every {kind} from the user text. \
         Reply with JSON only, matching this schema, and no markdown: {schema_text} \
         {} \
         Those examples are not the user text. Every mention must appear in the user text. Do not copy the examples. \
         Leave out anything that is not a {kind}. A color list does not include the aircraft, its size, or its shape. A creature list does not include the story or the road. \
         If the text has no {kind}, mentions is [].",
        many_examples(kind)
    );
    ep.user_prompt = format!(
        "List every {kind}. Proper nouns, descriptive phrases, and synonyms count. \
         Leave out anything that is not a {kind}. If there are none, mentions is [].\n{{{{input}}}}"
    );
    ep.max_tokens = Some(256);
    ep
}

/// The two live misses, plus a synonym. They stay in every kind's prompt
/// so a proper noun, a descriptive word, and a synonym are all shown.
fn many_examples(_kind: &str) -> String {
    "A proper noun counts. Asked for a creature, the text \"My story of Bigfoot\" yields Bigfoot. \
     A descriptive word or short phrase counts. Asked for a color, the text \"Large gray triangular aircraft\" yields gray. \
     A synonym counts even when the text never uses the kind word. Asked for a creature, \"a sasquatch crossed the road\" yields sasquatch."
        .to_string()
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
    let (clauses, _extra, parts) = read_agg(state);
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(summarize_text(&parts.join("\n\n"), &clauses)?))
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
    if clauses.trim().is_empty() {
        let ep = many_endpoint(kind);
        let body = encode_items(&ep, Operation::Extract, &[serde_json::json!({ "input": input })])
            .map_err(pg)?;
        let raw = post_op(&ep, "extract", body)?;
        let hit = decode_clause(&raw.body).map_err(pg)?;
        if let Some(err) = hit.error {
            return Err(pg(err));
        }
        return Ok(hit.value);
    }
    winner_of(&apply_clauses_text("extract_many", input, kind, clauses)?)
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
        rvbbit_cost::observe_response(&raw.body);
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
    rvbbit_cost::observe_response(&body);
    Ok(RawReply {
        body,
        http_calls: raw.http_calls,
    })
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
            seed: "",
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
    if !matches!(operation, Operation::Predict | Operation::Decide) {
        return None;
    }
    let threshold = ep.threshold?;
    Some(DecisionStack {
        models: Vec::new(),
        takes: None,
        combine: None,
        threshold: Some(threshold),
        phrasings: Vec::new(),
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
        assert!(ep.system_prompt.contains("proper noun"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("Bigfoot"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("descriptive word"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("gray"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("sasquatch"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("synonym"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("Do not copy the examples"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("must appear in the user text"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("not a creature"), "{}", ep.system_prompt);
        assert!(ep.system_prompt.contains("mentions is []"), "{}", ep.system_prompt);
        assert!(ep.user_prompt.contains("Proper nouns, descriptive phrases, and synonyms count"));
        assert!(ep.user_prompt.contains("not a creature"));
        assert!(ep.user_prompt.contains("mentions is []"));
        let schema: serde_json::Value = serde_json::from_str(&ep.return_type).unwrap();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["mentions"]["type"], "array");
        assert_eq!(schema["properties"]["mentions"]["items"]["type"], "string");
        assert!(schema["properties"]["mentions"]["description"]
            .as_str()
            .unwrap()
            .contains("creature"));
        let color = super::many_endpoint("color");
        assert!(color.system_prompt.contains("Large gray triangular aircraft"));
        assert!(color.system_prompt.contains("not a color"));
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
            "string"
        );
        let system = parsed["items"][0]["system"].as_str().unwrap();
        assert!(system.contains("Bigfoot"));
        assert!(system.contains("not a creature"));
        assert!(system.contains("mentions is []"));
        assert_ne!(
            super::scalar_key_extract("creature"),
            super::scalar_key_extract_many("creature")
        );
        assert!(super::scalar_key_extract_many("creature").starts_with("m\u{1e}"));
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
