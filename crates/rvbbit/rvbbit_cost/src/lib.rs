//! Provider cost and latency for semantic plans.
//!
//! Seed prices cover jev, gpt-4o-mini, and voyage rerank. A call observed
//! from hutch (the response `calls` array, `rvbbit.calls`, or `/stats`)
//! replaces the seed for that operation and model. Estimates fold in the
//! cache hit rate, `TAKES`, `PHRASINGS`, batching, and `FALLBACK`.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// Jev list price: about $0.042 per million input tokens.
pub const JEV_USD_PER_MILLION: f64 = 0.042;
/// Billed input tokens for one jev noul call.
pub const JEV_BILLED_TOKENS: f64 = 300.0;

const MINI_USD_PER_MILLION_IN: f64 = 0.15;
const MINI_USD_PER_MILLION_OUT: f64 = 0.60;
const MINI_TOKENS_IN: f64 = 300.0;
const MINI_TOKENS_OUT: f64 = 50.0;

/// Voyage rerank is priced per token, like the rerank-2 list rate.
const VOYAGE_USD_PER_MILLION: f64 = 0.05;
const VOYAGE_TOKENS_PER_DOC: f64 = 200.0;

const JEV_LATENCY_MS: f64 = 350.0;
const MINI_LATENCY_MS: f64 = 280.0;
const VOYAGE_LATENCY_MS: f64 = 120.0;

/// A configured `FALLBACK` is assumed to fire on a quarter of the rows.
const FALLBACK_EXTRA: f64 = 0.25;

const HISTORY_CAP: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Predict,
    Classify,
    Extract,
    Rerank,
    Summarize,
    Decide,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Predict => "predict",
            Op::Classify => "classify",
            Op::Extract => "extract",
            Op::Rerank => "rerank",
            Op::Summarize => "summarize",
            Op::Decide => "decide",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "predict" | "means" | "means_score" | "consensus" => Some(Op::Predict),
            "classify" | "classify_enum" => Some(Op::Classify),
            "extract" | "extract_typed" | "extract_many" => Some(Op::Extract),
            "rerank" | "rerank_for" => Some(Op::Rerank),
            "summarize" | "summary" => Some(Op::Summarize),
            "decide" => Some(Op::Decide),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grouping {
    /// Rows are packed into batches. One HTTP call carries `batch_rows` items.
    Batched,
    /// One provider call per group (aggregate final functions).
    PerGroup,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Knobs {
    pub takes: u32,
    pub phrasings: u32,
    pub across: u32,
    pub fallback: bool,
    pub cache_off: bool,
    pub budget: Option<f64>,
    pub threshold: Option<f64>,
    pub model: Option<String>,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            takes: 1,
            phrasings: 1,
            across: 1,
            fallback: false,
            cache_off: false,
            budget: None,
            threshold: None,
            model: None,
        }
    }
}

