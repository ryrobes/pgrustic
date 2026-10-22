//! Streamable HTTP at `/mcp`, plus the OAuth 2.1 endpoints.
//!
//! Login is a Postgres password check for a role listed in `rvbbit.users`.
//! The browser form is only that check. OIDC can replace it later without
//! moving the token tables.

use std::sync::Arc;

use axum::extract::{Form, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::oauth::{Oauth, PasswordTarget};
use super::pg::Pool;
use super::tools::Warehouse;

#[derive(Clone)]
pub struct App {
    pub oauth: Arc<Oauth>,
    pub public_url: String,
}

#[derive(Clone, Debug)]
pub struct InboundConfig {
    pub bind: String,
    pub public_url: String,
    pub conninfo: String,
    pub jwt_secret: String,
    pub password: PasswordTarget,
    pub stdio_bearer: Option<String>,
}

impl InboundConfig {
    pub fn from_env(bind: String) -> Result<Self, String> {
        let conninfo = std::env::var("RVBBIT_PG_CONNINFO").map_err(|_| "RVBBIT_PG_CONNINFO is not set")?;
        let jwt_secret = std::env::var("RVBBIT_MCP_JWT_SECRET").map_err(|_| "RVBBIT_MCP_JWT_SECRET is not set")?;
        if jwt_secret.trim().is_empty() {
            return Err("RVBBIT_MCP_JWT_SECRET is empty".into());
        }
        let public_url = std::env::var("RVBBIT_MCP_PUBLIC_URL").unwrap_or_else(|_| format!("http://{bind}"));
        let password = PasswordTarget {
            host: std::env::var("RVBBIT_PG_PASSWORD_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: std::env::var("RVBBIT_PG_PASSWORD_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5432),
            dbname: std::env::var("RVBBIT_PG_PASSWORD_DB").unwrap_or_else(|_| "postgres".into()),
        };
        Ok(Self {
            bind,
            public_url: public_url.trim_end_matches('/').to_string(),
            conninfo,
            jwt_secret,
            password,
            stdio_bearer: std::env::var("RVBBIT_MCP_BEARER").ok(),
        })
    }
}

pub fn app_from(cfg: &InboundConfig) -> App {
    let oauth = Arc::new(Oauth {
        pool: Pool::new(cfg.conninfo.clone(), 4),
        jwt_secret: cfg.jwt_secret.as_bytes().to_vec(),
        password: cfg.password.clone(),
    });
    App {
        oauth,
        public_url: cfg.public_url.clone(),
    }
}

pub fn serve(cfg: InboundConfig) -> Result<(), String> {
    let app = app_from(&cfg);
    let router = router(app.clone(), cfg.stdio_bearer.clone());
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&cfg.bind).await.map_err(|e| e.to_string())?;
        axum::serve(listener, router).await.map_err(|e| e.to_string())
    })
}

pub fn serve_stdio(cfg: InboundConfig) -> Result<(), String> {
    let app = app_from(&cfg);
    let bearer = cfg
        .stdio_bearer
        .clone()
        .ok_or_else(|| "RVBBIT_MCP_BEARER is required for --inbound-stdio".to_string())?;
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let warehouse = Warehouse::new(app.oauth.clone(), Some(bearer));
        let transport = rmcp::transport::io::stdio_pipes();
        let running = warehouse.serve(transport).await.map_err(|e| e.to_string())?;
running.waiting().await.map(|_| ()).map_err(|e| e.to_string())
    })
}

pub fn issue_key(cfg: &InboundConfig, role: &str, name: &str) -> Result<String, String> {
    let app = app_from(cfg);
    app.oauth.issue_api_key(role, name)
}

fn router(app: App, stdio_bearer: Option<String>) -> Router {
    let oauth = app.oauth.clone();
    // Stateless JSON: each call carries its own bearer, so a second replica
    // does not need the in-memory MCP session. Clients that speak SSE still
    // accept application/json (the streamable HTTP spec requires both).
    let mcp = StreamableHttpService::new(
        move || Ok(Warehouse::new(oauth.clone(), stdio_bearer.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_sse_keep_alive(None),
    );
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/.well-known/oauth-authorization-server", get(metadata))
        .route("/.well-known/oauth-protected-resource", get(protected_resource))
        .route("/register", post(register))
        .route("/authorize", get(authorize))
        .route("/login", post(login))
        .route("/token", post(token))
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(app.clone(), gate))
        .with_state(app)
}

async fn gate(State(app): State<App>, req: axum::extract::Request, next: Next) -> Response {
    if !req.uri().path().starts_with("/mcp") {
        return next.run(req).await;
    }
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if app.oauth.identity_for_bearer(presented).is_err() {
        return unauthorized(&app);
    }
    next.run(req).await
}

fn unauthorized(app: &App) -> Response {
    let meta = format!("{}/.well-known/oauth-protected-resource", app.public_url);
    let mut response = (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    let value = format!("Bearer resource_metadata=\"{meta}\"");
    if let Ok(parsed) = value.parse() {
        response.headers_mut().insert(header::WWW_AUTHENTICATE, parsed);
    }
    response
}

async fn metadata(State(app): State<App>) -> Json<Value> {
    let base = &app.public_url;
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "registration_endpoint": format!("{base}/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none", "client_secret_post"],
    }))
}

