//! OAuth 2.1 authorization server backed by Postgres.
//!
//! Clients, authorization codes, and refresh tokens are hashed rows, so
//! any replica with the same database can redeem them. Access tokens are
//! JWTs: role, credential version, and a one-hour expiry. An API key is
//! a bearer that never looks like a JWT; only its hash is stored.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use pg_sha2::{PgSha256Ctx, PG_SHA256_DIGEST_LENGTH};
use serde_json::{json, Value};

use super::pg::{conninfo_escape, password_ok, quote_lit, Pool, Rows};

const ACCESS_TTL_SECS: u64 = 3600;
const CODE_TTL_SECS: u64 = 600;
const REFRESH_TTL_SECS: u64 = 30 * 24 * 3600;

#[derive(Clone, Debug)]
pub struct Identity {
    pub subject: String,
    pub role: String,
    pub max_cost: Option<f64>,
    pub statement_timeout_ms: u64,
    pub credential_version: i32,
}

#[derive(Clone, Debug)]
pub struct PasswordTarget {
    pub host: String,
    pub port: u16,
    pub dbname: String,
}

pub struct Oauth {
    pub pool: Pool,
    pub jwt_secret: Vec<u8>,
    pub password: PasswordTarget,
}

impl Oauth {
    pub fn register(&self, redirect_uris: &[String], client_name: &str) -> Result<Value, String> {
        if redirect_uris.is_empty() {
            return Err("redirect_uris is required".into());
        }
        for uri in redirect_uris {
            if !uri.starts_with("http://") && !uri.starts_with("https://") {
                return Err(format!("redirect_uri {uri} is not http(s)"));
            }
        }
        let client_id = format!("rvb_client_{}", hex(&random_bytes(16)?));
        let secret = b64url(&random_bytes(32)?);
        let secret_hash = sha256_hex(secret.as_bytes());
        let uris = serde_json::to_string(redirect_uris).map_err(|e| e.to_string())?;
        let name = if client_name.trim().is_empty() {
            "mcp".to_string()
        } else {
            client_name.trim().to_string()
        };
        self.pool.with(|conn| {
            conn.exec(&format!(
                "INSERT INTO rvbbit.oauth_client (client_id, secret_hash, redirect_uris) VALUES ({}, {}, {}::jsonb)",
                quote_lit(&client_id),
                quote_lit(&secret_hash),
                quote_lit(&uris),
            ))
        })?;
        let _ = name;
        Ok(json!({
            "client_id": client_id,
            "client_secret": secret,
            "client_name": name,
            "redirect_uris": redirect_uris,
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
        }))
    }

    pub fn client_allows(&self, client_id: &str, redirect_uri: &str) -> Result<bool, String> {
        let rows = self.pool.with(|conn| {
            conn.query(&format!(
                "SELECT redirect_uris::text FROM rvbbit.oauth_client WHERE client_id = {}",
                quote_lit(client_id)
            ))
        })?;
        let Some(raw) = cell(&rows, 0, 0) else {
            return Ok(false);
        };
        let uris: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
        Ok(uris.iter().any(|u| u == redirect_uri))
    }

    pub fn login(
        &self,
        client_id: &str,
        redirect_uri: &str,
        challenge: &str,
        method: &str,
        user: &str,
        password: &str,
    ) -> Result<String, String> {
        if method != "S256" {
            return Err("code_challenge_method must be S256".into());
        }
        if !ident_ok(user) {
            return Err("role name is not an identifier".into());
        }
        if !self.client_allows(client_id, redirect_uri)? {
            return Err("client or redirect_uri is not registered".into());
        }
        let user_row = self.user_row(user)?;
        if !self.password_matches(user, password)? {
            return Err("postgres rejected the password".into());
        }
        let code = b64url(&random_bytes(32)?);
        let hash = sha256_hex(code.as_bytes());
        let exp = now_secs() + CODE_TTL_SECS;
        self.pool.with(|conn| {
            conn.exec(&format!(
                "INSERT INTO rvbbit.oauth_code (code_hash, client_id, role, subject, redirect_uri, code_challenge, credential_version, expires_at) \
                 VALUES ({}, {}, {}, {}, {}, {}, {}, to_timestamp({exp}))",
                quote_lit(&hash),
                quote_lit(client_id),
                quote_lit(user),
                quote_lit(user),
                quote_lit(redirect_uri),
                quote_lit(challenge),
                user_row.credential_version,
            ))
        })?;
        Ok(code)
    }

