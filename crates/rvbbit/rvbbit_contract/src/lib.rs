//! Hutch wire contract `rvbbit-hutch-v1`.
//!
//! Shared by pgrustic and (later) the real hutch. No engine dependencies.
//! The cache key built here is the client key: hutch mixes the tenant in
//! with [`storage_key`] and never trusts a client-supplied tenant.
//!
//! See `docs/rvbbit/HUTCH_CONTRACT.md`.

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

pub const CONTRACT_VERSION: &str = "rvbbit-hutch-v1";
pub const CACHE_KEY_VERSION: &str = "rvbbit-cache-v2";

pub const HEADER_CONTRACT: &str = "X-Rvbbit-Contract";
pub const HEADER_CACHE: &str = "X-Rvbbit-Cache";
pub const HEADER_AUTHORIZATION: &str = "Authorization";

pub const PATH_PREDICT: &str = "/v1/predict";
pub const PATH_CLASSIFY: &str = "/v1/classify";
pub const PATH_EXTRACT: &str = "/v1/extract";
pub const PATH_EMBED: &str = "/v1/embed";
pub const PATH_RERANK: &str = "/v1/rerank";
pub const PATH_DECIDE: &str = "/v1/decide";
pub const PATH_CACHE_LOOKUP: &str = "/v1/cache/lookup";
pub const PATH_CACHE_PUT: &str = "/v1/cache/put";
pub const PATH_CACHE_PURGE: &str = "/v1/cache/purge";
pub const PATH_CACHE_STATS: &str = "/v1/cache/stats";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    Predict,
    Classify,
    Extract,
    Embed,
    Rerank,
    Decide,
}

