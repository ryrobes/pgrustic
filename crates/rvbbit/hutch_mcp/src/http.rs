//! Postgres-facing HTTP API. The backend posts server specs and tool
//! calls here. Secrets are resolved inside the process. Read-only tools
//! with a cache ttl are memoized. Every attempt is appended to the call
//! log that `GET /v1/calls` serves.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};

use socket2::{Domain, Socket, Type};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use mcp_core::{ServerSpec, ToolCache};
use rvbbit_contract::{CallMeter, CALL_LOG_CAP};
use serde_json::{json, Value};

use crate::client;

struct State {
    cache: ToolCache,
    calls: VecDeque<CallMeter>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(State {
            cache: ToolCache::new(),
            calls: VecDeque::new(),
        })
    })
}

pub fn bind_listener(addr: &str) -> std::io::Result<TcpListener> {
    let std_addr: SocketAddr = addr.parse().map_err(|err| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("bad address {addr}: {err}"))
    })?;
    let domain = match std_addr {
        SocketAddr::V4(_) => Domain::IPV4,
        SocketAddr::V6(_) => Domain::IPV6,
    };
    let socket = Socket::new(domain, Type::STREAM, None)?;
    socket.set_reuse_address(true)?;
    socket.bind(&std_addr.into())?;
    socket.listen(128)?;
    Ok(TcpListener::from(socket))
}

pub fn serve(addr: &str) -> std::io::Result<()> {
    let listener = bind_listener(addr)?;
    for conn in listener.incoming() {
        let conn = conn?;
        thread::spawn(move || {
            let _ = handle(conn);
        });
    }
    Ok(())
}