    pub fn exchange_code(
        &self,
        client_id: &str,
        redirect_uri: &str,
        code: &str,
        verifier: &str,
    ) -> Result<Value, String> {
        let got = s256_challenge(verifier)?;
        let hash = sha256_hex(code.as_bytes());
        let rows = self.pool.with(|conn| {
            conn.query(&format!(
                "DELETE FROM rvbbit.oauth_code WHERE code_hash = {} AND expires_at > now() \
                 RETURNING client_id, role, subject, redirect_uri, code_challenge, credential_version::text",
                quote_lit(&hash)
            ))
        })?;
        if rows.rows.is_empty() {
            return Err("authorization code is unknown or expired".into());
        }
        let row = &rows.rows[0];
        let stored_client = row.first().and_then(|c| c.clone()).unwrap_or_default();
        let role = row.get(1).and_then(|c| c.clone()).unwrap_or_default();
        let subject = row.get(2).and_then(|c| c.clone()).unwrap_or_default();
        let stored_redirect = row.get(3).and_then(|c| c.clone()).unwrap_or_default();
        let challenge = row.get(4).and_then(|c| c.clone()).unwrap_or_default();
        let version: i32 = row.get(5).and_then(|c| c.as_deref()).unwrap_or("0").parse().unwrap_or(0);
        if stored_client != client_id || stored_redirect != redirect_uri {
            return Err("authorization code does not match this client".into());
        }
        if !constant_eq(challenge.as_bytes(), got.as_bytes()) {
            return Err("PKCE verification failed".into());
        }
        self.tokens(&role, &subject, version, client_id)
    }

    pub fn exchange_refresh(&self, client_id: &str, refresh: &str) -> Result<Value, String> {
        let hash = sha256_hex(refresh.as_bytes());
        let rows = self.pool.with(|conn| {
            conn.query(&format!(
                "DELETE FROM rvbbit.oauth_refresh WHERE token_hash = {} AND expires_at > now() \
                 RETURNING client_id, role, subject, credential_version::text",
                quote_lit(&hash)
            ))
        })?;
        if rows.rows.is_empty() {
            return Err("refresh token is unknown or expired".into());
        }
        let row = &rows.rows[0];
        let stored_client = row.first().and_then(|c| c.clone()).unwrap_or_default();
        if stored_client != client_id {
            return Err("refresh token does not match this client".into());
        }
        let role = row.get(1).and_then(|c| c.clone()).unwrap_or_default();
        let subject = row.get(2).and_then(|c| c.clone()).unwrap_or_default();
        let version: i32 = row.get(3).and_then(|c| c.as_deref()).unwrap_or("0").parse().unwrap_or(0);
        let current = self.user_row(&role)?;
        if current.credential_version != version {
            return Err("credential version was revoked".into());
        }
        self.tokens(&role, &subject, current.credential_version, client_id)
    }

