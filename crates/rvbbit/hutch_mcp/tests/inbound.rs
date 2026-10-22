//! OAuth 2.1 (PKCE + dynamic client registration), then the warehouse
//! tools on a real Postgres: role mapping, RLS, read-only, budget, audit.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hutch-mcp")
}

fn pg_bin() -> String {
    std::env::var("PG_BIN").unwrap_or_else(|_| "/usr/lib/postgresql/18/bin".into())
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

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().redirects(0).timeout(Duration::from_secs(10)).build()
}

fn call(agent: &ureq::Agent, method: &str, url: &str, content_type: &str, body: &str) -> (u16, String, Vec<(String, String)>) {
    let req = match method {
        "GET" => agent.get(url),
        "POST" => agent.post(url).set("content-type", content_type),
        other => panic!("method {other}"),
    };
    match req.send_string(body) {
        Ok(resp) => read_resp(resp),
        Err(ureq::Error::Status(_, resp)) => read_resp(resp),
        Err(err) => panic!("{method} {url}: {err}"),
    }
}

fn read_resp(resp: ureq::Response) -> (u16, String, Vec<(String, String)>) {
    let status = resp.status();
    let headers: Vec<(String, String)> = resp
        .headers_names()
        .into_iter()
        .map(|name| {
            let value = resp.header(&name).unwrap_or("").to_string();
            (name, value)
        })
        .collect();
    let mut body = String::new();
    resp.into_reader().read_to_string(&mut body).ok();
    (status, body, headers)
}

fn mcp(agent: &ureq::Agent, base: &str, token: &str, session: Option<&str>, id: i64, method: &str, params: Value) -> (u16, Value, Option<String>) {
    let mut req = agent
        .post(&format!("{base}/mcp"))
        .set("content-type", "application/json")
        .set("accept", "application/json, text/event-stream")
        .set("authorization", &format!("Bearer {token}"))
        .set("mcp-protocol-version", "2025-03-26");
    if let Some(session) = session {
        req = req.set("mcp-session-id", session);
    }
    let body = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string();
    let (status, text, headers) = match req.send_string(&body) {
        Ok(resp) => read_resp(resp),
        Err(ureq::Error::Status(_, resp)) => read_resp(resp),
        Err(err) => panic!("mcp {method}: {err}"),
    };
    let session = headers.into_iter().find(|(n, _)| n.eq_ignore_ascii_case("mcp-session-id")).map(|(_, v)| v);
    (status, parse_mcp_body(&text), session)
}

/// Streamable HTTP may answer with one JSON object or an SSE `data:` frame.
fn parse_mcp_body(text: &str) -> Value {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        if value.is_object() {
            return value;
        }
    }
    let mut last = None;
    for line in text.lines() {
        let Some(data) = line.trim().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data) {
            last = Some(value);
        }
    }
    last.unwrap_or_else(|| Value::String(text.to_string()))
}

fn tool_text(value: &Value) -> String {
    value["result"]["content"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item["text"].as_str())
        .unwrap_or("")
        .to_string()
}