/// Clause tail knobs the cost model cares about. Unknown words are skipped.
pub fn knobs_from_tail(tail: &str) -> Knobs {
    let mut k = Knobs::default();
    let tokens = tokenize(tail);
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i].as_str() {
            "takes" => {
                if let Some(n) = tokens.get(i + 1).and_then(|s| s.parse::<f64>().ok()) {
                    k.takes = n.max(1.0) as u32;
                    i += 2;
                    continue;
                }
            }
            "phrasings" => {
                let (n, next) = count_list(&tokens, i + 1);
                if n > 0 {
                    k.phrasings = n;
                    i = next;
                    continue;
                }
            }
            "across" => {
                let start = if tokens.get(i + 1).map(|s| s.as_str()) == Some("models") {
                    i + 2
                } else {
                    i + 1
                };
                let (n, next) = count_list(&tokens, start);
                if n > 0 {
                    k.across = n;
                    i = next;
                    continue;
                }
            }
            "fallback" => {
                k.fallback = true;
                i += 1;
                if tokens.get(i).is_some_and(|s| !s.parse::<f64>().is_ok() && !is_keyword(s)) {
                    i += 1;
                }
                continue;
            }
            "budget" => {
                if let Some(n) = tokens.get(i + 1).and_then(|s| s.parse::<f64>().ok()) {
                    k.budget = Some(n);
                    i += 2;
                    continue;
                }
            }
            "threshold" => {
                if let Some(n) = tokens.get(i + 1).and_then(|s| s.parse::<f64>().ok()) {
                    k.threshold = Some(n);
                    i += 2;
                    continue;
                }
            }
            "cache" => {
                if tokens.get(i + 1).map(|s| s.as_str()) == Some("off") {
                    k.cache_off = true;
                    i += 2;
                    continue;
                }
            }
            "model" => {
                if let Some(slug) = tokens.get(i + 1) {
                    if !is_keyword(slug) && slug.parse::<f64>().is_err() {
                        k.model = Some(slug.clone());
                        i += 2;
                        continue;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    k
}

fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        "with" | "takes"
            | "phrasings"
            | "across"
            | "models"
            | "using"
            | "model"
            | "fallback"
            | "budget"
            | "threshold"
            | "cache"
            | "on"
            | "off"
            | "pick"
            | "by"
            | "retry"
            | "check"
            | "report"
            | "and"
    )
}

fn tokenize(tail: &str) -> Vec<String> {
    let mut out = Vec::new();
    let b = tail.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        if b[i] == b'\'' {
            let start = i + 1;
            i += 1;
            let mut s = String::new();
            while i < b.len() {
                if b[i] == b'\'' && b.get(i + 1) == Some(&b'\'') {
                    s.push('\'');
                    i += 2;
                    continue;
                }
                if b[i] == b'\'' {
                    i += 1;
                    break;
                }
                s.push(b[i] as char);
                i += 1;
            }
            let _ = start;
            out.push(s.to_ascii_lowercase());
            continue;
        }
        if b[i] == b',' {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b',' && b[i] != b'\'' {
            i += 1;
        }
        out.push(tail[start..i].to_ascii_lowercase());
    }
    out
}

fn count_list(tokens: &[String], mut i: usize) -> (u32, usize) {
    let mut n = 0u32;
    while i < tokens.len() && !is_keyword(&tokens[i]) {
        n += 1;
        i += 1;
    }
    (n, i)
}

#[derive(Clone, Debug)]
pub struct Work {
    pub op: Op,
    pub model: String,
    pub rows: f64,
    pub per_row: f64,
    pub knobs: Knobs,
    pub grouping: Grouping,
    pub batch_rows: u32,
    pub concurrency: u32,
    /// Predict filters use this. `None` is the default 0.5.
    pub threshold: Option<f64>,
    pub filters: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Estimate {
    pub calls: f64,
    pub usd: f64,
    pub latency_ms: f64,
    pub selectivity: f64,
    pub model: String,
    pub items: f64,
}

impl Estimate {
    pub fn zero() -> Self {
        Self {
            calls: 0.0,
            usd: 0.0,
            latency_ms: 0.0,
            selectivity: 1.0,
            model: String::new(),
            items: 0.0,
        }
    }

    pub fn add(&mut self, other: &Estimate) {
        self.calls += other.calls;
        self.usd += other.usd;
        self.latency_ms += other.latency_ms;
        self.items += other.items;
        if self.model.is_empty() {
            self.model = other.model.clone();
        } else if !other.model.is_empty() && !self.model.split(',').any(|m| m.trim() == other.model) {
            self.model.push_str(", ");
            self.model.push_str(&other.model);
        }
        if other.selectivity < self.selectivity {
            self.selectivity = other.selectivity;
        }
    }
}

/// THRESHOLD-aware filter selectivity. Unset is 0.5.
pub fn selectivity(threshold: Option<f64>) -> f64 {
    match threshold {
        None => 0.5,
        Some(t) => {
            let t = t.clamp(0.0, 1.0);
            (1.0 - t).clamp(0.05, 0.95)
        }
    }
}

struct Seed {
    usd_per_item: f64,
    latency_ms: f64,
    priced_as: &'static str,
}

fn seed_for(model: &str, op: Op) -> Seed {
    let m = model.trim().to_ascii_lowercase();
    if m.contains("gpt-4o-mini") || m.contains("4o-mini") {
        return Seed {
            usd_per_item: (MINI_TOKENS_IN * MINI_USD_PER_MILLION_IN
                + MINI_TOKENS_OUT * MINI_USD_PER_MILLION_OUT)
                / 1_000_000.0,
            latency_ms: MINI_LATENCY_MS,
            priced_as: "gpt-4o-mini",
        };
    }
    if op == Op::Rerank && (m.contains("voyage") || m.contains("rerank-3")) {
        return Seed {
            usd_per_item: VOYAGE_TOKENS_PER_DOC * VOYAGE_USD_PER_MILLION / 1_000_000.0,
            latency_ms: VOYAGE_LATENCY_MS,
            priced_as: "voyageai/rerank-3",
        };
    }
    Seed {
        usd_per_item: JEV_BILLED_TOKENS * JEV_USD_PER_MILLION / 1_000_000.0,
        latency_ms: JEV_LATENCY_MS,
        priced_as: "jev",
    }
}

#[derive(Clone, Debug)]
struct Sample {
    op: String,
    model: String,
    usd: f64,
    latency_ms: f64,
    hit: bool,
}

struct History {
    samples: Vec<Sample>,
}

impl History {
    fn note(&mut self, sample: Sample) {
        if self.samples.len() == HISTORY_CAP {
            self.samples.remove(0);
        }
        self.samples.push(sample);
    }

    fn matching<'a>(&'a self, op: &str, model: &str) -> Vec<&'a Sample> {
        let op = op.to_ascii_lowercase();
        let model = model.to_ascii_lowercase();
        self.samples
            .iter()
            .filter(|s| s.op.eq_ignore_ascii_case(&op) && s.model.eq_ignore_ascii_case(&model))
            .collect()
    }
}