    fn tokens(&self, role: &str, subject: &str, version: i32, client_id: &str) -> Result<Value, String> {
        let access = sign_jwt(&self.jwt_secret, role, subject, version, now_secs() + ACCESS_TTL_SECS)?;
        let refresh = b64url(&random_bytes(32)?);
        let hash = sha256_hex(refresh.as_bytes());
        let exp = now_secs() + REFRESH_TTL_SECS;
        self.pool.with(|conn| {
            conn.exec(&format!(
                "INSERT INTO rvbbit.oauth_refresh (token_hash, client_id, role, subject, credential_version, expires_at) \
                 VALUES ({}, {}, {}, {}, {}, to_timestamp({exp}))",
                quote_lit(&hash),
                quote_lit(client_id),
                quote_lit(role),
                quote_lit(subject),
                version,
            ))
        })?;
        Ok(json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": ACCESS_TTL_SECS,
            "refresh_token": refresh,
        }))
    }

    pub fn issue_api_key(&self, role: &str, name: &str) -> Result<String, String> {
        if !ident_ok(role) {
            return Err("role name is not an identifier".into());
        }
        let user = self.user_row(role)?;
        let key = format!("rvb_{}", b64url(&random_bytes(32)?));
        let hash = sha256_hex(key.as_bytes());
        self.pool.with(|conn| {
            conn.exec(&format!(
                "INSERT INTO rvbbit.api_key (key_hash, name, role, credential_version) VALUES ({}, {}, {}, {})",
                quote_lit(&hash),
                quote_lit(name),
                quote_lit(role),
                user.credential_version,
            ))
        })?;
        Ok(key)
    }

    pub fn identity_for_bearer(&self, token: &str) -> Result<Identity, String> {
        let token = token.trim();
        let token = token.strip_prefix("Bearer ").or_else(|| token.strip_prefix("bearer ")).unwrap_or(token);
        if token.is_empty() {
            return Err("missing bearer token".into());
        }
        if token.matches('.').count() == 2 {
            self.identity_for_jwt(token)
        } else {
            self.identity_for_api_key(token)
        }
    }

    fn identity_for_jwt(&self, token: &str) -> Result<Identity, String> {
        let claims = verify_jwt(&self.jwt_secret, token)?;
        let user = self.user_row(&claims.role)?;
        if user.credential_version != claims.ver {
            return Err("credential version was revoked".into());
        }
        Ok(Identity {
            subject: claims.sub,
            role: user.role,
            max_cost: user.max_cost,
            statement_timeout_ms: user.statement_timeout_ms,
            credential_version: user.credential_version,
        })
    }

    fn identity_for_api_key(&self, token: &str) -> Result<Identity, String> {
        let hash = sha256_hex(token.as_bytes());
        let rows = self.pool.with(|conn| {
            conn.query(&format!(
                "SELECT k.name, k.role, k.credential_version::text, u.credential_version::text, \
                        u.max_cost::text, u.statement_timeout_ms::text \
                 FROM rvbbit.api_key k JOIN rvbbit.users u ON u.role = k.role \
                 WHERE k.key_hash = {}",
                quote_lit(&hash)
            ))
        })?;
        if rows.rows.is_empty() {
            return Err("api key is unknown".into());
        }
        let row = &rows.rows[0];
        let name = row.first().and_then(|c| c.clone()).unwrap_or_default();
        let role = row.get(1).and_then(|c| c.clone()).unwrap_or_default();
        let key_ver: i32 = row.get(2).and_then(|c| c.as_deref()).unwrap_or("0").parse().unwrap_or(0);
        let user_ver: i32 = row.get(3).and_then(|c| c.as_deref()).unwrap_or("0").parse().unwrap_or(0);
        if key_ver != user_ver {
            return Err("credential version was revoked".into());
        }
        Ok(Identity {
            subject: name,
            role,
            max_cost: row.get(4).and_then(|c| c.as_deref()).and_then(|s| s.parse().ok()),
            statement_timeout_ms: row
                .get(5)
                .and_then(|c| c.as_deref())
                .and_then(|s| s.parse().ok())
                .unwrap_or(15_000),
            credential_version: user_ver,
        })
    }

    fn user_row(&self, role: &str) -> Result<Identity, String> {
        let rows = self.pool.with(|conn| {
            conn.query(&format!(
                "SELECT role, credential_version::text, max_cost::text, statement_timeout_ms::text \
                 FROM rvbbit.users WHERE role = {}",
                quote_lit(role)
            ))
        })?;
        if rows.rows.is_empty() {
            return Err(format!("role {role} is not in rvbbit.users"));
        }
        let row = &rows.rows[0];
        Ok(Identity {
            subject: role.to_string(),
            role: row.first().and_then(|c| c.clone()).unwrap_or_default(),
            credential_version: row.get(1).and_then(|c| c.as_deref()).unwrap_or("1").parse().unwrap_or(1),
            max_cost: row.get(2).and_then(|c| c.as_deref()).and_then(|s| s.parse().ok()),
            statement_timeout_ms: row
                .get(3)
                .and_then(|c| c.as_deref())
                .and_then(|s| s.parse().ok())
                .unwrap_or(15_000),
        })
    }

    fn password_matches(&self, user: &str, password: &str) -> Result<bool, String> {
        let host = conninfo_escape(&self.password.host)?;
        let db = conninfo_escape(&self.password.dbname)?;
        let user = conninfo_escape(user)?;
        let password = conninfo_escape(password)?;
        let info = format!(
            "host='{host}' port={} dbname='{db}' user='{user}' password='{password}' connect_timeout=5",
            self.password.port
        );
        password_ok(&info)
    }
}

fn cell(rows: &Rows, r: usize, c: usize) -> Option<String> {
    rows.rows.get(r)?.get(c)?.clone()
}

struct Claims {
    sub: String,
    role: String,
    ver: i32,
}

static JTI_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_jti() -> String {
    let n = JTI_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{}-{n}", now_secs())
}

