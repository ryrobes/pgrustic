//! OpenRouter cost and token fields.
//!
//! Chat completions include `usage.cost`, `usage.prompt_tokens`, and
//! `usage.completion_tokens` on every non-streaming response. The Decisions
//! API includes `usage.cost`, `usage.input_tokens`, and `usage.output_tokens`.
//! Both responses carry `id`. When `usage.cost` is missing and `id` matches
//! `gen-...`, hutch GETs `{base}/generation?id=` and reads `data.total_cost`
//! (or numeric `data.usage`), `data.tokens_prompt`, and `data.tokens_completion`.

use std::time::{SystemTime, UNIX_EPOCH};

use rvbbit_contract::CallMeter;

#[derive(Clone, Debug, Default)]
pub struct ProviderUsage {
    pub generation_id: Option<String>,
    pub model: Option<String>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub cost_usd: Option<f64>,
}

pub fn usage_from_body(body: &str) -> ProviderUsage {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return ProviderUsage::default();
    };
    let mut usage = ProviderUsage {
        generation_id: v.get("id").and_then(json_str),
        model: v.get("model").and_then(json_str),
        tokens_in: None,
        tokens_out: None,
        cost_usd: None,
    };
    if let Some(u) = v.get("usage") {
        usage.tokens_in = field_u64(u, &["prompt_tokens", "input_tokens"]);
        usage.tokens_out = field_u64(u, &["completion_tokens", "output_tokens"]);
        usage.cost_usd = u.get("cost").and_then(json_f64);
    }
    usage
}

/// Fill gaps from `GET /generation`. Does not overwrite a cost that was
/// already present on the inference response.
pub fn merge_generation(usage: &mut ProviderUsage, body: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return;
    };
    let data = v.get("data").unwrap_or(&v);
    if usage.generation_id.is_none() {
        usage.generation_id = data.get("id").and_then(json_str);
    }
    if usage.model.is_none() {
        usage.model = data.get("model").and_then(json_str);
    }
    if usage.tokens_in.is_none() {
        usage.tokens_in = field_u64(data, &["tokens_prompt", "native_tokens_prompt"]);
    }
    if usage.tokens_out.is_none() {
        usage.tokens_out = field_u64(data, &["tokens_completion", "native_tokens_completion"]);
    }
    if usage.cost_usd.is_none() {
        usage.cost_usd = data
            .get("total_cost")
            .and_then(json_f64)
            .or_else(|| data.get("usage").and_then(json_f64));
    }
}

/// OpenRouter generation ids match `^gen-[0-9A-Za-z-]+$`.
pub fn generation_id_ok(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("gen-") else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

pub fn op_name(op: rvbbit_contract::Operation) -> &'static str {
    match op {
        rvbbit_contract::Operation::Predict => "predict",
        rvbbit_contract::Operation::Classify => "classify",
        rvbbit_contract::Operation::Extract => "extract",
        rvbbit_contract::Operation::Decide => "decide",
        rvbbit_contract::Operation::Embed => "embed",
        rvbbit_contract::Operation::Rerank => "rerank",
    }
}

pub fn summarize_produced(calls: &[CallMeter]) -> (Option<f64>, String, Option<String>) {
    let mut models: Vec<&str> = Vec::new();
    for call in calls {
        if !call.model.is_empty() && !models.contains(&call.model.as_str()) {
            models.push(call.model.as_str());
        }
    }
    let model = models.join(",");
    let misses: Vec<&CallMeter> = calls.iter().filter(|c| c.cache == "miss").collect();
    let produced_cost = if misses.is_empty() || misses.iter().any(|c| c.cost_usd.is_none()) {
        None
    } else {
        Some(misses.iter().filter_map(|c| c.cost_usd).sum())
    };
    let generation_id = if calls.len() == 1 {
        calls[0].generation_id.clone()
    } else {
        None
    };
    (produced_cost, model, generation_id)
}

pub fn meter_log_line(meter: &CallMeter, api_key: &str) -> String {
    let line = format!(
        "hutch_call ts={} op={} model={} tokens_in={} tokens_out={} cost_usd={} cost_saved_usd={} provider_latency_ms={} latency_ms={} cache={} items={} tenant={} generation_id={} node={} score={}",
        meter.timestamp,
        log_token(&meter.op),
        log_token(&meter.model),
        opt_num(meter.tokens_in),
        opt_num(meter.tokens_out),
        opt_f64(meter.cost_usd),
        opt_f64(meter.cost_saved_usd),
        opt_num(meter.provider_latency_ms),
        meter.latency_ms,
        log_token(&meter.cache),
        meter.items,
        log_token(&meter.tenant),
        meter.generation_id.as_deref().map(log_token).unwrap_or_else(|| "-".into()),
        meter.node.as_deref().map(log_token).unwrap_or_else(|| "-".into()),
        opt_f64(meter.score),
    );
    redact_key(&line, api_key)
}

pub fn percentile(samples: &[u64], p: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut v = samples.to_vec();
    v.sort_unstable();
    let idx = ((v.len() - 1) as f64 * p).round() as usize;
    v[idx.min(v.len() - 1)]
}