static HISTORY: Mutex<History> = Mutex::new(History { samples: Vec::new() });

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn note_observation(op: &str, model: &str, usd: f64, latency_ms: f64, hit: bool) {
    lock(&HISTORY).note(Sample {
        op: op.to_string(),
        model: model.to_string(),
        usd,
        latency_ms,
        hit,
    });
}

pub fn reset_history() {
    lock(&HISTORY).samples.clear();
}

struct Learned {
    usd_per_item: Option<f64>,
    latency_ms: Option<f64>,
    hit_rate: Option<f64>,
}

fn learned(op: Op, model: &str) -> Learned {
    let history = lock(&HISTORY);
    let samples: Vec<Sample> = history
        .matching(op.as_str(), model)
        .into_iter()
        .cloned()
        .collect();
    drop(history);
    if samples.is_empty() {
        return Learned { usd_per_item: None, latency_ms: None, hit_rate: None };
    }
    let hits = samples.iter().filter(|s| s.hit).count();
    let misses: Vec<&Sample> = samples.iter().filter(|s| !s.hit).collect();
    // A stats row has latency but no USD. Zero does not replace the seed.
    let priced: Vec<&Sample> = misses.iter().copied().filter(|s| s.usd > 0.0).collect();
    let usd = if priced.is_empty() {
        None
    } else {
        Some(priced.iter().map(|s| s.usd).sum::<f64>() / priced.len() as f64)
    };
    let latency = if misses.is_empty() {
        None
    } else {
        Some(misses.iter().map(|s| s.latency_ms).sum::<f64>() / misses.len() as f64)
    };
    Learned {
        usd_per_item: usd,
        latency_ms: latency,
        hit_rate: Some(hits as f64 / samples.len() as f64),
    }
}