pub fn sign_jwt(secret: &[u8], role: &str, subject: &str, ver: i32, exp: u64) -> Result<String, String> {
    let header = b64url(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = b64url(
        json!({
            "sub": subject,
            "role": role,
            "ver": ver,
            "iat": now_secs(),
            "jti": next_jti(),
            "exp": exp,
        })
        .to_string()
        .as_bytes(),
    );
    let signing = format!("{header}.{payload}");
    let mac = b64url(&hmac_sha256(secret, signing.as_bytes()));
    Ok(format!("{signing}.{mac}"))
}

fn verify_jwt(secret: &[u8], token: &str) -> Result<Claims, String> {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(mac), None) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("access token is not a JWT".into());
    };
    let expect = b64url(&hmac_sha256(secret, format!("{header}.{payload}").as_bytes()));
    if !constant_eq(expect.as_bytes(), mac.as_bytes()) {
        return Err("access token signature does not match".into());
    }
    let bytes = b64url_decode(payload)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let exp = value.get("exp").and_then(|v| v.as_u64()).unwrap_or(0);
    if exp < now_secs() {
        return Err("access token is expired".into());
    }
    let role = value.get("role").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if !ident_ok(&role) {
        return Err("access token role is not an identifier".into());
    }
    Ok(Claims {
        sub: value.get("sub").and_then(|v| v.as_str()).unwrap_or(&role).to_string(),
        role,
        ver: value.get("ver").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
    })
}

pub fn s256_challenge(verifier: &str) -> Result<String, String> {
    if !(43..=128).contains(&verifier.len()) {
        return Err("code_verifier length must be 43 to 128".into());
    }
    Ok(b64url(&sha256(verifier.as_bytes())))
}

pub fn ident_ok(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => chars.all(|c| c.is_ascii_alphanumeric() || c == '_'),
        _ => false,
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn sha256(data: &[u8]) -> [u8; PG_SHA256_DIGEST_LENGTH] {
    let mut ctx = PgSha256Ctx::init_sha256();
    ctx.update(data);
    ctx.final_sha256()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&sha256(data))
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0u8; 64];
    let mut opad = [0u8; 64];
    for i in 0..64 {
        ipad[i] = k[i] ^ 0x36;
        opad[i] = k[i] ^ 0x5c;
    }
    let mut inner = Vec::with_capacity(64 + msg.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(msg);
    let inner_hash = sha256(&inner);
    let mut outer = Vec::with_capacity(96);
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner_hash);
    sha256(&outer)
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
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

fn b64url_decode(s: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Result<u8, String> {
        match c {
            b'A'..=b'Z' => Ok(c - b'A'),
            b'a'..=b'z' => Ok(c - b'a' + 26),
            b'0'..=b'9' => Ok(c - b'0' + 52),
            b'-' => Ok(62),
            b'_' => Ok(63),
            _ => Err("access token payload is not base64url".into()),
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        let n = ((val(bytes[i])? as u32) << 18)
            | ((val(bytes[i + 1])? as u32) << 12)
            | ((val(bytes[i + 2])? as u32) << 6)
            | val(bytes[i + 3])? as u32;
        out.push((n >> 16) as u8);
        out.push((n >> 8) as u8);
        out.push(n as u8);
        i += 4;
    }
    let rest = bytes.len() - i;
    if rest == 2 {
        let n = ((val(bytes[i])? as u32) << 18) | ((val(bytes[i + 1])? as u32) << 12);
        out.push((n >> 16) as u8);
    } else if rest == 3 {
        let n = ((val(bytes[i])? as u32) << 18)
            | ((val(bytes[i + 1])? as u32) << 12)
            | ((val(bytes[i + 2])? as u32) << 6);
        out.push((n >> 16) as u8);
        out.push((n >> 8) as u8);
    } else if rest != 0 {
        return Err("access token payload is not base64url".into());
    }
    Ok(out)
}

fn random_bytes(n: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; n];
    let mut file = std::fs::File::open("/dev/urandom").map_err(|e| e.to_string())?;
    std::io::Read::read_exact(&mut file, &mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_matches_the_rfc_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = s256_challenge(verifier).unwrap();
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn jwt_round_trip_carries_role_and_version() {
        let secret = b"test-secret";
        let token = sign_jwt(secret, "analyst", "analyst", 3, now_secs() + 60).unwrap();
        let again = sign_jwt(secret, "analyst", "analyst", 3, now_secs() + 60).unwrap();
        assert_ne!(token, again, "iat and jti must make a refresh differ");
        let claims = verify_jwt(secret, &token).unwrap();
        assert_eq!(claims.role, "analyst");
        assert_eq!(claims.ver, 3);
        let payload = token.split('.').nth(1).unwrap();
        let body: Value = serde_json::from_slice(&b64url_decode(payload).unwrap()).unwrap();
        assert!(body.get("iat").and_then(|v| v.as_u64()).is_some(), "{body}");
        assert!(body.get("jti").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()), "{body}");
        let other: Value = serde_json::from_slice(&b64url_decode(again.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_ne!(body["jti"], other["jti"]);
        let mut parts: Vec<String> = token.split('.').map(|s| s.to_string()).collect();
        parts[2].pop();
        parts[2].push('A');
        let tampered = parts.join(".");
        assert!(verify_jwt(secret, &tampered).is_err());
    }
}