fn handle(mut conn: TcpStream) -> std::io::Result<()> {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(30)));
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        let n = conn.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_header_end(&buf) {
            header_end = pos;
            break;
        }
        if buf.len() > 1_048_576 {
            return write_json(&mut conn, 413, &json!({"error": "request too large"}));
        }
    }
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request = lines.next().unwrap_or("");
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let mut content_length = 0usize;
    for line in lines {
        if let Some(rest) = line.split_once(':') {
            if rest.0.eq_ignore_ascii_case("content-length") {
                content_length = rest.1.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = conn.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);
    let response = route(method, path, &body);
    write_json(&mut conn, response.0, &response.1)
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn route(method: &str, path: &str, body: &[u8]) -> (u16, Value) {
    let path = path.split('?').next().unwrap_or(path);
    if method == "GET" && path == "/health" {
        return (200, json!({"ok": true}));
    }
    if method == "GET" && (path == "/v1/calls" || path.starts_with("/v1/calls")) {
        return (200, json!({"calls": recent_calls(query_limit(path))}));
    }
    if method != "POST" {
        return (404, json!({"error": "not found"}));
    }
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return (400, json!({"error": format!("json: {e}")})),
    };
    match path {
        "/v1/mcp/list" => (200, list(&parsed)),
        "/v1/mcp/call" => (200, call_one(&parsed)),
        "/v1/mcp/batch" => (200, call_batch(&parsed)),
        _ => (404, json!({"error": "not found"})),
    }
}

fn query_limit(path: &str) -> usize {
    path.split("limit=")
        .nth(1)
        .and_then(|s| s.split('&').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(50)
        .min(CALL_LOG_CAP)
}

fn recent_calls(limit: usize) -> Vec<CallMeter> {
    let guard = state().lock().unwrap_or_else(|e| e.into_inner());
    let skip = guard.calls.len().saturating_sub(limit);
    guard.calls.iter().skip(skip).cloned().collect()
}

fn list(body: &Value) -> Value {
    let spec = match spec_of(body) {
        Ok(spec) => spec,
        Err(e) => return json!({"error": e}),
    };
    let retries = spec.retries;
    let mut last = String::new();
    for _ in 0..=retries {
        match client::list_tools(&spec) {
            Ok(tools) => return json!({"tools": tools}),
            Err(e) => last = e,
        }
    }
    json!({"error": last})
}

fn call_one(body: &Value) -> Value {
    let spec = match spec_of(body.get("server").unwrap_or(body)) {
        Ok(spec) => spec,
        Err(e) => return json!({"error": e, "status": "error"}),
    };
    let tool = body.get("tool").and_then(|v| v.as_str()).unwrap_or("");
    let arguments = body.get("arguments").cloned().unwrap_or(json!({}));
    let opts = opts_of(body, &spec);
    json!(invoke(&spec, tool, arguments, &opts))
}

fn call_batch(body: &Value) -> Value {
    let spec = match spec_of(body.get("server").unwrap_or(body)) {
        Ok(spec) => spec,
        Err(e) => return json!({"error": e}),
    };
    let opts = opts_of(body, &spec);
    let calls = body.get("calls").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut pending = Vec::new();
    let mut results = vec![Value::Null; calls.len()];
    for (i, call) in calls.iter().enumerate() {
        let tool = call.get("tool").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let arguments = call.get("arguments").cloned().unwrap_or(json!({}));
        if let Some(hit) = cache_get(&spec, &tool, &arguments, &opts) {
            results[i] = hit;
            continue;
        }
        if let Err(message) = cost_gate(&opts) {
            results[i] = error_row(&spec, &tool, &message, 0, "miss", &opts.role);
            continue;
        }
        pending.push((i, tool, arguments));
    }
    if !pending.is_empty() {
        let batch = pending.iter().map(|(_, tool, args)| (tool.clone(), args.clone())).collect();
        match client::call_many_timed(&spec, batch, Duration::from_millis(opts.timeout_ms), opts.concurrency) {
            Ok(values) => {
                for ((idx, tool, arguments), timed) in pending.into_iter().zip(values) {
                    results[idx] = finish(&spec, &tool, &arguments, &opts, timed.result, timed.latency_ms);
                }
            }
            Err(message) => {
                for (idx, tool, _) in pending {
                    results[idx] = error_row(&spec, &tool, &message, 0, "miss", &opts.role);
                }
            }
        }
    }
    json!({"results": results})
}

struct Opts {
    timeout_ms: u64,
    retries: u32,
    cache_ttl_ms: u64,
    read_only: bool,
    max_cost: Option<f64>,
    concurrency: usize,
    role: String,
}

fn opts_of(body: &Value, spec: &ServerSpec) -> Opts {
    Opts {
        timeout_ms: body.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(spec.timeout_ms),
        retries: body.get("retries").and_then(|v| v.as_u64()).unwrap_or(spec.retries as u64) as u32,
        cache_ttl_ms: body.get("cache_ttl_ms").and_then(|v| v.as_u64()).unwrap_or(0),
        read_only: body.get("read_only").and_then(|v| v.as_bool()).unwrap_or(false),
        max_cost: body.get("max_cost").and_then(|v| v.as_f64()),
        concurrency: body.get("concurrency").and_then(|v| v.as_u64()).unwrap_or(4) as usize,
        role: body.get("role").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    }
}

fn spec_of(value: &Value) -> Result<ServerSpec, String> {
    serde_json::from_value(value.clone()).map_err(|e| format!("server spec: {e}"))
}

fn invoke(spec: &ServerSpec, tool: &str, arguments: Value, opts: &Opts) -> Value {
    if let Some(hit) = cache_get(spec, tool, &arguments, opts) {
        return hit;
    }
    if let Err(message) = cost_gate(opts) {
        return error_row(spec, tool, &message, 0, "miss", &opts.role);
    }
    let mut last = String::new();
    let started = Instant::now();
    for attempt in 0..=opts.retries {
        match client::call_tool(spec, tool, arguments.clone(), Duration::from_millis(opts.timeout_ms)) {
            Ok(value) => {
                let latency = started.elapsed().as_millis() as u64;
                return finish(spec, tool, &arguments, opts, Ok(value), latency);
            }
            Err(message) => {
                let tool_error = message.starts_with("mcp-tool:");
                last = message;
                if tool_error || attempt == opts.retries {
                    break;
                }
            }
        }
    }
    let latency = started.elapsed().as_millis() as u64;
    error_row(spec, tool, &last, latency, "miss", &opts.role)
}

fn finish(
    spec: &ServerSpec,
    tool: &str,
    arguments: &Value,
    opts: &Opts,
    value: Result<Value, String>,
    latency_ms: u64,
) -> Value {
    match value {
        Ok(value) => {
            if opts.read_only && opts.cache_ttl_ms > 0 {
                let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
                guard.cache.put(
                    cache_key(spec, tool, arguments),
                    value.to_string(),
                    Duration::from_millis(opts.cache_ttl_ms),
                    Instant::now(),
                );
            }
            ok_row(spec, tool, value, latency_ms, "miss", &opts.role)
        }
        Err(message) => error_row(spec, tool, &display_error(&message), latency_ms, "miss", &opts.role),
    }
}

fn cache_get(spec: &ServerSpec, tool: &str, arguments: &Value, opts: &Opts) -> Option<Value> {
    if !opts.read_only || opts.cache_ttl_ms == 0 {
        return None;
    }
    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
    let hit = guard.cache.get(&cache_key(spec, tool, arguments), Instant::now())?;
    drop(guard);
    let value = serde_json::from_str(&hit).unwrap_or(Value::String(hit));
    Some(ok_row(spec, tool, value, 0, "hit", &opts.role))
}

fn cache_key(spec: &ServerSpec, tool: &str, arguments: &Value) -> String {
    mcp_core::schema_hash(&format!("{}:{tool}", spec.name), arguments, &Value::Null)
}

fn display_error(message: &str) -> String {
    message.strip_prefix("mcp-tool:").unwrap_or(message).trim().to_string()
}

fn cost_gate(opts: &Opts) -> Result<(), String> {
    if opts.max_cost == Some(0.0) {
        Err("max_cost is 0".into())
    } else {
        Ok(())
    }
}

fn ok_row(spec: &ServerSpec, tool: &str, value: Value, latency_ms: u64, cache: &str, role: &str) -> Value {
    let meter = meter(spec, tool, latency_ms, cache, "ok", role);
    json!({
        "value": value,
        "error": null,
        "latency_ms": latency_ms,
        "cache": cache,
        "status": "ok",
        "calls": [meter],
    })
}

fn error_row(spec: &ServerSpec, tool: &str, message: &str, latency_ms: u64, cache: &str, role: &str) -> Value {
    let meter = meter(spec, tool, latency_ms, cache, "error", role);
    json!({
        "value": null,
        "error": message,
        "latency_ms": latency_ms,
        "cache": cache,
        "status": "error",
        "calls": [meter],
    })
}

fn meter(spec: &ServerSpec, tool: &str, latency_ms: u64, cache: &str, status: &str, role: &str) -> CallMeter {
    let row = CallMeter {
        op: "mcp".into(),
        model: format!("mcp:{}/{}", spec.name, tool),
        tokens_in: None,
        tokens_out: None,
        cost_usd: Some(0.0),
        cost_saved_usd: if cache == "hit" { Some(0.0) } else { None },
        provider_latency_ms: if cache == "hit" { None } else { Some(latency_ms) },
        latency_ms,
        cache: cache.into(),
        items: 1,
        tenant: role.to_string(),
        role: role.to_string(),
        timestamp: stamp(),
        generation_id: Some(status.into()),
        identical: None,
        node: Some(spec.name.clone()),
        score: None,
        payload: None,
        retake: None,
        server: Some(spec.name.clone()),
        tool: Some(tool.into()),
        status: Some(status.into()),
    };
    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
    if guard.calls.len() >= CALL_LOG_CAP {
        guard.calls.pop_front();
    }
    guard.calls.push_back(row.clone());
    row
}

fn stamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn write_json(conn: &mut TcpStream, status: u16, body: &Value) -> std::io::Result<()> {
    let bytes = body.to_string();
    let reason = if status == 200 { "OK" } else { "ERR" };
    write!(
        conn,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{bytes}",
        bytes.len()
    )
}