pub fn estimate(work: &Work) -> Estimate {
    let model = work
        .knobs
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| work.model.clone());
    let seed = seed_for(&model, work.op);
    let learned = learned(work.op, &model);
    let usd_per_item = learned.usd_per_item.unwrap_or(seed.usd_per_item);
    let latency_per = learned.latency_ms.unwrap_or(seed.latency_ms);
    let hit_rate = if work.knobs.cache_off {
        0.0
    } else {
        learned.hit_rate.unwrap_or(0.0).clamp(0.0, 0.95)
    };
    let takes = work.knobs.takes.max(1) as f64;
    let phrasings = work.knobs.phrasings.max(1) as f64;
    let across = work.knobs.across.max(1) as f64;
    let fallback = if work.knobs.fallback { 1.0 + FALLBACK_EXTRA } else { 1.0 };
    let rows = work.rows.max(0.0);
    let per_row = if work.per_row > 0.0 { work.per_row } else { 1.0 };
    let items = rows * per_row * takes * phrasings * across * fallback;
    let charged = items * (1.0 - hit_rate);
    let batch = work.batch_rows.max(1) as f64;
    let calls = match work.grouping {
        Grouping::Batched => {
            if charged <= 0.0 {
                0.0
            } else {
                (charged / batch).ceil()
            }
        }
        Grouping::PerGroup => charged,
    };
    let conc = work.concurrency.max(1) as f64;
    let waves = if calls <= 0.0 { 0.0 } else { (calls / conc).ceil() };
    let sel = if work.filters {
        selectivity(work.threshold.or(work.knobs.threshold))
    } else {
        1.0
    };
    let shown_model = if model.is_empty() || model.eq_ignore_ascii_case("mock") || model.eq_ignore_ascii_case("default")
    {
        seed.priced_as.to_string()
    } else if seed.priced_as == "voyageai/rerank-3" && !model.contains("voyage") {
        seed.priced_as.to_string()
    } else {
        model
    };
    Estimate {
        calls,
        usd: charged * usd_per_item,
        latency_ms: waves * latency_per,
        selectivity: sel,
        model: shown_model,
        items: charged,
    }
}

/// `/stats` payload: hit rate and p50 latency per operation and model.
/// USD stays on the seed until a call row arrives.
pub fn ingest_stats_json(text: &str) -> usize {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return 0;
    };
    let Some(rows) = v.get("latency_by_op_model").and_then(|r| r.as_array()) else {
        return 0;
    };
    let mut n = 0;
    for row in rows {
        let op = row.get("op").and_then(|s| s.as_str()).unwrap_or("");
        let model = row.get("model").and_then(|s| s.as_str()).unwrap_or("");
        let latency = row.get("p50_ms").and_then(|s| s.as_f64()).unwrap_or(0.0);
        let calls = row.get("calls").and_then(|s| s.as_u64()).unwrap_or(0);
        let hits = row.get("hits").and_then(|s| s.as_u64()).unwrap_or(0);
        for _ in 0..calls {
            note_observation(op, model, 0.0, latency, false);
            n += 1;
        }
        for _ in 0..hits {
            note_observation(op, model, 0.0, 0.0, true);
            n += 1;
        }
    }
    n
}

/// One hutch call-log array, or `{"calls":[...]}`.
pub fn ingest_calls_json(text: &str) -> usize {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return 0;
    };
    let rows = v
        .get("calls")
        .and_then(|c| c.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    let mut n = 0;
    for row in rows {
        let op = row.get("op").and_then(|s| s.as_str()).unwrap_or("");
        let model = row.get("model").and_then(|s| s.as_str()).unwrap_or("");
        if op.is_empty() {
            continue;
        }
        let usd = row
            .get("cost")
            .or_else(|| row.get("cost_usd"))
            .and_then(|s| s.as_f64())
            .unwrap_or(0.0);
        let latency = row.get("latency_ms").and_then(|s| s.as_f64()).unwrap_or(0.0);
        let cache = row.get("cache").and_then(|s| s.as_str()).unwrap_or("miss");
        note_observation(op, model, usd, latency, cache == "hit");
        n += 1;
    }
    n
}

#[derive(Clone, Debug, Default)]
pub struct Actual {
    pub calls: u64,
    pub cache_hits: u64,
    pub usd: f64,
    pub latency_ms: f64,
    pub model: String,
}

impl Actual {
    fn add_call(&mut self, usd: f64, latency_ms: f64, hit: bool, model: &str) {
        self.calls += 1;
        if hit {
            self.cache_hits += 1;
        }
        self.usd += usd;
        self.latency_ms += latency_ms;
        if self.model.is_empty() && !model.is_empty() {
            self.model = model.to_string();
        }
    }
}

struct QueryState {
    armed_usd: Option<f64>,
    clause_budget: Option<f64>,
    checked: bool,
    node: Option<i32>,
    by_node: HashMap<i32, Actual>,
    total: Actual,
}

