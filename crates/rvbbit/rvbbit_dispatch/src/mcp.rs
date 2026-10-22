//! HTTP client for the `hutch-mcp` gateway. Postgres stays on ureq.
//! Retries happen in the gateway, so this posts once.

use std::time::Duration;

use mcp_core::{arguments_object, ListedTool, ServerSpec};
use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub struct McpCallOpts {
    pub timeout_ms: u64,
    pub retries: u32,
    pub cache_ttl_ms: u64,
    pub read_only: bool,
    pub max_cost: Option<f64>,
    pub concurrency: usize,
}

impl Default for McpCallOpts {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            retries: 2,
            cache_ttl_ms: 0,
            read_only: false,
            max_cost: None,
            concurrency: 4,
        }
    }
}

pub fn mcp_list(spec: &ServerSpec) -> Result<Vec<ListedTool>, String> {
    let body = serde_json::to_string(spec).map_err(|e| e.to_string())?;
    let text = post("/v1/mcp/list", &body, 60)?;
    let parsed: Value = serde_json::from_str(&text).map_err(|e| format!("mcp list json: {e}"))?;
    if let Some(err) = parsed.get("error").and_then(|v| v.as_str()) {
        if !err.is_empty() {
            return Err(err.to_string());
        }
    }
    let tools = parsed.get("tools").cloned().unwrap_or(Value::Null);
    serde_json::from_value(tools).map_err(|e| format!("mcp list tools: {e}"))
}

pub fn mcp_invoke_tool(tool: &rvbbit_resolve::McpTool, positional_json: &str) -> Result<Value, String> {
    let spec = spec_from_tool(tool);
    let arguments = mcp_arguments(&tool.arg_keys, positional_json)?;
    mcp_call(&spec, &tool.tool, arguments, &opts_from_tool(tool))
}

fn spec_from_tool(tool: &rvbbit_resolve::McpTool) -> ServerSpec {
    ServerSpec {
        name: tool.server.clone(),
        transport: tool.transport.clone(),
        url: tool.url.clone(),
        command: tool.command.clone(),
        args: tool.args.clone(),
        headers: serde_json::from_str(&tool.headers_json).unwrap_or_default(),
        env: serde_json::from_str(&tool.env_json).unwrap_or_default(),
        timeout_ms: tool.timeout_ms,
        retries: tool.retries,
    }
}

fn opts_from_tool(tool: &rvbbit_resolve::McpTool) -> McpCallOpts {
    McpCallOpts {
        timeout_ms: tool.timeout_ms,
        retries: tool.retries,
        cache_ttl_ms: tool.cache_ttl_ms,
        read_only: tool.read_only,
        max_cost: tool.max_cost,
        concurrency: 4,
    }
}

pub fn mcp_call(spec: &ServerSpec, tool: &str, arguments: Value, opts: &McpCallOpts) -> Result<Value, String> {
    let body = serde_json::to_string(&call_body(spec, tool, arguments, opts)).map_err(|e| e.to_string())?;
    let text = post("/v1/mcp/call", &body, budget_secs(opts))?;
    let row: Value = serde_json::from_str(&text).map_err(|e| format!("mcp call json: {e}"))?;
    observe(&[&row], opts.concurrency);
    row_value(&row)
}

pub fn mcp_batch_for_tool(
    tool: &rvbbit_resolve::McpTool,
    calls: Vec<(String, Value)>,
    opts: &McpCallOpts,
) -> Result<Vec<Result<Value, String>>, String> {
    mcp_batch(&spec_from_tool(tool), calls, opts)
}

pub fn mcp_batch(
    spec: &ServerSpec,
    calls: Vec<(String, Value)>,
    opts: &McpCallOpts,
) -> Result<Vec<Result<Value, String>>, String> {
    let body = json!({
        "server": spec,
        "concurrency": opts.concurrency.max(1),
        "timeout_ms": opts.timeout_ms,
        "retries": opts.retries,
        "cache_ttl_ms": opts.cache_ttl_ms,
        "read_only": opts.read_only,
        "max_cost": opts.max_cost,
        "calls": calls.into_iter().map(|(tool, arguments)| json!({"tool": tool, "arguments": arguments})).collect::<Vec<_>>(),
    });
    let text = post("/v1/mcp/batch", &body.to_string(), budget_secs(opts))?;
    let parsed: Value = serde_json::from_str(&text).map_err(|e| format!("mcp batch json: {e}"))?;
    if let Some(err) = parsed.get("error").and_then(|v| v.as_str()) {
        if parsed.get("results").is_none() {
            return Err(err.to_string());
        }
    }
    let results = parsed.get("results").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let refs: Vec<&Value> = results.iter().collect();
    observe(&refs, opts.concurrency);
    Ok(results.iter().map(row_value).collect())
}