async fn protected_resource(State(app): State<App>) -> Json<Value> {
    Json(json!({
        "resource": format!("{}/mcp", app.public_url),
        "authorization_servers": [app.public_url],
        "bearer_methods_supported": ["header"],
    }))
}

#[derive(Debug, Deserialize)]
struct RegisterBody {
    redirect_uris: Vec<String>,
    #[serde(default)]
    client_name: String,
}

async fn register(State(app): State<App>, Json(body): Json<RegisterBody>) -> Response {
    match app.oauth.register(&body.redirect_uris, &body.client_name) {
        Ok(value) => (StatusCode::CREATED, Json(value)).into_response(),
        Err(message) => (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct AuthorizeQuery {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    code_challenge_method: String,
    #[serde(default)]
    state: String,
}

async fn authorize(State(app): State<App>, Query(q): Query<AuthorizeQuery>) -> Response {
    if q.response_type != "code" || q.code_challenge_method != "S256" {
        return (StatusCode::BAD_REQUEST, "response_type=code and S256 are required").into_response();
    }
    match app.oauth.client_allows(&q.client_id, &q.redirect_uri) {
        Ok(true) => {}
        Ok(false) => return (StatusCode::BAD_REQUEST, "client or redirect_uri is not registered").into_response(),
        Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
    }
    Html(login_page(&q)).into_response()
}

fn login_page(q: &AuthorizeQuery) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>rvbbit</title></head>\
<body><h1>rvbbit</h1><p>Postgres role in rvbbit.users.</p>\
<form method=\"post\" action=\"/login\">\
<input type=\"hidden\" name=\"client_id\" value=\"{}\">\
<input type=\"hidden\" name=\"redirect_uri\" value=\"{}\">\
<input type=\"hidden\" name=\"code_challenge\" value=\"{}\">\
<input type=\"hidden\" name=\"code_challenge_method\" value=\"{}\">\
<input type=\"hidden\" name=\"state\" value=\"{}\">\
<label>Role <input name=\"username\" autocomplete=\"username\"></label>\
<label>Password <input name=\"password\" type=\"password\" autocomplete=\"current-password\"></label>\
<button type=\"submit\">Sign in</button></form></body></html>",
        html_escape(&q.client_id),
        html_escape(&q.redirect_uri),
        html_escape(&q.code_challenge),
        html_escape(&q.code_challenge_method),
        html_escape(&q.state),
    )
}

#[derive(Debug, Deserialize)]
struct LoginForm {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    code_challenge_method: String,
    #[serde(default)]
    state: String,
    username: String,
    password: String,
}

async fn login(State(app): State<App>, Form(form): Form<LoginForm>) -> Response {
    match app.oauth.login(
        &form.client_id,
        &form.redirect_uri,
        &form.code_challenge,
        &form.code_challenge_method,
        &form.username,
        &form.password,
    ) {
        Ok(code) => {
            let mut url = format!("{}?code={}", form.redirect_uri, urlencoding_escape(&code));
            if !form.state.is_empty() {
                url.push_str("&state=");
                url.push_str(&urlencoding_escape(&form.state));
            }
Response::builder()
                .status(StatusCode::FOUND)
                .header(header::LOCATION, url)
                .body(axum::body::Body::empty())
                .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "redirect").into_response())
                .into_response()
        }
        Err(message) => (StatusCode::UNAUTHORIZED, message).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct TokenForm {
    grant_type: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    code_verifier: String,
    #[serde(default)]
    refresh_token: String,
}

async fn token(State(app): State<App>, headers: HeaderMap, body: String) -> Response {
    let parsed = parse_token_body(&headers, &body);
    let Ok(form) = parsed else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_request"}))).into_response();
    };
    let result = if form.grant_type == "authorization_code" {
        app.oauth
            .exchange_code(&form.client_id, &form.redirect_uri, &form.code, &form.code_verifier)
    } else if form.grant_type == "refresh_token" {
        app.oauth.exchange_refresh(&form.client_id, &form.refresh_token)
    } else {
        Err("unsupported grant_type".into())
    };
    match result {
        Ok(value) => Json(value).into_response(),
        Err(message) => (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant", "error_description": message}))).into_response(),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn parse_token_body(headers: &HeaderMap, body: &str) -> Result<TokenForm, String> {
    let value = if headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .contains("application/json")
        || body.trim_start().starts_with('{')
    {
        serde_json::from_str::<Value>(body).map_err(|e| e.to_string())?
    } else {
        let mut map = serde_json::Map::new();
        for pair in body.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            map.insert(url_decode(k), Value::String(url_decode(v)));
        }
        Value::Object(map)
    };
    serde_json::from_value(value).map_err(|e| e.to_string())
}

fn url_decode(s: &str) -> String {
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

fn urlencoding_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