pub fn rfc3339_now() -> String {
    rfc3339(SystemTime::now())
}

fn rfc3339(now: SystemTime) -> String {
    let dur = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs();
    let millis = dur.subsec_millis();
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Howard Hinnant's `civil_from_days`. `days` is Unix days.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    (year, month as u32, day as u32)
}

fn json_str(v: &serde_json::Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(|s| s.to_string())
}

fn json_f64(v: &serde_json::Value) -> Option<f64> {
    v.as_f64().filter(|n| n.is_finite())
}

fn json_u64(v: &serde_json::Value) -> Option<u64> {
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    let n = v.as_f64()?;
    if n.is_finite() && n >= 0.0 && n <= u64::MAX as f64 {
        Some(n as u64)
    } else {
        None
    }
}

fn field_u64(obj: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    let obj = obj.as_object()?;
    for key in keys {
        if let Some(v) = obj.get(*key) {
            if let Some(n) = json_u64(v) {
                return Some(n);
            }
        }
    }
    None
}

fn opt_num(n: Option<u64>) -> String {
    n.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
}

fn opt_f64(n: Option<f64>) -> String {
    match n {
        Some(v) if v.is_finite() => format!("{v}"),
        _ => "-".into(),
    }
}

fn log_token(value: &str) -> String {
    if value.is_empty() {
        return "-".into();
    }
    value
        .chars()
        .map(|c| if c.is_whitespace() { '_' } else { c })
        .collect()
}

fn redact_key(line: &str, api_key: &str) -> String {
    if api_key.is_empty() {
        return line.to_string();
    }
    line.replace(api_key, "[redacted]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_and_decision_usage_fields() {
        let chat = usage_from_body(
            r#"{"id":"gen-chat","model":"openai/gpt-4o-mini","usage":{"prompt_tokens":100,"completion_tokens":20,"cost":0.000027}}"#,
        );
        assert_eq!(chat.generation_id.as_deref(), Some("gen-chat"));
        assert_eq!(chat.tokens_in, Some(100));
        assert_eq!(chat.tokens_out, Some(20));
        assert_eq!(chat.cost_usd, Some(0.000027));

        let decision = usage_from_body(
            r#"{"id":"gen-dec-1","model":"typesafe/jev-1.14","usage":{"input_tokens":80,"output_tokens":4,"cost":0.00000336}}"#,
        );
        assert_eq!(decision.tokens_in, Some(80));
        assert_eq!(decision.tokens_out, Some(4));
        assert_eq!(decision.cost_usd, Some(0.00000336));
    }

    #[test]
    fn generation_fallback_fills_a_missing_cost() {
        let mut usage = usage_from_body(r#"{"id":"gen-nocost-1","model":"openai/gpt-4o-mini"}"#);
        assert!(usage.cost_usd.is_none());
        merge_generation(
            &mut usage,
            r#"{"data":{"id":"gen-nocost-1","total_cost":0.000009,"tokens_prompt":50,"tokens_completion":10,"usage":0.000009}}"#,
        );
        assert_eq!(usage.cost_usd, Some(0.000009));
        assert_eq!(usage.tokens_in, Some(50));
        assert_eq!(usage.tokens_out, Some(10));
        assert!(generation_id_ok("gen-dec-1790015143-AIaTutprXsJ5EwohRSjb"));
        assert!(!generation_id_ok("chatcmpl-1"));
    }

    #[test]
    fn percentile_and_log_line() {
        assert_eq!(percentile(&[], 0.5), 0);
        assert_eq!(percentile(&[10], 0.95), 10);
        assert_eq!(percentile(&[10, 20, 30, 40, 100], 0.5), 30);
        let meter = CallMeter {
            op: "predict".into(),
            model: "sk-secret".into(),
            tokens_in: Some(1),
            tokens_out: None,
            cost_usd: Some(0.25),
            cost_saved_usd: Some(0.0),
            provider_latency_ms: Some(4),
            latency_ms: 5,
            cache: "miss".into(),
            items: 1,
            tenant: "local".into(),
            timestamp: "2026-10-21T00:00:00.000Z".into(),
            generation_id: None,
            node: None,
            score: None,
            identical: None,
            payload: None,
        };
        let line = meter_log_line(&meter, "sk-secret");
        assert!(line.starts_with("hutch_call "));
        assert!(!line.contains("sk-secret"), "{line}");
        assert!(line.contains("model=[redacted]"), "{line}");
        assert!(line.contains("provider_latency_ms=4"), "{line}");
        assert!(line.contains("tokens_out=-"), "{line}");
        assert!(line.contains("generation_id=-"), "{line}");
    }

    #[test]
    fn rfc3339_epoch_and_a_known_day() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let day = UNIX_EPOCH + std::time::Duration::from_secs(86_400);
        assert_eq!(rfc3339(day), "1970-01-02T00:00:00.000Z");
        // 2021-01-01T00:00:00Z
        let y2021 = UNIX_EPOCH + std::time::Duration::from_secs(1_609_459_200);
        assert_eq!(rfc3339(y2021), "2021-01-01T00:00:00.000Z");
    }
}
