//! Mock OIDC issuer in front of inbound OAuth: discovery, JWKS, PKCE,
//! RS256 id_token, email and hd mapping, domain allow-list, and a
//! write session. Password login stays read-only. Nothing here prints
//! the client secret, the upstream verifier, or an id_token.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ring::rand::SecureRandom;
use serde_json::{json, Value};

const SECRET: &str = "oidc-secret-9f3c1a";
const OIDC_CLIENT: &str = "rvbbit-oidc-test";
const REDIRECT: &str = "http://127.0.0.1:9/cb";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hutch-mcp")
}

fn pg_bin() -> String {
    std::env::var("PG_BIN").unwrap_or_else(|_| "/usr/lib/postgresql/18/bin".into())
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn refuse(port: u16) {
    for blocked in [55432, 8099, 55461, 8112, 55463, 8114, 55465, 8116] {
        assert_ne!(port, blocked, "refusing reserved port {port}");
    }
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().redirects(0).timeout(Duration::from_secs(15)).build()
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
        Err(err) => panic!("{method} {}: {err}", redact(url)),
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

fn location_of(headers: &[(String, String)]) -> String {
    headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("location")).map(|(_, v)| v.clone()).unwrap_or_default()
}

fn redact(text: &str) -> String {
    if text.contains("eyJ") || text.contains(SECRET) {
        return "<redacted>".into();
    }
    text.chars().take(400).collect()
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

fn b64url(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | data[i + 2] as u32;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(T[((n >> 6) & 63) as usize] as char);
        out.push(T[(n & 63) as usize] as char);
        i += 3;
    }
    let rest = data.len() - i;
    if rest == 1 {
        let n = (data[i] as u32) << 16;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
    } else if rest == 2 {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(T[((n >> 6) & 63) as usize] as char);
    }
    out
}

fn pct_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = &s[i + 1..i + 3];
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn form_pairs(s: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in s.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(pct_decode(k), pct_decode(v));
    }
    map
}

fn s256(verifier: &str) -> String {
    b64url(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()).as_ref())
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Clone)]
struct Mint {
    email: &'static str,
    hd: Option<&'static str>,
    verified: bool,
    bad_nonce: bool,
    alg_none: bool,
}

struct Pending {
    nonce: String,
    challenge: String,
    redirect: String,
    mint: Mint,
}

struct Issuer {
    port: u16,
    next: Arc<Mutex<Option<Mint>>>,
}