impl QueryState {
    fn fresh_arm() -> Self {
        Self {
            armed_usd: None,
            clause_budget: None,
            checked: false,
            node: None,
            by_node: HashMap::new(),
            total: Actual::default(),
        }
    }
}

static QUERY: LazyLock<Mutex<QueryState>> = LazyLock::new(|| Mutex::new(QueryState::fresh_arm()));

fn query() -> std::sync::MutexGuard<'static, QueryState> {
    QUERY.lock().unwrap_or_else(|e| e.into_inner())
}

type GucRead = fn(&str) -> Option<String>;
static GUC: Mutex<Option<GucRead>> = Mutex::new(None);
static REFRESH: Mutex<Option<fn()>> = Mutex::new(None);

/// Planner startup installs this. It pulls `rvbbit.calls` / hutch `/stats`
/// once, when `RVBBIT_HUTCH_URL` is set, and then returns.
pub fn set_history_refresher(refresh: fn()) {
    *lock(&REFRESH) = Some(refresh);
}

pub fn refresh_history() {
    if let Some(refresh) = *lock(&REFRESH) {
        refresh();
    }
}

pub fn set_guc_reader(read: GucRead) {
    *lock(&GUC) = Some(read);
}

fn guc(name: &str) -> Option<String> {
    let read = (*lock(&GUC))?;
    let value = read(name)?;
    let value = value.trim();
    if value.is_empty() { None } else { Some(value.to_string()) }
}

pub fn arm_query(usd: f64, clause_budget: Option<f64>) {
    let mut q = query();
    q.armed_usd = Some(usd);
    q.clause_budget = clause_budget;
    q.checked = false;
}

pub fn disarm_query() {
    let mut q = query();
    q.armed_usd = None;
    q.clause_budget = None;
    q.checked = false;
}

std::thread_local! {
    static EXEC_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

pub fn execution_in_progress() -> bool {
    EXEC_DEPTH.with(|d| d.get() > 0)
}

/// New execution. Keeps the estimate armed at plan time. Nested SPI
/// executions share this thread's depth so their plans cannot replace that
/// estimate, and they do not wipe actuals the outer statement already recorded.
pub fn begin_query() {
    let prev = EXEC_DEPTH.with(|d| {
        let prev = d.get();
        d.set(prev.saturating_add(1));
        prev
    });
    if prev > 0 {
        return;
    }
    let mut q = query();
    q.checked = false;
    q.node = None;
    q.by_node.clear();
    q.total = Actual::default();
}

/// Executor end, including the abort path that never reaches ExecutorEnd.
/// The next statement's planner may arm a new estimate once this thread's
/// outermost execution has finished.
pub fn end_query() {
    EXEC_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
}

pub struct ChargeScope {
    prev: Option<i32>,
}

pub fn charge_scope(plan_node_id: i32) -> ChargeScope {
    let mut q = query();
    let prev = q.node.replace(plan_node_id);
    ChargeScope { prev }
}

impl Drop for ChargeScope {
    fn drop(&mut self) {
        query().node = self.prev;
    }
}

pub fn node_actual(plan_node_id: i32) -> Option<Actual> {
    query().by_node.get(&plan_node_id).cloned()
}

pub fn query_actual() -> Actual {
    query().total.clone()
}

/// Parse a hutch batch body and record its `calls` arrays.
pub fn observe_response(body: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return;
    };
    let Some(outputs) = v.get("outputs").and_then(|o| o.as_array()) else {
        return;
    };
    for item in outputs {
        let Some(calls) = item.get("calls").and_then(|c| c.as_array()) else {
            continue;
        };
        for call in calls {
            let op = call.get("op").and_then(|s| s.as_str()).unwrap_or("");
            let model = call.get("model").and_then(|s| s.as_str()).unwrap_or("");
            let usd = call.get("cost_usd").and_then(|s| s.as_f64()).unwrap_or(0.0);
            let latency = call
                .get("provider_latency_ms")
                .or_else(|| call.get("latency_ms"))
                .and_then(|s| s.as_f64())
                .unwrap_or(0.0);
            let cache = call.get("cache").and_then(|s| s.as_str()).unwrap_or("miss");
            let hit = cache == "hit";
            if !op.is_empty() {
                note_observation(op, model, usd, latency, hit);
            }
            let mut q = query();
            let node = q.node;
            q.total.add_call(usd, latency, hit, model);
            if let Some(id) = node {
                q.by_node.entry(id).or_default().add_call(usd, latency, hit, model);
            }
        }
    }
}