impl Operation {
    pub fn path(self) -> &'static str {
        match self {
            Operation::Predict => PATH_PREDICT,
            Operation::Classify => PATH_CLASSIFY,
            Operation::Extract => PATH_EXTRACT,
            Operation::Embed => PATH_EMBED,
            Operation::Rerank => PATH_RERANK,
            Operation::Decide => PATH_DECIDE,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheMode {
    ReadWrite,
    Refresh,
    Off,
}

impl CacheMode {
    pub fn as_str(self) -> &'static str {
        match self {
            CacheMode::ReadWrite => "read-write",
            CacheMode::Refresh => "refresh",
            CacheMode::Off => "off",
        }
    }
}

/// `X-Rvbbit-Cache: key=<hex>; ttl=<s>; mode=read-write|refresh|off`.
/// `key` and `ttl` are omitted when absent. A batch carries per-item keys
/// in the body; the header mode applies to every item.
pub fn cache_header(mode: CacheMode, key_hex: Option<&str>, ttl_secs: Option<u64>) -> String {
    let mut parts = Vec::new();
    if let Some(key) = key_hex {
        parts.push(format!("key={key}"));
    }
    if let Some(ttl) = ttl_secs {
        parts.push(format!("ttl={ttl}"));
    }
    parts.push(format!("mode={}", mode.as_str()));
    parts.join("; ")
}

/// Optional stack of decision-model calls for `predict`, `decide`, and
/// fixed-label `classify`. Absent means one call to `model` (or the
/// server default). A decision-tree node with `USING (...) COMBINE`,
/// `TAKES`, or `PHRASINGS` fills this. Hutch-lite honors it when a
/// client sends it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DecisionStack {
    /// Model slugs, in order. Empty means the request `model`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Repeats of each model. Absent means the server default (1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takes: Option<u32>,
    /// `mean` (default) or `vote`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub combine: Option<String>,
    /// Cut for a boolean. Absent means the server default (0.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
    /// Extra phrasings of the same question. Empty means the item prompt
    /// once. Each phrasing is another decision call, then `combine`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phrasings: Vec<String>,
    /// `mean`, `vote`, `consensus`, `judge`, or `first`. Absent means `combine`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pick: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BatchRequest {
    pub contract: String,
    pub operation: Operation,
    pub model: String,
    #[serde(default)]
    pub temperature: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    pub items: Vec<ItemRequest>,
    /// Decision pathway only (`predict`, `decide`, fixed-label `classify`).
    /// Chat operations ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<DecisionStack>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ItemRequest {
    /// Client correlation id. Responses echo it and stay positional.
    pub key: String,
    /// Hex client cache key ([`client_cache_key_hex`]). Not the storage key.
    pub cache_key: String,
    /// Canonical JSON object of named inputs.
    pub inputs: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criterion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Declared labels for fixed-label classify. Absent or empty means
    /// free-form classify (chat). Two or more labels use the decision
    /// model. Callers fold a non-empty list into the return-schema
    /// string before [`client_cache_key`] so unlabeled functions keep
    /// the same key. This field is not hashed on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    /// Optional description for each [`Self::labels`] entry, same order.
    /// An empty string uses the label itself. Omitted when every
    /// description is empty, so label-only cache keys stay put.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_descriptions: Option<Vec<String>>,
    /// Decision-tree node id (`n3`). Omitted from the provider body.
    /// Copied onto each [`CallMeter`] for this item so `rvbbit.calls`
    /// can group cost by node. Not part of the cache key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Extract decode. Absent or `json` requires a JSON value (catalog
    /// EXTRACT). `scalar` accepts a JSON scalar or plain text and stores
    /// the text as a string. Not part of the client cache key: callers
    /// that change the mode also change the prompt, which is hashed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extract_mode: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BatchResponse {
    pub contract: String,
    /// `outputs[i]` answers `items[i]`. A failed item sets `error` and
    /// leaves `value` null; siblings may still succeed.
    pub outputs: Vec<ItemResponse>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ItemResponse {
    pub key: String,
    #[serde(default)]
    pub value: Option<serde_json::Value>,
    /// Probability of `value`, when the operation has one.
    /// Predict / decide use the decision model's yes-probability.
    /// Fixed-label classify uses the winning label's probability, not
    /// the model's separate confidence field. Absent on chat operations
    /// and on errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Per-label probabilities for fixed-label classify. Keys are the
    /// declared labels. Absent on predict, extract, free-form classify,
    /// and errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scores: Option<serde_json::Value>,
    /// Provider calls and cache hits that produced this item, in order.
    /// A hit has `cost_usd` 0 and `cost_saved_usd` set to the provider
    /// USD that was avoided. Absent when the server did not meter the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls: Option<Vec<CallMeter>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ItemError>,
    /// Whether two or more takes of the same model returned the same
    /// score. False when they differed. Absent when the item was a
    /// single take. A live Jev run varied by about 0.03, so this was
    /// false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identical_takes: Option<bool>,
}

/// One provider call, or a cache hit that avoided one.
///
/// `cost_usd` and `cost_saved_usd` are US dollars as reported by the
/// provider (`usage.cost` or the generation endpoint's `total_cost`).
/// A cache hit stores `cost_usd = 0` and the avoided charge in
/// `cost_saved_usd`. Tokens on a hit are 0: the original miss already
/// recorded the billed tokens.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallMeter {
    /// `predict`, `classify`, `extract`, or `decide`.
    pub op: String,
    /// Response model when the provider sent one, otherwise the slug we sent.
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    /// USD charged for this call. 0 on a cache hit. Absent when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// USD avoided. 0 on a miss. Absent when the original charge is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_saved_usd: Option<f64>,
    /// Milliseconds spent in the inference HTTP call.
    /// Absent on a cache hit and on a single-flight waiter, so
    /// `avg(provider_latency_ms)` is the miss average. A
    /// generation-metadata GET is not included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_latency_ms: Option<u64>,
    /// Milliseconds spent in hutch for this call, including a generation
    /// lookup when one was required.
    pub latency_ms: u64,
    /// `hit`, `miss`, or `single-flight`.
    pub cache: String,
    /// Items covered by this provider call. Hutch-lite fans a batch out,
    /// so this is 1.
    pub items: u32,
    pub tenant: String,
    /// UTC RFC3339 with milliseconds.
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_id: Option<String>,
    /// Whether this call was one of several same-model takes that
    /// agreed. False when they differed. Absent on a single take.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identical: Option<bool>,
    /// Decision-tree node that caused this call. Absent for MEANS,
    /// classify, and extract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Yes-probability for a noul call, or the winning label's
    /// probability for a choice call. Absent on chat and on errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Request snippet when `semantic.audit_payloads` is on. Absent
    /// otherwise. The hutch call log is the receipt; this is not a
    /// Postgres table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ItemError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CacheLookupRequest {
    pub keys: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CacheHit {
    pub key: String,
    pub output: serde_json::Value,
    /// Yes-probability or winning-label probability, when the stored
    /// value had one. Older servers omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CacheLookupResponse {
    pub hits: Vec<CacheHit>,
    pub misses: Vec<String>,
}

/// Writer-queue payload. Real hutch stores these under [`storage_key`].
/// The mock does not implement this route.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CachePutRequest {
    pub entries: Vec<CachePutEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CachePutEntry {
    /// Hex client cache key.
    pub key: String,
    pub output: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CachePutResponse {
    pub stored: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CachePurgeRequest {
    /// Hex client keys. Empty `keys` with `all` purges the tenant.
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub all: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CachePurgeResponse {
    pub purged: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CacheStatsResponse {
    pub entries: u64,
    pub hits: u64,
    pub misses: u64,
    pub single_flight_waits: u64,
}

/// Fields that go into the client cache key. Tenant is intentionally absent.
#[derive(Clone, Copy, Debug)]
pub struct CacheKeyParts<'a> {
    pub fn_name: &'a str,
    pub fn_version: i32,
    pub system_prompt: &'a str,
    pub user_prompt: &'a str,
    pub return_schema: &'a str,
    pub model: &'a str,
    pub temperature: f64,
    pub max_tokens: Option<u32>,
    pub take_index: u32,
    pub seed: &'a str,
    /// Sorted option object. Labels and label descriptions live here so
    /// a description change misses even when the return schema forgot
    /// them. Empty object when the call has no options.
    pub opts: &'a serde_json::Value,
    /// Noul cutoff. `None` is an empty field, distinct from `0`.
    pub threshold: Option<f64>,
    /// `scalar`, `json`, or empty when the call is not an extract.
    pub extract_mode: &'a str,
    pub inputs: &'a serde_json::Value,
}

pub fn template_hash(system_prompt: &str, user_prompt: &str, return_schema: &str) -> String {
    let mut buf = Vec::new();
    push_lp(&mut buf, system_prompt.as_bytes());
    push_lp(&mut buf, user_prompt.as_bytes());
    push_lp(&mut buf, return_schema.as_bytes());
    hex(&blake3::hash(&buf))
}

pub fn client_cache_key(parts: &CacheKeyParts<'_>) -> [u8; 32] {
    let template = template_hash(parts.system_prompt, parts.user_prompt, parts.return_schema);
    let inputs = canonical_json(parts.inputs);
    let mut buf = Vec::new();
    push_lp(&mut buf, CACHE_KEY_VERSION.as_bytes());
    push_lp(&mut buf, parts.fn_name.as_bytes());
    push_lp(&mut buf, parts.fn_version.to_string().as_bytes());
    push_lp(&mut buf, template.as_bytes());
    push_lp(&mut buf, parts.model.as_bytes());
    push_lp(&mut buf, canonical_number(parts.temperature).as_bytes());
    push_lp(
        &mut buf,
        parts.max_tokens.map(|n| n.to_string()).unwrap_or_default().as_bytes(),
    );
    push_lp(&mut buf, canonical_json(parts.opts).as_bytes());
    push_lp(&mut buf, parts.take_index.to_string().as_bytes());
    push_lp(&mut buf, parts.seed.as_bytes());
    push_lp(
        &mut buf,
        parts
            .threshold
            .map(canonical_number)
            .unwrap_or_default()
            .as_bytes(),
    );
    push_lp(&mut buf, parts.extract_mode.as_bytes());
    push_lp(&mut buf, inputs.as_bytes());
    blake3::hash(&buf).into()
}

pub fn client_cache_key_hex(parts: &CacheKeyParts<'_>) -> String {
    hex(&client_cache_key(parts).into())
}

/// Hutch-side storage key. `client_key` is the raw 32-byte client key,
/// not the hex form. Tenant is mixed here and nowhere in the client key.
pub fn storage_key(client_key: &[u8; 32], tenant: &str) -> [u8; 32] {
    let mut buf = Vec::with_capacity(36 + tenant.len());
    push_lp(&mut buf, client_key);
    push_lp(&mut buf, tenant.as_bytes());
    blake3::hash(&buf).into()
}

fn push_lp(buf: &mut Vec<u8>, bytes: &[u8]) {
    let n = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    buf.extend_from_slice(&n.to_be_bytes());
    buf.extend_from_slice(&bytes[..n as usize]);
}

fn hex(hash: &blake3::Hash) -> String {
    hash.to_hex().to_string()
}

fn canonical_number(n: f64) -> String {
    if n.is_finite() {
        if let Some(num) = serde_json::Number::from_f64(n) {
            return num.to_string();
        }
    }
    "null".to_string()
}

/// Sorted object keys, NFC strings, stable JSON numbers, array order kept.
pub fn canonical_json(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_canon(&mut out, value);
    out
}

fn write_canon(out: &mut String, value: &serde_json::Value) {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        serde_json::Value::Number(n) => out.push_str(&n.to_string()),
        serde_json::Value::String(s) => {
            let nfc: String = s.nfc().collect();
            out.push_str(&serde_json::to_string(&nfc).unwrap_or_else(|_| "\"\"".into()));
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canon(out, item);
            }
            out.push(']');
        }
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let nfc: String = key.nfc().collect();
                out.push_str(&serde_json::to_string(&nfc).unwrap_or_else(|_| "\"\"".into()));
                out.push(':');
                write_canon(out, &map[*key]);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_ignores_object_key_order_and_nfc() {
        let a = serde_json::json!({"b": 1, "a": "é"});
        let composed = "e\u{0301}";
        let b = serde_json::json!({"a": composed, "b": 1});
        let opts = serde_json::json!({});
        let pa = CacheKeyParts {
            fn_name: "rvbbit.means",
            fn_version: 1,
            system_prompt: "sys",
            user_prompt: "user",
            return_schema: "boolean",
            model: "mock",
            temperature: 0.0,
            max_tokens: Some(16),
            take_index: 0,
            seed: "",
            opts: &opts,
            threshold: None,
            extract_mode: "",
            inputs: &a,
        };
        let mut pb = pa.clone();
        pb.inputs = &b;
        assert_eq!(client_cache_key(&pa), client_cache_key(&pb));
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert!(canonical_json(&a).starts_with("{\"a\":"));
    }

    #[test]
    fn version_and_tenant_change_the_key() {
        let inputs = serde_json::json!({"input": "x"});
        let opts = serde_json::json!({});
        let p = CacheKeyParts {
            fn_name: "rvbbit.means",
            fn_version: 1,
            system_prompt: "",
            user_prompt: "",
            return_schema: "boolean",
            model: "mock",
            temperature: 0.0,
            max_tokens: None,
            take_index: 0,
            seed: "",
            opts: &opts,
            threshold: None,
            extract_mode: "",
            inputs: &inputs,
        };
        let mut q = p.clone();
        q.fn_version = 2;
        assert_ne!(client_cache_key(&p), client_cache_key(&q));
        let raw = client_cache_key(&p);
        assert_ne!(storage_key(&raw, "a"), storage_key(&raw, "b"));
        assert_ne!(storage_key(&raw, "a"), raw);
    }

    #[test]
    fn header_off_has_no_key() {
        assert_eq!(cache_header(CacheMode::Off, None, None), "mode=off");
        assert_eq!(
            cache_header(CacheMode::ReadWrite, Some("ab"), Some(30)),
            "key=ab; ttl=30; mode=read-write"
        );
    }

    #[test]
    fn threshold_extract_mode_and_opts_change_the_key() {
        let inputs = serde_json::json!({"input": "x"});
        let opts = serde_json::json!({"label_descriptions": ["tracks", "saw it"]});
        let swapped = serde_json::json!({"label_descriptions": ["tracks", "saw it"]});
        let base = CacheKeyParts {
            fn_name: "classify",
            fn_version: 1,
            system_prompt: "",
            user_prompt: "q",
            return_schema: "text",
            model: "typesafe/jev-1.14",
            temperature: 0.0,
            max_tokens: None,
            take_index: 0,
            seed: "",
            opts: &opts,
            threshold: Some(0.5),
            extract_mode: "",
            inputs: &inputs,
        };
        let mut other = base.clone();
        other.opts = &swapped;
        assert_eq!(client_cache_key(&base), client_cache_key(&other));
        other.threshold = Some(0.9);
        assert_ne!(client_cache_key(&base), client_cache_key(&other));
        other.threshold = Some(0.5);
        other.extract_mode = "scalar";
        assert_ne!(client_cache_key(&base), client_cache_key(&other));
        other.extract_mode = "";
        other.model = "other";
        assert_ne!(client_cache_key(&base), client_cache_key(&other));
        other.model = "typesafe/jev-1.14";
        other.temperature = 0.4;
        assert_ne!(client_cache_key(&base), client_cache_key(&other));
        other.temperature = 0.0;
        other.user_prompt = "other prompt";
        assert_ne!(client_cache_key(&base), client_cache_key(&other));
        other.user_prompt = "q";
        other.take_index = 2;
        assert_ne!(client_cache_key(&base), client_cache_key(&other));
    }
}