impl Issuer {
    fn start() -> Self {
        let dir = std::env::temp_dir().join(format!("rvbbit-oidc-issuer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pem = dir.join("key.pem");
        let pkcs8 = dir.join("key.pk8");
        let gen = Command::new("openssl")
            .args(["genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out"])
            .arg(&pem)
            .output()
            .expect("openssl");
        assert!(gen.status.success(), "{}", String::from_utf8_lossy(&gen.stderr));
        let conv = Command::new("openssl")
            .args(["pkcs8", "-topk8", "-nocrypt", "-outform", "DER", "-in"])
            .arg(&pem)
            .arg("-out")
            .arg(&pkcs8)
            .output()
            .expect("openssl pkcs8");
        assert!(conv.status.success(), "{}", String::from_utf8_lossy(&conv.stderr));
        let pair = ring::signature::RsaKeyPair::from_pkcs8(&std::fs::read(&pkcs8).unwrap()).unwrap();
        let public = ring::signature::RsaPublicKeyComponents::<Vec<u8>>::from(pair.public());
        let n = b64url(&public.n);
        let e = b64url(&public.e);
        let pair = Arc::new(pair);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        refuse(port);
        let issuer = format!("http://127.0.0.1:{port}");
        let next = Arc::new(Mutex::new(None));
        let pending = Arc::new(Mutex::new(HashMap::<String, Pending>::new()));
        let next_t = next.clone();
        let pending_t = pending.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some((method, target, body)) = read_http(&mut stream) else { continue };
                let path = target.split('?').next().unwrap_or("/");
                let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
                match (method.as_str(), path) {
                    ("GET", "/.well-known/openid-configuration") => {
                        let doc = json!({
                            "issuer": issuer,
                            "authorization_endpoint": format!("{issuer}/authorize"),
                            "token_endpoint": format!("{issuer}/token"),
                            "jwks_uri": format!("{issuer}/jwks"),
                        });
                        write_http(&mut stream, 200, "OK", "application/json", &doc.to_string(), None);
                    }
                    ("GET", "/jwks") => {
                        let doc = json!({"keys":[{"kty":"RSA","kid":"test","alg":"RS256","n":n,"e":e}]});
                        write_http(&mut stream, 200, "OK", "application/json", &doc.to_string(), None);
                    }
                    ("GET", "/authorize") => {
                        let q = form_pairs(query);
                        let mint = next_t.lock().unwrap().clone();
                        let Some(mint) = mint else {
                            write_http(&mut stream, 400, "Bad Request", "text/plain", "no profile", None);
                            continue;
                        };
                        let code = b64url(&random_bytes(18));
                        pending_t.lock().unwrap().insert(
                            code.clone(),
                            Pending {
                                nonce: q.get("nonce").cloned().unwrap_or_default(),
                                challenge: q.get("code_challenge").cloned().unwrap_or_default(),
                                redirect: q.get("redirect_uri").cloned().unwrap_or_default(),
                                mint,
                            },
                        );
                        let redirect = q.get("redirect_uri").cloned().unwrap_or_default();
                        let state = q.get("state").cloned().unwrap_or_default();
                        let sep = if redirect.contains('?') { "&" } else { "?" };
                        let loc = format!("{redirect}{sep}code={code}&state={state}");
                        write_http(&mut stream, 302, "Found", "text/plain", "", Some(&loc));
                    }
                    ("POST", "/token") => {
                        let form = form_pairs(&String::from_utf8_lossy(&body));
                        if form.get("client_id").map(String::as_str) != Some(OIDC_CLIENT)
                            || form.get("client_secret").map(String::as_str) != Some(SECRET)
                        {
                            write_http(&mut stream, 400, "Bad Request", "application/json", r#"{"error":"invalid_client"}"#, None);
                            continue;
                        }
                        let code = form.get("code").cloned().unwrap_or_default();
                        let row = pending_t.lock().unwrap().remove(&code);
                        let Some(row) = row else {
                            write_http(&mut stream, 400, "Bad Request", "application/json", r#"{"error":"invalid_grant"}"#, None);
                            continue;
                        };
                        let verifier = form.get("code_verifier").cloned().unwrap_or_default();
                        let redirect = form.get("redirect_uri").cloned().unwrap_or_default();
                        if s256(&verifier) != row.challenge || redirect != row.redirect {
                            write_http(&mut stream, 400, "Bad Request", "application/json", r#"{"error":"invalid_grant"}"#, None);
                            continue;
                        }
                        let nonce = if row.mint.bad_nonce { "not-the-recorded-nonce" } else { row.nonce.as_str() };
                        let mut payload = json!({
                            "iss": issuer,
                            "aud": OIDC_CLIENT,
                            "exp": now_secs() + 600,
                            "iat": now_secs(),
                            "nonce": nonce,
                            "email": row.mint.email,
                            "email_verified": row.mint.verified,
                        });
                        if let Some(hd) = row.mint.hd {
                            payload["hd"] = json!(hd);
                        }
                        let token = if row.mint.alg_none {
                            let header = b64url(br#"{"alg":"none","typ":"JWT"}"#);
                            let body = b64url(payload.to_string().as_bytes());
                            format!("{header}.{body}.")
                        } else {
                            sign_rs(&pair, &payload.to_string())
                        };
                        let doc = json!({"id_token": token, "token_type": "Bearer", "expires_in": 3600});
                        write_http(&mut stream, 200, "OK", "application/json", &doc.to_string(), None);
                    }
                    _ => write_http(&mut stream, 404, "Not Found", "text/plain", "not found", None),
                }
            }
        });
        Self { port, next }
    }

    fn queue(&self, mint: Mint) {
        *self.next.lock().unwrap() = Some(mint);
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    ring::rand::SystemRandom::new().fill(&mut buf).expect("rng");
    buf
}

fn sign_rs(pair: &ring::signature::RsaKeyPair, payload: &str) -> String {
    let header = b64url(br#"{"alg":"RS256","kid":"test","typ":"JWT"}"#);
    let body = b64url(payload.as_bytes());
    let message = format!("{header}.{body}");
    let rng = ring::rand::SystemRandom::new();
    let mut sig = vec![0u8; pair.public().modulus_len()];
    pair.sign(&ring::signature::RSA_PKCS1_SHA256, &rng, message.as_bytes(), &mut sig).expect("sign");
    format!("{message}.{}", b64url(&sig))
}

fn read_http(stream: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        if buf.len() > 256 * 1024 {
            return None;
        }
    }
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request = lines.next()?.to_string();
    let mut content_length = 0usize;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        if k.trim().eq_ignore_ascii_case("content-length") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);
    let mut parts = request.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    Some((method, target, body))
}

fn write_http(stream: &mut TcpStream, status: u16, reason: &str, content_type: &str, body: &str, location: Option<&str>) {
    let loc = location.map(|v| format!("Location: {v}\r\n")).unwrap_or_default();
    let bytes = body.as_bytes();
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{loc}\r\n",
        bytes.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(bytes);
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

#[test]
fn oidc_mock_maps_roles_and_password_stays_read_only() {
    let issuer = Issuer::start();
    let pg_port = free_port();
    let http_port = free_port();
    refuse(pg_port);
    refuse(http_port);
    let data = format!("/tmp/rvbbit-oidc-{pg_port}");
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
    let log_path = format!("/tmp/rvbbit-oidc-{pg_port}.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let postgres = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "ulimit -s 65520; exec \"{bin}\" -D \"{data}\" -k /tmp -p {pg_port} -c listen_addresses=127.0.0.1 -c io_method=sync -c max_stack_depth=60000",
            bin = format!(
                "{}/postgres",
                std::env::var("CARGO_MANIFEST_DIR").unwrap().replace("/crates/rvbbit/hutch_mcp", "/target/fast-profile")
            ),
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

    psql(
        pg_port,
        "\
CREATE ROLE rvbbit_auth LOGIN NOSUPERUSER NOINHERIT;
CREATE ROLE analyst LOGIN PASSWORD 'analyst-secret' NOSUPERUSER;
CREATE ROLE intern LOGIN PASSWORD 'intern-secret' NOSUPERUSER;
CREATE ROLE writer LOGIN PASSWORD 'writer-secret' NOSUPERUSER;
GRANT analyst, intern, writer TO rvbbit_auth;",
    );
    let boot = format!("/tmp/rvbbit-oidc-{pg_port}.sql");
    std::fs::write(&boot, include_str!("../sql/bootstrap.sql")).unwrap();
    let loaded = Command::new(format!("{}/psql", pg_bin()))
        .args(["-h", "/tmp", "-p", &pg_port.to_string(), "-U", "postgres", "-v", "ON_ERROR_STOP=1", "-f", &boot])
        .env("PGRUST_PGSHAREDIR", "/usr/share/postgresql/18")
        .output()
        .unwrap();
    assert!(loaded.status.success(), "{}", String::from_utf8_lossy(&loaded.stderr));
    psql(
        pg_port,
        "\
INSERT INTO rvbbit.users (role, max_cost) VALUES ('analyst', NULL), ('intern', NULL), ('writer', NULL);
INSERT INTO rvbbit.oidc_role (claim, value, role, read_only) VALUES
  ('email', 'ada@example.com', 'analyst', true),
  ('email', 'writer@example.com', 'writer', false),
  ('hd', 'example.com', 'intern', true);
CREATE TABLE notes (owner text, body text);
ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
ALTER TABLE notes FORCE ROW LEVEL SECURITY;
CREATE POLICY own ON notes USING (owner = current_user);
GRANT SELECT ON notes TO analyst, intern, writer;
GRANT INSERT ON notes TO writer;
INSERT INTO notes (owner, body) VALUES ('analyst', 'a-secret'), ('intern', 'i-secret');",
    );

    let base = format!("http://127.0.0.1:{http_port}");
    let hutch_log = format!("/tmp/rvbbit-oidc-hutch-{http_port}.log");
    let hutch_file = std::fs::File::create(&hutch_log).unwrap();
    let server = Command::new(bin())
        .args(["--inbound", &format!("127.0.0.1:{http_port}")])
        .env("RVBBIT_PG_CONNINFO", format!("host=/tmp port={pg_port} dbname=postgres user=rvbbit_auth"))
        .env("RVBBIT_PG_PASSWORD_HOST", "127.0.0.1")
        .env("RVBBIT_PG_PASSWORD_PORT", pg_port.to_string())
        .env("RVBBIT_PG_PASSWORD_DB", "postgres")
        .env("RVBBIT_MCP_JWT_SECRET", "inbound-test-secret")
        .env("RVBBIT_MCP_PUBLIC_URL", &base)
        .env("RVBBIT_OIDC_ISSUER", issuer.base())
        .env("RVBBIT_OIDC_CLIENT_ID", OIDC_CLIENT)
        .env("RVBBIT_OIDC_CLIENT_SECRET", SECRET)
        .env("RVBBIT_OIDC_DOMAINS", "example.com")
        .stdout(Stdio::from(hutch_file.try_clone().unwrap()))
        .stderr(Stdio::from(hutch_file))
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

    let (status, reg, _) = call(
        &http,
        "POST",
        &format!("{base}/register"),
        "application/json",
        &json!({"redirect_uris":[REDIRECT],"client_name":"oidc-test"}).to_string(),
    );
    assert_eq!(status, 201, "{}", redact(&reg));
    let client_id = serde_json::from_str::<Value>(&reg).unwrap()["client_id"].as_str().unwrap().to_string();

    let auth = format!(
        "{base}/authorize?response_type=code&client_id={}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256&state=page",
        url_encode(&client_id),
        url_encode(REDIRECT),
    );
    let (status, page, _) = call(&http, "GET", &auth, "application/json", "");
    assert_eq!(status, 200, "{}", redact(&page));
    assert!(page.contains("Sign in with OIDC"), "{}", redact(&page));
    assert!(page.contains("name=\"password\""), "password form missing");
    assert!(page.contains("name=\"username\""), "username field missing");

    let analyst = password_bearer(&http, &base, &client_id, "analyst", "analyst-secret");
    let analyst_session = open_session(&http, &base, &analyst, "analyst-pw");
    let ro = run_sql(&http, &base, &analyst, analyst_session.as_deref(), "SELECT current_setting('transaction_read_only')");
    assert!(tool_text(&ro).contains("\"on\""), "{}", redact(&tool_text(&ro)));
    let notes = run_sql(&http, &base, &analyst, analyst_session.as_deref(), "SELECT body FROM notes");
    let note_text = tool_text(&notes);
    assert!(note_text.contains("a-secret"), "{}", redact(&note_text));
    assert!(!note_text.contains("i-secret"), "{}", redact(&note_text));
    assert!(is_tool_error(&run_sql(
        &http,
        &base,
        &analyst,
        analyst_session.as_deref(),
        "INSERT INTO notes (owner, body) VALUES ('analyst', 'nope')"
    )));

    let writer_pw = password_bearer(&http, &base, &client_id, "writer", "writer-secret");
    let writer_pw_session = open_session(&http, &base, &writer_pw, "writer-pw");
    let writer_ro = run_sql(
        &http,
        &base,
        &writer_pw,
        writer_pw_session.as_deref(),
        "SELECT current_setting('transaction_read_only')",
    );
    assert!(tool_text(&writer_ro).contains("\"on\""), "password login stays read-only: {}", redact(&tool_text(&writer_ro)));
    assert!(is_tool_error(&run_sql(
        &http,
        &base,
        &writer_pw,
        writer_pw_session.as_deref(),
        "INSERT INTO notes (owner, body) VALUES ('writer', 'pw')"
    )));

    let ada = oidc_bearer(
        &http,
        &base,
        &issuer,
        &client_id,
        Mint { email: "ada@example.com", hd: Some("example.com"), verified: true, bad_nonce: false, alg_none: false },
        "ada-state",
    );
    let ada_session = open_session(&http, &base, &ada, "ada");
    let ada_ro = run_sql(&http, &base, &ada, ada_session.as_deref(), "SELECT current_setting('transaction_read_only')");
    assert!(tool_text(&ada_ro).contains("\"on\""), "{}", redact(&tool_text(&ada_ro)));
    let ada_notes = tool_text(&run_sql(&http, &base, &ada, ada_session.as_deref(), "SELECT body FROM notes"));
    assert!(ada_notes.contains("a-secret") && !ada_notes.contains("i-secret"), "{}", redact(&ada_notes));
    assert!(is_tool_error(&run_sql(
        &http,
        &base,
        &ada,
        ada_session.as_deref(),
        "INSERT INTO notes (owner, body) VALUES ('analyst', 'oidc')"
    )));

    let peer = oidc_bearer(
        &http,
        &base,
        &issuer,
        &client_id,
        Mint { email: "peer@example.com", hd: Some("example.com"), verified: true, bad_nonce: false, alg_none: false },
        "hd-state",
    );
    let peer_session = open_session(&http, &base, &peer, "peer");
    let peer_notes = tool_text(&run_sql(&http, &base, &peer, peer_session.as_deref(), "SELECT body FROM notes"));
    assert!(peer_notes.contains("i-secret") && !peer_notes.contains("a-secret"), "{}", redact(&peer_notes));

    let stranger = oidc_error(
        &http,
        &base,
        &issuer,
        &client_id,
        Mint { email: "stranger@example.com", hd: None, verified: true, bad_nonce: false, alg_none: false },
        "stranger-state",
    );
    assert!(stranger.contains("no role mapping"), "{}", redact(&stranger));
    assert!(!stranger.contains("stranger@"), "{}", redact(&stranger));

    let foreign = oidc_error(
        &http,
        &base,
        &issuer,
        &client_id,
        Mint { email: "foreign@other.test", hd: None, verified: true, bad_nonce: false, alg_none: false },
        "foreign-state",
    );
    assert!(foreign.contains("not allowed"), "{}", redact(&foreign));
    assert!(!foreign.contains("foreign@"), "{}", redact(&foreign));

    let unverified = oidc_error(
        &http,
        &base,
        &issuer,
        &client_id,
        Mint { email: "ada@example.com", hd: Some("example.com"), verified: false, bad_nonce: false, alg_none: false },
        "unverified-state",
    );
    assert!(unverified.contains("not verified"), "{}", redact(&unverified));

    let bad_nonce = oidc_error(
        &http,
        &base,
        &issuer,
        &client_id,
        Mint { email: "ada@example.com", hd: Some("example.com"), verified: true, bad_nonce: true, alg_none: false },
        "nonce-state",
    );
    assert!(bad_nonce.contains("nonce"), "{}", redact(&bad_nonce));
    assert!(!bad_nonce.contains("eyJ"), "callback leaked an id_token");

    let alg_none = oidc_error(
        &http,
        &base,
        &issuer,
        &client_id,
        Mint { email: "ada@example.com", hd: Some("example.com"), verified: true, bad_nonce: false, alg_none: true },
        "none-state",
    );
    assert!(alg_none.contains("RS256"), "{}", redact(&alg_none));

    let writer = oidc_bearer(
        &http,
        &base,
        &issuer,
        &client_id,
        Mint { email: "writer@example.com", hd: Some("example.com"), verified: true, bad_nonce: false, alg_none: false },
        "writer-state",
    );
    let writer_session = open_session(&http, &base, &writer, "writer");
    let writer_flag = run_sql(
        &http,
        &base,
        &writer,
        writer_session.as_deref(),
        "SELECT current_setting('transaction_read_only')",
    );
    assert!(tool_text(&writer_flag).contains("\"off\""), "{}", redact(&tool_text(&writer_flag)));
    let inserted = run_sql(
        &http,
        &base,
        &writer,
        writer_session.as_deref(),
        "INSERT INTO notes (owner, body) VALUES ('writer', 'w-secret')",
    );
    assert!(!is_tool_error(&inserted), "{}", redact(&tool_text(&inserted)));
    let wrote = tool_text(&run_sql(&http, &base, &writer, writer_session.as_deref(), "SELECT body FROM notes"));
    assert!(wrote.contains("w-secret") && !wrote.contains("a-secret"), "{}", redact(&wrote));
    let set_role = run_sql(&http, &base, &writer, writer_session.as_deref(), "SET ROLE postgres");
    assert!(is_tool_error(&set_role), "{}", redact(&tool_text(&set_role)));
    assert!(tool_text(&set_role).contains("SET ROLE"), "{}", redact(&tool_text(&set_role)));
    let set_session = run_sql(
        &http,
        &base,
        &writer,
        writer_session.as_deref(),
        "SET SESSION AUTHORIZATION postgres",
    );
    assert!(tool_text(&set_session).contains("SET ROLE"), "{}", redact(&tool_text(&set_session)));

    let ada_again = tool_text(&run_sql(&http, &base, &ada, ada_session.as_deref(), "SELECT body FROM notes"));
    assert!(ada_again.contains("a-secret") && !ada_again.contains("w-secret"), "{}", redact(&ada_again));

    let subjects = psql(pg_port, "SELECT string_agg(DISTINCT subject, ',') FROM rvbbit.mcp_audit");
    assert!(subjects.contains("ada@example.com"), "{subjects}");
    assert!(subjects.contains("analyst"), "{subjects}");
    assert!(!subjects.contains(SECRET), "audit stored the client secret");

    let key = Command::new(bin())
        .args(["--issue-key", "writer", "claude"])
        .env("RVBBIT_PG_CONNINFO", format!("host=/tmp port={pg_port} dbname=postgres user=rvbbit_auth"))
        .env("RVBBIT_MCP_JWT_SECRET", "inbound-test-secret")
        .output()
        .unwrap();
    assert!(key.status.success(), "{}", String::from_utf8_lossy(&key.stderr));
    let api_key = String::from_utf8_lossy(&key.stdout).trim().to_string();
    assert!(api_key.starts_with("rvb_"), "issue-key did not print a key");
    let key_session = open_session(&http, &base, &api_key, "key");
    assert!(is_tool_error(&run_sql(
        &http,
        &base,
        &api_key,
        key_session.as_deref(),
        "INSERT INTO notes (owner, body) VALUES ('writer', 'key')"
    )));

    drop(server);
    let logged = std::fs::read_to_string(&hutch_log).unwrap_or_default();
    assert!(!logged.contains(SECRET), "server log contains the OIDC client secret");
    assert!(!logged.contains("id_token"), "server log contains an id_token");
    drop(postgres);
    let _ = std::fs::remove_dir_all(&data);
}

fn password_bearer(agent: &ureq::Agent, base: &str, client_id: &str, user: &str, password: &str) -> String {
    let form = format!(
        "client_id={}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256&state=pw&username={user}&password={password}",
        url_encode(client_id),
        url_encode(REDIRECT),
    );
    let (status, body, headers) = call(agent, "POST", &format!("{base}/login"), "application/x-www-form-urlencoded", &form);
    assert_eq!(status, 302, "{}", redact(&body));
    exchange(agent, base, client_id, &location_of(&headers))
}

fn oidc_bearer(agent: &ureq::Agent, base: &str, issuer: &Issuer, client_id: &str, mint: Mint, state: &str) -> String {
    let (status, body, location) = oidc_round(agent, base, issuer, client_id, mint, state);
    assert_eq!(status, 302, "{}", redact(&body));
    assert!(location.contains(&format!("state={state}")), "{}", redact(&location));
    exchange(agent, base, client_id, &location)
}

fn oidc_error(agent: &ureq::Agent, base: &str, issuer: &Issuer, client_id: &str, mint: Mint, state: &str) -> String {
    let (status, body, location) = oidc_round(agent, base, issuer, client_id, mint, state);
    assert_eq!(status, 400, "expected denial, location {}", redact(&location));
    assert!(!body.contains(SECRET), "error body contains the client secret");
    assert!(!body.contains("eyJ"), "error body contains a jwt");
    body
}

fn oidc_round(agent: &ureq::Agent, base: &str, issuer: &Issuer, client_id: &str, mint: Mint, state: &str) -> (u16, String, String) {
    issuer.queue(mint);
    let start = format!(
        "{base}/oidc/start?response_type=code&client_id={}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256&state={state}",
        url_encode(client_id),
        url_encode(REDIRECT),
    );
    let (status, body, headers) = call(agent, "GET", &start, "application/json", "");
    assert_eq!(status, 302, "oidc start: {}", redact(&body));
    let idp = location_of(&headers);
    assert!(idp.starts_with(&issuer.base()), "authorize redirect left the mock issuer: {}", redact(&idp));
    assert!(!idp.contains(SECRET), "authorize redirect contains the client secret");
    assert!(idp.contains("code_challenge="), "authorize redirect missing PKCE");
    assert!(idp.contains("nonce="), "authorize redirect missing nonce");
    let (status, body, headers) = call(agent, "GET", &idp, "application/json", "");
    assert_eq!(status, 302, "issuer authorize: {}", redact(&body));
    let callback = location_of(&headers);
    let (status, body, headers) = call(agent, "GET", &callback, "application/json", "");
    (status, body, location_of(&headers))
}

fn exchange(agent: &ureq::Agent, base: &str, client_id: &str, location: &str) -> String {
    let code = location.split("code=").nth(1).unwrap_or("").split('&').next().unwrap_or("");
    assert!(!code.is_empty(), "{}", redact(location));
    let token_body = format!(
        "grant_type=authorization_code&client_id={}&redirect_uri={}&code={}&code_verifier={VERIFIER}",
        url_encode(client_id),
        url_encode(REDIRECT),
        url_encode(code),
    );
    let (status, tokens, _) = call(agent, "POST", &format!("{base}/token"), "application/x-www-form-urlencoded", &token_body);
    assert_eq!(status, 200, "{}", redact(&tokens));
    let tokens: Value = serde_json::from_str(&tokens).unwrap();
    let access = tokens["access_token"].as_str().unwrap().to_string();
    assert_eq!(access.matches('.').count(), 2, "access token is not a jwt");
    access
}

fn open_session(agent: &ureq::Agent, base: &str, token: &str, name: &str) -> Option<String> {
    let (status, init, session) = mcp(
        agent,
        base,
        token,
        None,
        1,
        "initialize",
        json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":name,"version":"0"}}),
    );
    assert_eq!(status, 200, "{}", redact(&init.to_string()));
    session
}

fn run_sql(agent: &ureq::Agent, base: &str, token: &str, session: Option<&str>, sql: &str) -> Value {
    let (status, value, _) = mcp(
        agent,
        base,
        token,
        session,
        2,
        "tools/call",
        json!({"name":"run_sql","arguments":{"sql":sql}}),
    );
    assert_eq!(status, 200, "{}", redact(&value.to_string()));
    value
}

fn is_tool_error(value: &Value) -> bool {
    value["result"]["isError"] == true
}
