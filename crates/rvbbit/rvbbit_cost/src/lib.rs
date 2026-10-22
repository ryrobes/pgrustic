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
/// Billed input tokens for one jev CLASSIFY choice call.
/// A 12-row live gate billed $0.000246 against the 300-token estimate of
/// $0.000166 (that estimate already includes the 10% retake). 444 tokens
/// is that ratio, so 12 * 1.10 * 444 * $0.042 / 1e6 lands on $0.000246.
pub const JEV_CHOICE_BILLED_TOKENS: f64 = 444.0;

const MINI_USD_PER_MILLION_IN: f64 = 0.15;
const MINI_USD_PER_MILLION_OUT: f64 = 0.60;
const MINI_TOKENS_IN: f64 = 300.0;
const MINI_TOKENS_OUT: f64 = 50.0;

/// Voyage rerank is priced per token, like the rerank-2 list rate.
/// Live `/rerank` rows billed about half of a 200-token document.
const VOYAGE_USD_PER_MILLION: f64 = 0.05;
const VOYAGE_TOKENS_PER_DOC: f64 = 100.0;

const JEV_LATENCY_MS: f64 = 350.0;
const MINI_LATENCY_MS: f64 = 280.0;
/// Observed gpt-4o-mini SUMMARIZE p50, until `rvbbit.calls` replaces it.
const SUMMARIZE_LATENCY_MS: f64 = 1000.0;
/// Live SUMMARIZE USD ran at 5/8 of the shared mini seed (1.6x high).
const SUMMARIZE_USD_FACTOR: f64 = 5.0 / 8.0;
/// OpenRouter list price for `openai/text-embedding-3-small`.
const EMBED_USD_PER_MILLION: f64 = 0.02;
/// Seed tokens for one short note, until `rvbbit.calls` replaces it.
const EMBED_TOKENS_PER_TEXT: f64 = 32.0;
/// Seed latency for one embeddings POST. A measured p50 replaces it.
const EMBED_LATENCY_MS: f64 = 120.0;
const EMBED_MODEL: &str = "openai/text-embedding-3-small";
const VOYAGE_LATENCY_MS: f64 = 120.0;

/// A configured `FALLBACK` is assumed to fire on a quarter of the rows.
const FALLBACK_EXTRA: f64 = 0.25;
/// Share of single-take CLASSIFY rows that take the second choice call
/// when the winning probability is under 0.60. An Enron-scale run made
/// 199 extra calls on 2000 rows.
pub const CHOICE_RETAKE_FRACTION: f64 = 0.10;

const HISTORY_CAP: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Predict,
    Classify,
    Extract,
    Rerank,
    Summarize,
    Decide,
    Embed,
    /// An outbound MCP tool call. Unpriced: the seed is 0 USD.
    Mcp,
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
            Op::Embed => "embed",
            Op::Mcp => "mcp",
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
            "embed" | "about" | "semantic_similarity" => Some(Op::Embed),
            "mcp" => Some(Op::Mcp),
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
}

