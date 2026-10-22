//! Mock MCP server over stdio and streamable HTTP, then the same tools
//! through the Postgres-facing gateway.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use mcp_core::ServerSpec;
use serde_json::{json, Value};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hutch-mcp")
}

fn stdio_spec() -> ServerSpec {
    ServerSpec {
        name: "demo".into(),
        transport: "stdio".into(),
        url: None,
        command: Some(bin().into()),
        args: vec!["--mock-stdio".into()],
        headers: Default::default(),
        env: Default::default(),
        timeout_ms: 10_000,
        retries: 0,
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(args: &[&str]) -> ChildGuard {
    let child = Command::new(bin())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    ChildGuard(child)
}

fn wait_port(port: u16) {
    let addr = format!("127.0.0.1:{port}");
    for _ in 0..50 {
        if std::net::TcpStream::connect(&addr).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("nothing listened on {addr}");
}

#[test]
fn stdio_lists_echo_and_rejects_fail() {
    let spec = stdio_spec();
    let tools = hutch_mcp::list_tools(&spec).unwrap();
    let echo = tools.iter().find(|t| t.name == "echo").expect("echo");
    assert!(echo.input_schema["properties"]["text"]["type"] == "string");
    let people = tools.iter().find(|t| t.name == "people").expect("people");
    assert_eq!(people.output_schema["type"], "array");
    let value = hutch_mcp::call_tool(&spec, "echo", json!({"text": "Ken Lay"}), Duration::from_secs(10)).unwrap();
    assert_eq!(value, json!("Ken Lay"));
    let err = hutch_mcp::call_tool(&spec, "fail", json!({}), Duration::from_secs(10)).unwrap_err();
    assert!(err.to_lowercase().contains("fail"), "{err}");
}

#[test]
fn http_lists_and_calls_people() {
    let port = free_port();
    let _child = spawn(&["--mock-http", &format!("127.0.0.1:{port}")]);
    wait_port(port);
    let spec = ServerSpec {
        name: "http-demo".into(),
        transport: "http".into(),
        url: Some(format!("http://127.0.0.1:{port}/mcp")),
        command: None,
        args: Vec::new(),
        headers: Default::default(),
        env: Default::default(),
        timeout_ms: 10_000,
        retries: 0,
    };
    let tools = hutch_mcp::list_tools(&spec).unwrap();
    assert!(tools.iter().any(|t| t.name == "now"));
    let rows = hutch_mcp::call_tool(&spec, "people", json!({}), Duration::from_secs(10)).unwrap();
    let rows = rows.as_array().unwrap_or_else(|| panic!("people: {rows}"));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["name"], "Ada");
}

#[test]
fn gateway_caches_read_only_calls_and_logs_them() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let listen = addr.clone();
    thread::spawn(move || {
        let _ = hutch_mcp::serve(&listen);
    });
    wait_port(port);
    let spec = stdio_spec();
    let body = json!({
        "server": spec,
        "tool": "echo",
        "arguments": {"text": "Ada"},
        "read_only": true,
        "cache_ttl_ms": 60_000,
        "timeout_ms": 10_000,
        "retries": 0
    });
    let first = post(&addr, "/v1/mcp/call", &body);
    assert_eq!(first["status"], "ok", "{first}");
    assert_eq!(first["cache"], "miss");
    assert_eq!(first["value"], "Ada");
    let second = post(&addr, "/v1/mcp/call", &body);
    assert_eq!(second["cache"], "hit", "{second}");
    assert_eq!(second["value"], "Ada");
    let calls = get(&addr, "/v1/calls?limit=10");
    let rows = calls["calls"].as_array().unwrap();
    assert!(rows.iter().any(|row| row["op"] == "mcp" && row["tool"] == "echo" && row["server"] == "demo"));
    let blocked = post(
        &addr,
        "/v1/mcp/call",
        &json!({
            "server": spec,
            "tool": "echo",
            "arguments": {"text": "x"},
            "max_cost": 0
        }),
    );
    assert_eq!(blocked["status"], "error");
    assert!(blocked["error"].as_str().unwrap_or("").contains("max_cost"));
}

fn post(addr: &str, path: &str, body: &Value) -> Value {
    let bytes = body.to_string();
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{bytes}",
        bytes.len()
    )
    .unwrap();
    read_json(stream)
}

fn get(addr: &str, path: &str) -> Value {
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").unwrap();
    read_json(stream)
}

fn read_json(mut stream: std::net::TcpStream) -> Value {
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
    serde_json::from_str(body).unwrap_or_else(|e| panic!("body {body}: {e}"))
}