/// Error when the armed estimate is above `BUDGET` or `semantic.max_cost`.
/// `semantic.cost_check = off` skips the check. `semantic.budget` is a third cap.
/// A failure stays armed, so a later call in the same execution still errors.
pub fn preflight() -> Result<(), String> {
    let (usd, clause) = {
        let q = query();
        if q.checked {
            return Ok(());
        }
        (q.armed_usd.unwrap_or(0.0), q.clause_budget)
    };
    if guc("semantic.cost_check").is_some_and(|v| v.eq_ignore_ascii_case("off")) {
        query().checked = true;
        return Ok(());
    }
    let mut caps = Vec::new();
    if let Some(b) = clause {
        if b >= 0.0 {
            caps.push(b);
        }
    }
    if let Some(v) = guc("semantic.max_cost").and_then(|s| s.parse::<f64>().ok()) {
        if v >= 0.0 {
            caps.push(v);
        }
    }
    if let Some(v) = guc("semantic.budget").and_then(|s| s.parse::<f64>().ok()) {
        if v >= 0.0 {
            caps.push(v);
        }
    }
    let Some(cap) = caps.into_iter().reduce(f64::min) else {
        query().checked = true;
        return Ok(());
    };
    if usd > cap + 1e-12 {
        return Err(format!(
            "semantic estimate {usd:.6} USD exceeds budget {cap:.6}"
        ));
    }
    query().checked = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guc_off(_name: &str) -> Option<String> {
        None
    }

    #[test]
    fn jev_price_batches_and_multiplies_takes() {
        reset_history();
        let one = estimate(&Work {
            op: Op::Predict,
            model: "typesafe/jev-1.14".into(),
            rows: 32.0,
            per_row: 1.0,
            knobs: Knobs::default(),
            grouping: Grouping::Batched,
            batch_rows: 32,
            concurrency: 4,
            threshold: None,
            filters: true,
        });
        let per = JEV_BILLED_TOKENS * JEV_USD_PER_MILLION / 1_000_000.0;
        assert!((one.usd - 32.0 * per).abs() < 1e-12);
        assert_eq!(one.calls, 1.0);
        assert!((one.selectivity - 0.5).abs() < 1e-12);
        assert_eq!(one.model, "typesafe/jev-1.14");

        let mut knobs = Knobs::default();
        knobs.takes = 3;
        knobs.phrasings = 2;
        knobs.fallback = true;
        let many = estimate(&Work {
            op: Op::Predict,
            model: "jev".into(),
            rows: 32.0,
            per_row: 1.0,
            knobs,
            grouping: Grouping::Batched,
            batch_rows: 32,
            concurrency: 4,
            threshold: Some(0.8),
            filters: true,
        });
        let items = 32.0 * 3.0 * 2.0 * (1.0 + FALLBACK_EXTRA);
        assert!((many.items - items).abs() < 1e-9);
        assert_eq!(many.calls, (items / 32.0).ceil());
        assert!((many.selectivity - 0.2).abs() < 1e-12);
    }

    #[test]
    fn mini_and_voyage_seeds_and_history() {
        reset_history();
        let mini = estimate(&Work {
            op: Op::Predict,
            model: "openai/gpt-4o-mini".into(),
            rows: 1.0,
            per_row: 1.0,
            knobs: Knobs::default(),
            grouping: Grouping::Batched,
            batch_rows: 32,
            concurrency: 1,
            threshold: None,
            filters: false,
        });
        let expect = (300.0 * 0.15 + 50.0 * 0.60) / 1_000_000.0;
        assert!((mini.usd - expect).abs() < 1e-12);
        assert_eq!(mini.model, "openai/gpt-4o-mini");

        let voyage = estimate(&Work {
            op: Op::Rerank,
            model: "voyageai/rerank-3".into(),
            rows: 10.0,
            per_row: 1.0,
            knobs: Knobs::default(),
            grouping: Grouping::Batched,
            batch_rows: 32,
            concurrency: 1,
            threshold: None,
            filters: false,
        });
        let doc = 200.0 * 0.05 / 1_000_000.0;
        assert!((voyage.usd - 10.0 * doc).abs() < 1e-12);
        assert_eq!(voyage.calls, 1.0);

        note_observation("rerank", "voyageai/rerank-3", 0.01, 15.0, false);
        note_observation("rerank", "voyageai/rerank-3", 0.0, 0.0, true);
        let learned = estimate(&Work {
            op: Op::Rerank,
            model: "voyageai/rerank-3".into(),
            rows: 10.0,
            per_row: 1.0,
            knobs: Knobs::default(),
            grouping: Grouping::Batched,
            batch_rows: 32,
            concurrency: 1,
            threshold: None,
            filters: false,
        });
        assert!((learned.usd - 10.0 * 0.5 * 0.01).abs() < 1e-12);
        assert!((learned.latency_ms - 15.0).abs() < 1e-9);
        reset_history();
    }

    #[test]
    fn stats_json_sets_hit_rate_and_tail_parses_budget() {
        reset_history();
        let n = ingest_stats_json(
            r#"{"latency_by_op_model":[{"op":"predict","model":"jev","p50_ms":40,"calls":1,"hits":1}]}"#,
        );
        assert_eq!(n, 2);
        let est = estimate(&Work {
            op: Op::Predict,
            model: "jev".into(),
            rows: 2.0,
            per_row: 1.0,
            knobs: Knobs::default(),
            grouping: Grouping::PerGroup,
            batch_rows: 32,
            concurrency: 1,
            threshold: None,
            filters: true,
        });
        assert!((est.items - 1.0).abs() < 1e-9);
        assert!((est.latency_ms - 40.0).abs() < 1e-9);
        let k = knobs_from_tail("WITH TAKES 3 PHRASINGS 'one', 'two' BUDGET 0.01 CACHE off");
        assert_eq!(k.takes, 3);
        assert_eq!(k.phrasings, 2);
        assert_eq!(k.budget, Some(0.01));
        assert!(k.cache_off);
        let k = knobs_from_tail("USING MODEL 'voyageai/rerank-3' FALLBACK 'jev'");
        assert_eq!(k.model.as_deref(), Some("voyageai/rerank-3"));
        assert!(k.fallback);
        reset_history();
    }

    #[test]
    fn preflight_errors_and_the_override_skips_it() {
        disarm_query();
        set_guc_reader(guc_off);
        arm_query(0.02, Some(0.001));
        let err = preflight().unwrap_err();
        assert!(err.contains("exceeds budget"));
        let again = preflight().unwrap_err();
        assert!(again.contains("exceeds budget"));
        arm_query(0.02, Some(0.001));
        set_guc_reader(|name| {
            if name == "semantic.cost_check" {
                Some("off".into())
            } else {
                None
            }
        });
        assert!(preflight().is_ok());
        disarm_query();
        set_guc_reader(guc_off);
    }

    #[test]
    fn response_calls_land_on_the_node_and_the_total() {
        disarm_query();
        begin_query();
        let _scope = charge_scope(7);
        observe_response(
            r#"{"outputs":[{"key":"0","calls":[
                {"op":"predict","model":"jev","cost_usd":0.0000126,"latency_ms":40,"cache":"miss"},
                {"op":"predict","model":"jev","cost_usd":0,"latency_ms":0,"cache":"hit"}
            ]}]}"#,
        );
        drop(_scope);
        let node = node_actual(7).expect("node 7");
        assert_eq!(node.calls, 2);
        assert_eq!(node.cache_hits, 1);
        assert!((node.usd - 0.0000126).abs() < 1e-12);
        assert!((node.latency_ms - 40.0).abs() < 1e-9);
        assert_eq!(node.model, "jev");
        let total = query_actual();
        assert_eq!(total.calls, 2);
        assert_eq!(total.cache_hits, 1);
        end_query();
        begin_query();
        assert_eq!(query_actual().calls, 0);
        end_query();
        reset_history();
    }
}