fn seed_for(model: &str, op: Op) -> Seed {
    if op == Op::Mcp {
        return Seed { usd_per_item: 0.0, latency_ms: 50.0 };
    }
    let m = model.trim().to_ascii_lowercase();
    if m.contains("gpt-4o-mini") || m.contains("4o-mini") {
        let usd = (MINI_TOKENS_IN * MINI_USD_PER_MILLION_IN
            + MINI_TOKENS_OUT * MINI_USD_PER_MILLION_OUT)
            / 1_000_000.0;
        if op == Op::Summarize {
            return Seed {
                usd_per_item: usd * SUMMARIZE_USD_FACTOR,
                latency_ms: SUMMARIZE_LATENCY_MS,
            };
        }
        return Seed { usd_per_item: usd, latency_ms: MINI_LATENCY_MS };
    }
    if op == Op::Rerank && (m.contains("voyage") || m.contains("rerank-3")) {
        return Seed {
            usd_per_item: VOYAGE_TOKENS_PER_DOC * VOYAGE_USD_PER_MILLION / 1_000_000.0,
            latency_ms: VOYAGE_LATENCY_MS,
        };
    }
    if op == Op::Embed {
        return Seed {
            usd_per_item: EMBED_TOKENS_PER_TEXT * EMBED_USD_PER_MILLION / 1_000_000.0,
            latency_ms: EMBED_LATENCY_MS,
        };
    }
    let tokens = if op == Op::Classify && (m.contains("jev") || m.is_empty()) {
        JEV_CHOICE_BILLED_TOKENS
    } else {
        JEV_BILLED_TOKENS
    };
    Seed {
        usd_per_item: tokens * JEV_USD_PER_MILLION / 1_000_000.0,
        latency_ms: JEV_LATENCY_MS,
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

fn same_model(sample: &str, want: &str) -> bool {
    let a = sample.trim().to_ascii_lowercase();
    let b = want.trim().to_ascii_lowercase();
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

fn history_samples(op: Op, model: &str) -> Vec<Sample> {
    let history = lock(&HISTORY);
    history
        .samples
        .iter()
        .filter(|s| s.op.eq_ignore_ascii_case(op.as_str()) && same_model(&s.model, model))
        .cloned()
        .collect()
}

fn mean_priced(samples: &[Sample]) -> Option<f64> {
    let priced: Vec<&Sample> = samples.iter().filter(|s| !s.hit && s.usd > 0.0).collect();
    if priced.is_empty() {
        return None;
    }
    Some(priced.iter().map(|s| s.usd).sum::<f64>() / priced.len() as f64)
}

fn p50_miss(samples: &[Sample]) -> Option<f64> {
    let mut values: Vec<f64> = samples.iter().filter(|s| !s.hit).map(|s| s.latency_ms).collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((values.len() - 1) as f64 * 0.50).round() as usize;
    Some(values[idx.min(values.len() - 1)])
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

/// Wire `mock` is not what hutch calls. Extract and summarize chat is
/// gpt-4o-mini. A voyage slug is the rerank list price. Any other explicit
/// slug keeps its name so a nemotron rerank is not relabeled jev.
fn priced_model(op: Op, model: &str) -> String {
    let m = model.trim();
    let blank = m.is_empty() || m.eq_ignore_ascii_case("mock") || m.eq_ignore_ascii_case("default");
    match op {
        Op::Extract | Op::Summarize if blank => "openai/gpt-4o-mini".to_string(),
        Op::Embed => {
            let low = m.to_ascii_lowercase();
            if blank || low == "jev" || low == "mock" || low == "default" {
                EMBED_MODEL.to_string()
            } else {
                m.to_string()
            }
        }
        Op::Rerank => {
            let low = m.to_ascii_lowercase();
            if low.contains("voyage") || low.contains("rerank-3") {
                "voyageai/rerank-3".to_string()
            } else if blank || low == "jev" {
                "jev".to_string()
            } else {
                m.to_string()
            }
        }
        _ if blank => "jev".to_string(),
        _ => m.to_string(),
    }
}

/// Dedicated rerank is one provider request for the whole HTTP batch.
/// Jev and chat calls fan out, one provider call per charged item.
fn one_shot_rerank(op: Op, model: &str) -> bool {
    op == Op::Embed || (op == Op::Rerank && model != "jev")
}

/// The low-confidence second choice runs only for one model, one take,
/// and no extra phrasing. Stacked takes already asked more than once.
fn choice_may_retake(work: &Work) -> bool {
    work.op == Op::Classify
        && work.knobs.takes.max(1) == 1
        && work.knobs.phrasings.max(1) == 1
        && work.knobs.across.max(1) == 1
}

pub fn estimate(work: &Work) -> Estimate {
    let requested = work
        .knobs
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| work.model.clone());
    let model = priced_model(work.op, &requested);
    let seed = seed_for(&model, work.op);
    let learned = learned(work.op, &model);
    let mut usd_per_item = learned.usd_per_item.unwrap_or(seed.usd_per_item);
    let mut latency_per = learned.latency_ms.unwrap_or(seed.latency_ms);
    if work.op == Op::Summarize {
        let own = history_samples(Op::Summarize, &model);
        if let Some(usd) = mean_priced(&own) {
            usd_per_item = usd;
        }
        // Prefer this op's own p50. Otherwise the gpt-4o-mini chat p50,
        // which is what SUMMARIZE actually spends (logged as extract until
        // the meter was tagged).
        if let Some(ms) = p50_miss(&own).or_else(|| p50_miss(&history_samples(Op::Extract, &model)))
        {
            latency_per = ms;
        }
    }
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
    let mut items = rows * per_row * takes * phrasings * across * fallback;
    if choice_may_retake(work) {
        items *= 1.0 + CHOICE_RETAKE_FRACTION;
    }
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
    // `calls` is HTTP batches. Fan-out ops wait on provider calls, so a
    // batch of 12 at concurrency 4 is three waves, not one flat seed.
    // Voyage posts one `/rerank` per HTTP batch, so its wave count stays
    // on the batch.
    let wave_units = if one_shot_rerank(work.op, &model) { calls } else { charged };
    let waves = if wave_units <= 0.0 { 0.0 } else { (wave_units / conc).ceil() };
    let mut threshold = work.threshold.or(work.knobs.threshold);
    if work.op == Op::Embed && threshold.is_none() {
        threshold = Some(rvbbit_contract::ABOUT_THRESHOLD);
    }
    let sel = if work.filters { selectivity(threshold) } else { 1.0 };
    Estimate {
        calls,
        usd: charged * usd_per_item,
        latency_ms: waves * latency_per,
        selectivity: sel,
        model,
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
        // A voyage rerank meter is one request cloned across `items`
        // documents. The planner multiplies per document, so the sample
        // has to be per document too.
        let items = row.get("items").and_then(|n| n.as_u64()).unwrap_or(1).max(1);
        let per_item = if items > 1 { usd / items as f64 } else { usd };
        note_observation(op, model, per_item, latency, cache == "hit");
        n += 1;
    }
    n
}

#[derive(Clone, Debug, Default)]
pub struct Actual {
    /// Provider misses. A cache hit is not a call.
    pub calls: u64,
    /// CLASSIFY choice calls taken because the first score was under 0.60.
    pub retakes: u64,
    pub cache_hits: u64,
    /// HTTP attempts for this node. A fully local cache run is 0.
    pub http_batches: u64,
    pub usd: f64,
    /// Wall time: per-wave maxima, not the sum of every call.
    pub latency_ms: f64,
    pub model: String,
}

struct MissSample {
    op: String,
    model: String,
    latency_ms: f64,
    stamp: String,
    generation: String,
    /// `items > 1` is one provider request cloned onto every document
    /// (voyage `/rerank`). Those clones share a timestamp and must not
    /// each open a wave. A normal call has `items` 1 and keeps its own
    /// sample even when the mock stamps every meter the same.
    items: u64,
}

impl Actual {
    fn add_miss(&mut self, usd: f64, model: &str) {
        self.calls += 1;
        self.usd += usd;
        self.remember_model(model);
    }

    fn add_hit(&mut self, model: &str) {
        self.cache_hits += 1;
        self.remember_model(model);
    }

    fn remember_model(&mut self, model: &str) {
        if self.model.is_empty() && !model.is_empty() {
            self.model = model.to_string();
        }
    }
}

/// One wave is `concurrency` calls wide. The wave costs its slowest call.
/// A dedicated rerank clones one meter onto every document (`items > 1`);
/// that group is one sample. Per-row calls stay separate.
fn wall_ms(misses: &[MissSample], concurrency: u32) -> f64 {
    let mut seen = std::collections::HashSet::new();
    let mut latencies = Vec::new();
    for miss in misses {
        if miss.items > 1 {
            let key = format!(
                "{}\u{1e}{}\u{1e}{}\u{1e}{}\u{1e}{}",
                miss.op,
                miss.model,
                miss.stamp,
                miss.latency_ms.to_bits(),
                miss.generation
            );
            if !seen.insert(key) {
                continue;
            }
        }
        latencies.push(miss.latency_ms);
    }
    let width = concurrency.max(1) as usize;
    latencies.chunks(width).map(|chunk| chunk.iter().copied().fold(0.0, f64::max)).sum()
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

static NESTED_PLAN: Mutex<Option<fn() -> bool>> = Mutex::new(None);

/// Installed at startup. True while a catalog SPI connection is planning,
/// so that plan must not clear the user statement's scan-row counts.
pub fn set_nested_plan_probe(probe: fn() -> bool) {
    *lock(&NESTED_PLAN) = Some(probe);
}

pub fn in_nested_planner() -> bool {
    lock(&NESTED_PLAN).is_some_and(|probe| probe())
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
///
/// An output with a value and no `calls` array is a client-memo or lookup
/// hit. Those used to leave the actuals empty, so `EXPLAIN ANALYZE` hid
/// the block on a fully cached run. `http_calls` is that response's HTTP
/// attempts; a local body passes 0. `concurrency` is the fan-out width
/// used to turn miss latencies into wall time.
pub fn observe_response(body: &str, http_calls: u32, concurrency: u32) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return;
    };
    let Some(outputs) = v.get("outputs").and_then(|o| o.as_array()) else {
        return;
    };
    let mut misses = Vec::new();
    for item in outputs {
        let calls = item.get("calls").and_then(|c| c.as_array()).filter(|c| !c.is_empty());
        if let Some(calls) = calls {
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
                if hit {
                    q.total.add_hit(model);
                } else {
                    q.total.add_miss(usd, model);
                    misses.push(MissSample {
                        op: op.to_string(),
                        model: model.to_string(),
                        latency_ms: latency,
                        stamp: call.get("timestamp").and_then(|s| s.as_str()).unwrap_or("").to_string(),
                        generation: call
                            .get("generation_id")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string(),
                        items: call.get("items").and_then(|s| s.as_u64()).unwrap_or(1),
                    });
                }
                let retake = call.get("retake").and_then(|v| v.as_bool()).unwrap_or(false);
                if let Some(id) = node {
                    let act = q.by_node.entry(id).or_default();
                    if hit {
                        act.add_hit(model);
                    } else {
                        act.add_miss(usd, model);
                    }
                    if retake {
                        act.retakes += 1;
                    }
                }
                if retake {
                    q.total.retakes += 1;
                }
            }
        } else if item.get("value").is_some_and(|v| !v.is_null())
            && item.get("error").map(|e| e.is_null()).unwrap_or(true)
        {
            let mut q = query();
            let node = q.node;
            q.total.add_hit("");
            if let Some(id) = node {
                q.by_node.entry(id).or_default().add_hit("");
            }
        }
    }
    let wall = wall_ms(&misses, concurrency);
    let mut q = query();
    let node = q.node;
    q.total.latency_ms += wall;
    q.total.http_batches += u64::from(http_calls);
    if let Some(id) = node {
        let act = q.by_node.entry(id).or_default();
        act.latency_ms += wall;
        act.http_batches += u64::from(http_calls);
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
            "semantic estimate {usd:.8} USD exceeds budget {cap:.8}"
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

        reset_history();
        let embedded = estimate(&Work {
            op: Op::Embed,
            model: "openai/text-embedding-3-small".into(),
            rows: 32.0,
            per_row: 1.0,
            knobs: Knobs::default(),
            grouping: Grouping::Batched,
            batch_rows: 32,
            concurrency: 4,
            threshold: None,
            filters: true,
        });
        let embed_per = EMBED_TOKENS_PER_TEXT * EMBED_USD_PER_MILLION / 1_000_000.0;
        assert!((embedded.usd - 32.0 * embed_per).abs() < 1e-12, "{}", embedded.usd);
        assert_eq!(embedded.calls, 1.0);
        assert!((embedded.latency_ms - EMBED_LATENCY_MS).abs() < 1e-9);
        assert_eq!(embedded.model, "openai/text-embedding-3-small");
        let about = selectivity(Some(rvbbit_contract::ABOUT_THRESHOLD));
        assert!((embedded.selectivity - about).abs() < 1e-12, "{}", embedded.selectivity);
        assert!((rvbbit_contract::ABOUT_THRESHOLD - 0.30).abs() < 1e-12);

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
        let doc = VOYAGE_TOKENS_PER_DOC * VOYAGE_USD_PER_MILLION / 1_000_000.0;
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
        // One HTTP batch, so the learned 15ms is one wave, not 5 * 15.
        assert!((learned.latency_ms - 15.0).abs() < 1e-9);
        assert!((voyage.latency_ms - VOYAGE_LATENCY_MS).abs() < 1e-9);
        reset_history();
    }

    #[test]
    fn extract_and_summarize_price_mini_and_means_fans_out() {
        reset_history();
        let mini = (MINI_TOKENS_IN * MINI_USD_PER_MILLION_IN + MINI_TOKENS_OUT * MINI_USD_PER_MILLION_OUT)
            / 1_000_000.0;
        let extract = estimate(&Work {
            op: Op::Extract,
            model: "mock".into(),
            rows: 3.0,
            per_row: 1.0,
            knobs: Knobs::default(),
            grouping: Grouping::Batched,
            batch_rows: 32,
            concurrency: 4,
            threshold: None,
            filters: false,
        });
        assert_eq!(extract.model, "openai/gpt-4o-mini");
        assert!((extract.usd - 3.0 * mini).abs() < 1e-12);
        assert_eq!(extract.calls, 1.0);
        assert!((extract.items - 3.0).abs() < 1e-9);
        assert!((extract.latency_ms - MINI_LATENCY_MS).abs() < 1e-9);

        let summary = estimate(&Work {
            op: Op::Summarize,
            model: "mock".into(),
            rows: 3.0,
            per_row: 1.0,
            knobs: Knobs::default(),
            grouping: Grouping::PerGroup,
            batch_rows: 32,
            concurrency: 4,
            threshold: None,
            filters: false,
        });
        assert_eq!(summary.model, "openai/gpt-4o-mini");
        assert!((summary.usd - 3.0 * mini * SUMMARIZE_USD_FACTOR).abs() < 1e-12);
        // Three groups at concurrency 4 are one wave of the mini p50.
        assert!((summary.latency_ms - SUMMARIZE_LATENCY_MS).abs() < 1e-9);

        let means = estimate(&Work {
            op: Op::Predict,
            model: "jev".into(),
            rows: 12.0,
            per_row: 1.0,
            knobs: Knobs::default(),
            grouping: Grouping::Batched,
            batch_rows: 32,
            concurrency: 4,
            threshold: None,
            filters: false,
        });
        let per = JEV_BILLED_TOKENS * JEV_USD_PER_MILLION / 1_000_000.0;
        assert_eq!(means.model, "jev");
        assert_eq!(means.calls, 1.0);
        assert!((means.items - 12.0).abs() < 1e-9);
        assert!((means.usd - 12.0 * per).abs() < 1e-12);
        assert!((means.latency_ms - 3.0 * JEV_LATENCY_MS).abs() < 1e-9);

        let mut knobs = Knobs::default();
        knobs.model = Some("voyageai/rerank-3".into());
        let voyage = estimate(&Work {
            op: Op::Rerank,
            model: "mock".into(),
            rows: 12.0,
            per_row: 1.0,
            knobs,
            grouping: Grouping::Batched,
            batch_rows: 32,
            concurrency: 4,
            threshold: None,
            filters: false,
        });
        let doc = VOYAGE_TOKENS_PER_DOC * VOYAGE_USD_PER_MILLION / 1_000_000.0;
        assert_eq!(voyage.model, "voyageai/rerank-3");
        assert_eq!(voyage.calls, 1.0);
        assert!((voyage.items - 12.0).abs() < 1e-9);
        assert!((voyage.usd - 12.0 * doc).abs() < 1e-12);
        assert!((voyage.latency_ms - VOYAGE_LATENCY_MS).abs() < 1e-9);

        let nemotron = estimate(&Work {
            op: Op::Rerank,
            model: "nvidia/nemotron-3.5-rerank-8b:free".into(),
            rows: 2.0,
            per_row: 1.0,
            knobs: Knobs::default(),
            grouping: Grouping::Batched,
            batch_rows: 32,
            concurrency: 1,
            threshold: None,
            filters: false,
        });
        assert_eq!(nemotron.model, "nvidia/nemotron-3.5-rerank-8b:free");
        reset_history();
    }

    #[test]
    fn calls_history_calibrates_voyage_per_doc_and_summarize_p50() {
        reset_history();
        let n = ingest_calls_json(
            r#"{"calls":[
                {"op":"rerank","model":"voyageai/rerank-3","cost_usd":0.00012,"latency_ms":90,"cache":"miss","items":12},
                {"op":"extract","model":"gpt-4o-mini","cost_usd":0.0002,"latency_ms":800,"cache":"miss","items":1},
                {"op":"extract","model":"openai/gpt-4o-mini","cost_usd":0.0002,"latency_ms":1200,"cache":"miss","items":1}
            ]}"#,
        );
        assert_eq!(n, 3);
        let voyage = estimate(&Work {
            op: Op::Rerank,
            model: "voyageai/rerank-3".into(),
            rows: 12.0,
            per_row: 1.0,
            knobs: Knobs::default(),
            grouping: Grouping::Batched,
            batch_rows: 32,
            concurrency: 4,
            threshold: None,
            filters: false,
        });
        // The meter was the whole request. Per document is 0.00012 / 12.
        assert!((voyage.usd - 0.00012).abs() < 1e-12, "{}", voyage.usd);

        let summary = estimate(&Work {
            op: Op::Summarize,
            model: "mock".into(),
            rows: 3.0,
            per_row: 1.0,
            knobs: Knobs::default(),
            grouping: Grouping::PerGroup,
            batch_rows: 32,
            concurrency: 4,
            threshold: None,
            filters: false,
        });
        // p50 of 800 and 1200, same index rule as hutch percentile: the higher one.
        // Extract USD must not replace the summarize seed.
        let mini = (MINI_TOKENS_IN * MINI_USD_PER_MILLION_IN + MINI_TOKENS_OUT * MINI_USD_PER_MILLION_OUT)
            / 1_000_000.0;
        assert!((summary.latency_ms - 1200.0).abs() < 1e-9, "{}", summary.latency_ms);
        assert!((summary.usd - 3.0 * mini * SUMMARIZE_USD_FACTOR).abs() < 1e-12, "{}", summary.usd);

        note_observation("summarize", "openai/gpt-4o-mini", 0.00004, 900.0, false);
        let priced = estimate(&Work {
            op: Op::Summarize,
            model: "openai/gpt-4o-mini".into(),
            rows: 1.0,
            per_row: 1.0,
            knobs: Knobs::default(),
            grouping: Grouping::PerGroup,
            batch_rows: 32,
            concurrency: 4,
            threshold: None,
            filters: false,
        });
        assert!((priced.usd - 0.00004).abs() < 1e-12, "{}", priced.usd);
        assert!((priced.latency_ms - 900.0).abs() < 1e-9, "{}", priced.latency_ms);
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
        assert!(err.contains("0.00100000"), "{err}");
        let again = preflight().unwrap_err();
        assert!(again.contains("exceeds budget"));
        arm_query(0.0000002, Some(0.0000001));
        let tiny = preflight().unwrap_err();
        assert!(tiny.contains("0.00000010"), "{tiny}");
        assert!(!tiny.contains("0.000000 USD"), "{tiny}");
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
                {"op":"predict","model":"jev","cost_usd":0.0000126,"latency_ms":100,"cache":"miss","timestamp":"t0"},
                {"op":"predict","model":"jev","cost_usd":0.0000126,"latency_ms":400,"cache":"miss","timestamp":"t1"},
                {"op":"predict","model":"jev","cost_usd":0,"latency_ms":1,"cache":"hit","timestamp":"t2"},
                {"op":"predict","model":"jev","cost_usd":0.0000126,"latency_ms":50,"cache":"miss","timestamp":"t3"},
                {"op":"predict","model":"jev","cost_usd":0.0000126,"latency_ms":200,"cache":"miss","timestamp":"t4"}
            ]},{"key":"1","value":true}]}"#,
            1,
            2,
        );
        drop(_scope);
        let node = node_actual(7).expect("node 7");
        assert_eq!(node.calls, 4);
        assert_eq!(node.cache_hits, 2);
        assert_eq!(node.http_batches, 1);
        assert!((node.usd - 0.0000504).abs() < 1e-12);
        // Waves of 2: max(100, 400) + max(50, 200). The hit and the
        // valueless-calls gap do not add. Summing the misses would be 750.
        assert!((node.latency_ms - 600.0).abs() < 1e-9);
        assert_eq!(node.model, "jev");
        let total = query_actual();
        assert_eq!(total.calls, 4);
        assert_eq!(total.cache_hits, 2);
        assert_eq!(total.http_batches, 1);
        end_query();
        begin_query();
        let _scope = charge_scope(3);
        observe_response(
            r#"{"outputs":[{"key":"0","value":true},{"key":"1","value":false}]}"#,
            0,
            4,
        );
        let cached = node_actual(3).expect("cached node");
        assert_eq!(cached.calls, 0);
        assert_eq!(cached.cache_hits, 2);
        assert_eq!(cached.http_batches, 0);
        assert!((cached.latency_ms - 0.0).abs() < 1e-9);
        end_query();
        begin_query();
        let _scope = charge_scope(9);
        observe_response(
            r#"{"outputs":[
                {"key":"0","calls":[{"op":"rerank","model":"voyageai/rerank-3","cost_usd":0.00003,"latency_ms":200,"cache":"miss","timestamp":"same","items":3}]},
                {"key":"1","calls":[{"op":"rerank","model":"voyageai/rerank-3","cost_usd":0.00003,"latency_ms":200,"cache":"miss","timestamp":"same","items":3}]},
                {"key":"2","calls":[{"op":"rerank","model":"voyageai/rerank-3","cost_usd":0.00003,"latency_ms":200,"cache":"miss","timestamp":"same","items":3}]}
            ]}"#,
            1,
            4,
        );
        let rerank = node_actual(9).expect("rerank node");
        assert_eq!(rerank.calls, 3);
        assert!((rerank.latency_ms - 200.0).abs() < 1e-9, "cloned rerank meters are one wave");
        end_query();
        begin_query();
        assert_eq!(query_actual().calls, 0);
        end_query();
        reset_history();
    }

    #[test]
    fn project_calls_count_rows_and_classify_includes_retakes() {
        reset_history();
        let project = estimate(&Work {
            op: Op::Predict,
            model: "jev".into(),
            rows: 40.0,
            per_row: 1.0,
            knobs: Knobs::default(),
            grouping: Grouping::Batched,
            batch_rows: 32,
            concurrency: 4,
            threshold: None,
            filters: false,
        });
        assert!((project.items - 40.0).abs() < 1e-9, "provider calls are per row");
        assert_eq!(project.calls, 2.0, "HTTP batches stay packed");

        let classify = estimate(&Work {
            op: Op::Classify,
            model: "jev".into(),
            rows: 2000.0,
            per_row: 1.0,
            knobs: Knobs::default(),
            grouping: Grouping::Batched,
            batch_rows: 32,
            concurrency: 4,
            threshold: None,
            filters: false,
        });
        let with_retake = 2000.0 * (1.0 + CHOICE_RETAKE_FRACTION);
        assert!((classify.items - with_retake).abs() < 1e-6);
        let per_choice = JEV_CHOICE_BILLED_TOKENS * JEV_USD_PER_MILLION / 1_000_000.0;
        assert!((classify.usd - with_retake * per_choice).abs() < 1e-9);

        let live = estimate(&Work {
            op: Op::Classify,
            model: "jev".into(),
            rows: 12.0,
            per_row: 1.0,
            knobs: Knobs::default(),
            grouping: Grouping::Batched,
            batch_rows: 32,
            concurrency: 4,
            threshold: None,
            filters: false,
        });
        assert!((live.usd - 0.000246).abs() < 1e-6, "classify usd {}", live.usd);
        let means = estimate(&Work {
            op: Op::Predict,
            model: "jev".into(),
            rows: 12.0,
            per_row: 1.0,
            knobs: Knobs::default(),
            grouping: Grouping::Batched,
            batch_rows: 32,
            concurrency: 4,
            threshold: None,
            filters: false,
        });
        let noul = 12.0 * JEV_BILLED_TOKENS * JEV_USD_PER_MILLION / 1_000_000.0;
        assert!((means.usd - noul).abs() < 1e-12, "means stays on the 300-token seed");

        let mut knobs = Knobs::default();
        knobs.takes = 2;
        let stacked = estimate(&Work {
            op: Op::Classify,
            model: "jev".into(),
            rows: 10.0,
            per_row: 1.0,
            knobs,
            grouping: Grouping::Batched,
            batch_rows: 32,
            concurrency: 4,
            threshold: None,
            filters: false,
        });
        assert!((stacked.items - 20.0).abs() < 1e-9, "stacked takes do not add the retry");

        begin_query();
        let _scope = charge_scope(4);
        observe_response(
            r#"{"outputs":[{"key":"0","calls":[
                {"op":"classify","model":"jev","cost_usd":0.01,"latency_ms":10,"cache":"miss","timestamp":"a"},
                {"op":"classify","model":"jev","cost_usd":0.01,"latency_ms":12,"cache":"miss","timestamp":"b","retake":true}
            ]}]}"#,
            1,
            4,
        );
        let node = node_actual(4).expect("classify node");
        assert_eq!(node.calls, 2);
        assert_eq!(node.retakes, 1);
        assert_eq!(query_actual().retakes, 1);
        end_query();
        reset_history();
    }
}