fn psql(port: u16, sql: &str) -> String {
    let out = Command::new(format!("{}/psql", pg_bin()))
        .args(["-h", "/tmp", "-p", &port.to_string(), "-U", "postgres", "-v", "ON_ERROR_STOP=1", "-tA", "-c", sql])
        .env("PGRUST_PGSHAREDIR", "/usr/share/postgresql/18")
        .output()
        .expect("psql");
    if !out.status.success() {
        panic!("psql failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn oauth_pkce_maps_roles_and_audits() {
    let pg_port = free_port();
    let http_port = free_port();
    assert_ne!(pg_port, 55432);
    assert_ne!(http_port, 8099);
    let data = format!("/tmp/rvbbit-inbound-{pg_port}");
    let _ = std::fs::remove_dir_all(&data);
    let init = Command::new(format!("{}/initdb", pg_bin()))
        .args(["-D", &data, "--no-locale", "--encoding=UTF8", "--auth-local=trust", "--auth-host=trust", "-U", "postgres"])
        .env("PGRUST_PGSHAREDIR", "/usr/share/postgresql/18")
        .output()
        .expect("initdb");
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    std::fs::write(
        format!("{data}/pg_hba.conf"),
        "\
local all postgres trust
local all rvbbit_auth trust
host all postgres 127.0.0.1/32 trust
host all rvbbit_auth 127.0.0.1/32 trust
host all all 127.0.0.1/32 scram-sha-256
",
    )
    .unwrap();
    let log_path = format!("/tmp/rvbbit-inbound-{pg_port}.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let postgres = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "ulimit -s 65520; exec \"{bin}\" -D \"{data}\" -k /tmp -p {pg_port} -c listen_addresses=127.0.0.1 -c io_method=sync -c max_stack_depth=60000",
            bin = format!("{}/postgres", std::env::var("CARGO_MANIFEST_DIR").unwrap().replace("/crates/rvbbit/hutch_mcp", "/target/fast-profile")),
        ))
        .env("PGRUST_PGSHAREDIR", "/usr/share/postgresql/18")
        .env("PGRUST_TZDIR", "/usr/share/zoneinfo")
        .env("RUST_MIN_STACK", "33554432")
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .expect("postgres");
    let postgres = ChildGuard(postgres);
    let mut up = false;
    for _ in 0..80 {
        if Command::new(format!("{}/psql", pg_bin()))
            .args(["-h", "/tmp", "-p", &pg_port.to_string(), "-U", "postgres", "-c", "SELECT 1"])
            .env("PGRUST_PGSHAREDIR", "/usr/share/postgresql/18")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            up = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(up, "postgres did not start:\n{}", std::fs::read_to_string(&log_path).unwrap_or_default());

    psql(pg_port, "\
CREATE ROLE rvbbit_auth LOGIN NOSUPERUSER NOINHERIT;
CREATE ROLE analyst LOGIN PASSWORD 'analyst-secret' NOSUPERUSER;
CREATE ROLE intern LOGIN PASSWORD 'intern-secret' NOSUPERUSER;
CREATE ROLE broke LOGIN PASSWORD 'broke-secret' NOSUPERUSER;
GRANT analyst, intern, broke TO rvbbit_auth;");
    let bootstrap = include_str!("../sql/bootstrap.sql");
    let mut file = std::fs::File::create(format!("/tmp/rvbbit-inbound-{pg_port}.sql")).unwrap();
    file.write_all(bootstrap.as_bytes()).unwrap();
    let loaded = Command::new(format!("{}/psql", pg_bin()))
        .args(["-h", "/tmp", "-p", &pg_port.to_string(), "-U", "postgres", "-v", "ON_ERROR_STOP=1", "-f", &format!("/tmp/rvbbit-inbound-{pg_port}.sql")])
        .env("PGRUST_PGSHAREDIR", "/usr/share/postgresql/18")
        .output()
        .unwrap();
    assert!(loaded.status.success(), "{}", String::from_utf8_lossy(&loaded.stderr));
    psql(pg_port, "\
INSERT INTO rvbbit.users (role, max_cost) VALUES ('analyst', NULL), ('intern', NULL), ('broke', 0);
CREATE TABLE notes (owner text, body text);
ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
ALTER TABLE notes FORCE ROW LEVEL SECURITY;
CREATE POLICY own ON notes USING (owner = current_user);
GRANT SELECT ON notes TO analyst, intern, broke;
INSERT INTO notes (owner, body) VALUES ('analyst', 'a-secret'), ('intern', 'i-secret');");

    let base = format!("http://127.0.0.1:{http_port}");
    let server = Command::new(bin())
        .args(["--inbound", &format!("127.0.0.1:{http_port}")])
        .env("RVBBIT_PG_CONNINFO", format!("host=/tmp port={pg_port} dbname=postgres user=rvbbit_auth"))
        .env("RVBBIT_PG_PASSWORD_HOST", "127.0.0.1")
        .env("RVBBIT_PG_PASSWORD_PORT", pg_port.to_string())
        .env("RVBBIT_PG_PASSWORD_DB", "postgres")
        .env("RVBBIT_MCP_JWT_SECRET", "inbound-test-secret")
        .env("RVBBIT_MCP_PUBLIC_URL", &base)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("inbound");
    let server = ChildGuard(server);
    let http = agent();
    let mut healthy = false;
    for _ in 0..50 {
        if http.get(&format!("{base}/health")).call().is_ok() {
            healthy = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(healthy, "inbound server did not listen");

    let (status, body, headers) = call(&http, "GET", &format!("{base}/mcp"), "application/json", "");
    assert_eq!(status, 401, "{body}");
    assert!(headers.iter().any(|(n, v)| n.eq_ignore_ascii_case("www-authenticate") && v.contains("resource_metadata")), "{headers:?}");

    let (status, meta, _) = call(&http, "GET", &format!("{base}/.well-known/oauth-authorization-server"), "application/json", "");
    assert_eq!(status, 200, "{meta}");
    let meta: Value = serde_json::from_str(&meta).unwrap();
    assert!(meta["registration_endpoint"].as_str().unwrap().ends_with("/register"));
    assert!(meta["code_challenge_methods_supported"].as_array().unwrap().iter().any(|v| v == "S256"));

    let (status, reg, _) = call(
        &http,
        "POST",
        &format!("{base}/register"),
        "application/json",
        &json!({"redirect_uris":["http://127.0.0.1:9/cb"],"client_name":"test"}).to_string(),
    );
    assert_eq!(status, 201, "{reg}");
    let reg: Value = serde_json::from_str(&reg).unwrap();
    let client_id = reg["client_id"].as_str().unwrap();

    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    let form = format!(
        "client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state=xyz&username=analyst&password=analyst-secret",
        url_encode(client_id),
        url_encode("http://127.0.0.1:9/cb"),
        url_encode(challenge),
    );
    let (status, body, headers) = call(&http, "POST", &format!("{base}/login"), "application/x-www-form-urlencoded", &form);
    assert_eq!(status, 302, "{body}");
    let location = headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("location")).map(|(_, v)| v.clone()).unwrap_or_default();
    let code = location.split("code=").nth(1).unwrap_or("").split('&').next().unwrap_or("");
    assert!(!code.is_empty(), "{location}");
    assert!(location.contains("state=xyz"), "{location}");

    let token_body = format!(
        "grant_type=authorization_code&client_id={}&redirect_uri={}&code={}&code_verifier={verifier}",
        url_encode(client_id),
        url_encode("http://127.0.0.1:9/cb"),
        url_encode(code),
    );
    let (status, tokens, _) = call(&http, "POST", &format!("{base}/token"), "application/x-www-form-urlencoded", &token_body);
    assert_eq!(status, 200, "{tokens}");
    let tokens: Value = serde_json::from_str(&tokens).unwrap();
    let access = tokens["access_token"].as_str().unwrap();
    let refresh = tokens["refresh_token"].as_str().unwrap();
    assert_eq!(tokens["expires_in"], 3600);
    assert_eq!(access.matches('.').count(), 2, "{access}");

    let (status, init, session) = mcp(
        &http,
        &base,
        access,
        None,
        1,
        "initialize",
        json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"inbound-test","version":"0"}}),
    );
    assert_eq!(status, 200, "{init}");
    assert!(init["result"]["serverInfo"]["name"].is_string() || init["result"]["protocolVersion"].is_string(), "{init}");

    let (status, listed, _) = mcp(&http, &base, access, session.as_deref(), 2, "tools/list", json!({}));
    assert_eq!(status, 200, "{listed}");
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("{listed}"))
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for name in ["list_schemas", "list_tables", "search_data", "describe_table", "validate_sql", "explain", "run_sql", "semantic_ops_help", "list_semantic_functions"] {
        assert!(names.contains(&name), "{names:?}");
    }

    let (status, schemas, _) = mcp(&http, &base, access, session.as_deref(), 3, "tools/call", json!({"name":"list_schemas","arguments":{}}));
    assert_eq!(status, 200, "{schemas}");
    let schema_text = tool_text(&schemas);
    assert!(schema_text.contains("public"), "{schema_text}");
    assert!(schema_text.contains("rvbbit"), "{schema_text}");

    let (status, notes, _) = mcp(
        &http,
        &base,
        access,
        session.as_deref(),
        4,
        "tools/call",
        json!({"name":"run_sql","arguments":{"sql":"SELECT body FROM notes ORDER BY 1"}}),
    );
    assert_eq!(status, 200, "{notes}");
    let note_text = tool_text(&notes);
    assert!(note_text.contains("a-secret"), "{note_text}");
    assert!(!note_text.contains("i-secret"), "{note_text}");

    let (status, denied, _) = mcp(
        &http,
        &base,
        access,
        session.as_deref(),
        5,
        "tools/call",
        json!({"name":"run_sql","arguments":{"sql":"INSERT INTO notes (owner, body) VALUES ('analyst', 'nope')"}}),
    );
    assert_eq!(status, 200, "{denied}");
    let denied_text = serde_json::to_string(&denied).unwrap().to_ascii_lowercase();
    assert!(denied_text.contains("read-only") || denied_text.contains("read only"), "{denied}");
    assert_eq!(denied["result"]["isError"], true, "{denied}");
    assert!(denied.get("error").is_none(), "tool failure is an isError result, not a protocol error: {denied}");

    let (_, dropped, _) = mcp(
        &http,
        &base,
        access,
        session.as_deref(),
        8,
        "tools/call",
        json!({"name":"run_sql","arguments":{"sql":"DROP TABLE notes"}}),
    );
    let drop_text = tool_text(&dropped);
    assert_eq!(dropped["result"]["isError"], true, "{dropped}");
    assert!(drop_text.contains("only read-only SELECT/WITH/VALUES/EXPLAIN allowed"), "{dropped}");
    assert!(dropped.get("error").is_none(), "{dropped}");

    let (_, set_role, _) = mcp(
        &http,
        &base,
        access,
        session.as_deref(),
        9,
        "tools/call",
        json!({"name":"run_sql","arguments":{"sql":"SET ROLE postgres"}}),
    );
    assert_eq!(set_role["result"]["isError"], true, "{set_role}");
    assert!(tool_text(&set_role).contains("only read-only SELECT/WITH/VALUES/EXPLAIN allowed"), "{set_role}");

    let (status, plan, _) = mcp(
        &http,
        &base,
        access,
        session.as_deref(),
        6,
        "tools/call",
        json!({"name":"explain","arguments":{"sql":"SELECT body FROM notes"}}),
    );
    assert_eq!(status, 200, "{plan}");
    let plan_text = tool_text(&plan).to_ascii_lowercase();
    assert!(plan_text.contains("seq scan") || plan_text.contains("result"), "{plan_text}");

    let (status, help, _) = mcp(&http, &base, access, session.as_deref(), 7, "tools/call", json!({"name":"semantic_ops_help","arguments":{}}));
    assert!(tool_text(&help).contains("MEANS"), "{help}");
    let _ = status;

    let intern = bearer(&http, &base, client_id, "intern", "intern-secret");
    let (status, init, intern_session) = mcp(
        &http,
        &base,
        &intern,
        None,
        1,
        "initialize",
        json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"intern","version":"0"}}),
    );
    assert_eq!(status, 200, "{init}");
    let (_, intern_notes, _) = mcp(
        &http,
        &base,
        &intern,
        intern_session.as_deref(),
        2,
        "tools/call",
        json!({"name":"run_sql","arguments":{"sql":"SELECT body FROM notes"}}),
    );
    let intern_text = tool_text(&intern_notes);
    assert!(intern_text.contains("i-secret"), "{intern_text}");
    assert!(!intern_text.contains("a-secret"), "{intern_text}");

    let broke = bearer(&http, &base, client_id, "broke", "broke-secret");
    let (_, _, broke_session) = mcp(
        &http,
        &base,
        &broke,
        None,
        1,
        "initialize",
        json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"broke","version":"0"}}),
    );
    let (_, budget, _) = mcp(
        &http,
        &base,
        &broke,
        broke_session.as_deref(),
        2,
        "tools/call",
        json!({"name":"run_sql","arguments":{"sql":"SELECT 1"}}),
    );
    let budget_text = serde_json::to_string(&budget).unwrap();
    assert!(budget_text.to_ascii_lowercase().contains("budget"), "{budget}");

    let refresh_body = format!(
        "grant_type=refresh_token&client_id={}&refresh_token={}",
        url_encode(client_id),
        url_encode(refresh),
    );
    let (status, refreshed, _) = call(&http, "POST", &format!("{base}/token"), "application/x-www-form-urlencoded", &refresh_body);
    assert_eq!(status, 200, "{refreshed}");
    let refreshed_json: Value = serde_json::from_str(&refreshed).unwrap();
    let refreshed_access = refreshed_json["access_token"].as_str().unwrap();
    assert_ne!(refreshed_access, access, "refreshed access tokens must differ");
    assert_ne!(refreshed_json["refresh_token"].as_str().unwrap(), refresh);

    let key = Command::new(bin())
        .args(["--issue-key", "analyst", "claude"])
        .env("RVBBIT_PG_CONNINFO", format!("host=/tmp port={pg_port} dbname=postgres user=rvbbit_auth"))
        .env("RVBBIT_MCP_JWT_SECRET", "inbound-test-secret")
        .output()
        .unwrap();
    assert!(key.status.success(), "{}", String::from_utf8_lossy(&key.stderr));
    let api_key = String::from_utf8_lossy(&key.stdout).trim().to_string();
    assert!(api_key.starts_with("rvb_"), "{api_key}");
    let (_, init, key_session) = mcp(
        &http,
        &base,
        &api_key,
        None,
        1,
        "initialize",
        json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"key","version":"0"}}),
    );
    let (_, keyed, _) = mcp(
        &http,
        &base,
        &api_key,
        key_session.as_deref(),
        2,
        "tools/call",
        json!({"name":"list_schemas","arguments":{}}),
    );
    assert!(tool_text(&keyed).contains("public") || serde_json::to_string(&keyed).unwrap().contains("public"), "{keyed} {init}");

    let audit = psql(pg_port, "SELECT count(*) FROM rvbbit.mcp_audit");
    assert!(audit.parse::<i64>().unwrap_or(0) >= 5, "{audit}");
    let failed = psql(
        pg_port,
        "SELECT count(*) FROM rvbbit.mcp_audit WHERE status = 'error' AND error ILIKE '%read-only%'",
    );
    assert!(failed.parse::<i64>().unwrap_or(0) >= 1, "{failed}");
    let priced = psql(
        pg_port,
        "SELECT count(*) FROM rvbbit.mcp_audit WHERE tool = 'run_sql' AND status = 'ok' AND cost_usd IS NOT NULL AND semantic_calls IS NOT NULL",
    );
    assert!(priced.parse::<i64>().unwrap_or(0) >= 1, "{priced}");
    let subjects = psql(pg_port, "SELECT string_agg(DISTINCT subject, ',') FROM rvbbit.mcp_audit");
    assert!(subjects.contains("analyst"), "{subjects}");
    assert!(subjects.contains("intern"), "{subjects}");
    let hashed = psql(pg_port, "SELECT count(*) FROM rvbbit.oauth_code");
    assert_eq!(hashed, "0", "spent codes stay deleted");
    let refresh_rows = psql(pg_port, "SELECT count(*) FROM rvbbit.oauth_refresh");
    assert!(refresh_rows.parse::<i64>().unwrap_or(0) >= 1, "{refresh_rows}");

    drop(server);
    drop(postgres);
    let _ = std::fs::remove_dir_all(&data);
}

fn bearer(agent: &ureq::Agent, base: &str, client_id: &str, user: &str, password: &str) -> String {
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    let form = format!(
        "client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state=s&username={user}&password={password}",
        url_encode(client_id),
        url_encode("http://127.0.0.1:9/cb"),
        url_encode(challenge),
    );
    let (status, body, headers) = call(agent, "POST", &format!("{base}/login"), "application/x-www-form-urlencoded", &form);
    assert_eq!(status, 302, "{body}");
    let location = headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("location")).unwrap().1.clone();
    let code = location.split("code=").nth(1).unwrap().split('&').next().unwrap();
    let token_body = format!(
        "grant_type=authorization_code&client_id={}&redirect_uri={}&code={}&code_verifier={verifier}",
        url_encode(client_id),
        url_encode("http://127.0.0.1:9/cb"),
        url_encode(code),
    );
    let (status, tokens, _) = call(agent, "POST", &format!("{base}/token"), "application/x-www-form-urlencoded", &token_body);
    assert_eq!(status, 200, "{tokens}");
    serde_json::from_str::<Value>(&tokens).unwrap()["access_token"].as_str().unwrap().to_string()
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