/// Build the JSON object a tool expects from the positional SQL array.
pub fn mcp_arguments(keys: &[String], positional_json: &str) -> Result<Value, String> {
    let parsed: Value = serde_json::from_str(positional_json).unwrap_or(Value::Null);
    Ok(arguments_object(keys, &parsed))
}

pub fn mcp_url() -> String {
    let guc = guc::GetConfigOption("semantic.mcp_url", true, false).ok().flatten();
    if let Some(url) = guc.as_deref() {
        if !url.is_empty() && url != "http://127.0.0.1:8111" {
            return url.to_string();
        }
    }
    if let Ok(url) = std::env::var("RVBBIT_MCP_URL") {
        if !url.trim().is_empty() {
            return url.trim().to_string();
        }
    }
    if let Ok(port) = std::env::var("HUTCH_MCP_PORT") {
        let port = port.trim();
        if !port.is_empty() && !port.contains('/') {
            return format!("http://127.0.0.1:{port}");
        }
    }
    guc.unwrap_or_else(|| "http://127.0.0.1:8111".into())
}

fn call_body(spec: &ServerSpec, tool: &str, arguments: Value, opts: &McpCallOpts) -> Value {
    json!({
        "server": spec,
        "tool": tool,
        "arguments": arguments,
        "timeout_ms": opts.timeout_ms,
        "retries": opts.retries,
        "cache_ttl_ms": opts.cache_ttl_ms,
        "read_only": opts.read_only,
        "max_cost": opts.max_cost,
    })
}

/// A memo the builtin kept from an earlier statement. The gateway never
/// saw this call, so the row is recorded locally and the analyze counters
/// still see a cache hit.
pub fn note_mcp_memo_hit(server: &str, tool: &str, ok: bool) {
    let status = if ok { "ok" } else { "error" };
    let model = format!("mcp:{server}/{tool}");
    crate::push_local_mcp_call(json!({
        "op": "mcp",
        "model": model,
        "tokens_in": null,
        "tokens_out": null,
        "cost": 0.0,
        "cost_saved": 0.0,
        "latency_ms": 0,
        "provider_latency_ms": null,
        "cache": "hit",
        "items": 1,
        "tenant": "",
        "generation_id": status,
        "node": server,
        "score": null,
        "identical": null,
        "ts": rfc3339_now(),
        "server": server,
        "tool": tool,
        "status": status,
    }));
    let body = json!({
        "outputs": [{
            "value": if ok { json!("") } else { Value::Null },
            "calls": [{
                "op": "mcp",
                "model": model,
                "cache": "hit",
                "cost_usd": 0.0,
                "latency_ms": 0
            }]
        }]
    });
    rvbbit_cost::observe_response(&body.to_string(), 0, 1);
}

fn rfc3339_now() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let (year, month, day) = civil_from_days((dur.as_secs() / 86_400) as i64);
    let sod = dur.as_secs() % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60,
        dur.subsec_millis()
    )
}

/// Days since the Unix epoch to a civil date. Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    (year as i32, month as u32, day as u32)
}

fn observe(rows: &[&Value], concurrency: usize) {
    let outputs: Vec<Value> = rows
        .iter()
        .map(|row| json!({"value": row.get("value").cloned().unwrap_or(Value::Null), "calls": row.get("calls").cloned().unwrap_or(Value::Array(Vec::new()))}))
        .collect();
    let body = json!({"outputs": outputs}).to_string();
    rvbbit_cost::observe_response(&body, 1, concurrency.max(1) as u32);
}

fn row_value(row: &Value) -> Result<Value, String> {
    if let Some(err) = row.get("error").and_then(|v| v.as_str()) {
        if !err.is_empty() {
            return Err(err.to_string());
        }
    }
    if row.get("status").and_then(|v| v.as_str()) == Some("error") {
        return Err(row.get("error").and_then(|v| v.as_str()).unwrap_or("mcp tool error").to_string());
    }
    Ok(row.get("value").cloned().unwrap_or(Value::Null))
}

fn budget_secs(opts: &McpCallOpts) -> u64 {
    let one = opts.timeout_ms / 1000 + 2;
    one.saturating_mul(opts.retries as u64 + 1).saturating_add(5).max(10)
}

fn post(path: &str, body: &str, timeout_secs: u64) -> Result<String, String> {
    let url = format!("{}{path}", mcp_url().trim_end_matches('/'));
    let resp = ureq::post(&url)
        .set("content-type", "application/json")
        .timeout(Duration::from_secs(timeout_secs))
        .send_string(body)
        .map_err(|e| format!("hutch-mcp {path}: {e}"))?;
    resp.into_string().map_err(|e| format!("hutch-mcp body: {e}"))
}
