//! Generic OIDC login in front of the password form.
//!
//! The issuer document, client id, and client secret come from the
//! environment. `RVBBIT_OIDC_PROVIDER=google` is the Google issuer.
//! The id_token is checked with the provider JWKS (RS256), including
//! the nonce from this browser round trip. Email or `hd` maps to a
//! Postgres role. Nothing here writes a token, secret, or code to a log.

use std::time::Duration;

use serde_json::Value;

use super::oauth::{b64url, b64url_decode, constant_eq, now_secs, random_bytes, s256_challenge, sha256_hex, Oauth};
use super::pg::quote_lit;

const STATE_TTL_SECS: u64 = 600;

/// Google's issuer. Discovery is `{issuer}/.well-known/openid-configuration`.
pub const GOOGLE_ISSUER: &str = "https://accounts.google.com";

#[derive(Clone)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub domains: Vec<String>,
    /// Button label. Google's preset says so; anything else says OIDC.
    pub label: String,
}

impl std::fmt::Debug for OidcConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcConfig")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("domains", &self.domains)
            .field("label", &self.label)
            .finish()
    }
}

#[derive(Clone)]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
}

#[derive(Clone)]
pub struct Jwk {
    pub kid: String,
    pub n: String,
    pub e: String,
}

#[derive(Clone, Debug)]
pub struct RoleMapping {
    pub claim: String,
    pub value: String,
    pub role: String,
    pub read_only: bool,
}

#[derive(Clone, Debug)]
pub struct MappedRole {
    pub role: String,
    pub read_only: bool,
    pub subject: String,
}

pub struct OidcFinish {
    pub redirect_uri: String,
    pub code: String,
    pub client_state: String,
}

pub fn config_from_env() -> Option<OidcConfig> {
    config_from_values(
        std::env::var("RVBBIT_OIDC_PROVIDER").ok().as_deref(),
        std::env::var("RVBBIT_OIDC_ISSUER").ok().as_deref(),
        std::env::var("RVBBIT_OIDC_CLIENT_ID").ok().as_deref(),
        std::env::var("RVBBIT_OIDC_CLIENT_SECRET").ok().as_deref(),
        std::env::var("RVBBIT_OIDC_DOMAINS").ok().as_deref(),
    )
}

pub fn config_from_values(
    provider: Option<&str>,
    issuer: Option<&str>,
    client_id: Option<&str>,
    client_secret: Option<&str>,
    domains: Option<&str>,
) -> Option<OidcConfig> {
    let provider = provider.unwrap_or("").trim();
    let explicit = issuer.unwrap_or("").trim().trim_end_matches('/').to_string();
    let (issuer, label) = if !explicit.is_empty() {
        let label = if provider.eq_ignore_ascii_case("google") || explicit == GOOGLE_ISSUER {
            "Google"
        } else {
            "OIDC"
        };
        (explicit, label)
    } else if provider.eq_ignore_ascii_case("google") {
        (GOOGLE_ISSUER.to_string(), "Google")
    } else {
        return None;
    };
    let client_id = client_id.unwrap_or("").trim().to_string();
    let client_secret = client_secret.unwrap_or("").trim().to_string();
    if client_id.is_empty() || client_secret.is_empty() {
        return None;
    }
    let domains = domains
        .unwrap_or("")
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|s| s.trim().trim_start_matches('@').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    Some(OidcConfig { issuer, client_id, client_secret, domains, label: label.into() })
}

pub fn resolve_role(email: &str, hd: Option<&str>, domains: &[String], maps: &[RoleMapping]) -> Result<MappedRole, String> {
    let email = email.trim().to_ascii_lowercase();
    let Some((local, domain)) = email.rsplit_once('@') else {
        return Err("id_token email is not an address".into());
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return Err("id_token email is not an address".into());
    }
    if !domains.is_empty() {
        let email_ok = domains.iter().any(|d| d.eq_ignore_ascii_case(domain));
        let hd_ok = hd
            .map(|h| domains.iter().any(|d| d.eq_ignore_ascii_case(h.trim())))
            .unwrap_or(false);
        if !email_ok && !hd_ok {
            return Err("email domain is not allowed".into());
        }
    }
    if let Some(hit) = maps
        .iter()
        .find(|m| m.claim.eq_ignore_ascii_case("email") && m.value.eq_ignore_ascii_case(&email))
    {
        return Ok(MappedRole { role: hit.role.clone(), read_only: hit.read_only, subject: email });
    }
    if let Some(hd) = hd.map(str::trim).filter(|h| !h.is_empty()) {
        if let Some(hit) = maps
            .iter()
            .find(|m| m.claim.eq_ignore_ascii_case("hd") && m.value.eq_ignore_ascii_case(hd))
        {
            return Ok(MappedRole { role: hit.role.clone(), read_only: hit.read_only, subject: email });
        }
    }
    Err("no role mapping".into())
}

impl Oauth {
    pub fn begin_oidc(
        &self,
        client_id: &str,
        redirect_uri: &str,
        challenge: &str,
        method: &str,
        client_state: &str,
        callback_url: &str,
    ) -> Result<String, String> {
        let cfg = self.oidc.as_ref().ok_or_else(|| "OIDC is not configured".to_string())?;
        if method != "S256" {
            return Err("code_challenge_method must be S256".into());
        }
        if !self.client_allows(client_id, redirect_uri)? {
            return Err("client or redirect_uri is not registered".into());
        }
        let discovery = self.discovery(cfg)?;
        let state = b64url(&random_bytes(24)?);
        let nonce = b64url(&random_bytes(24)?);
        let verifier = b64url(&random_bytes(48)?);
        let upstream = s256_challenge(&verifier)?;
        let exp = now_secs() + STATE_TTL_SECS;
        self.pool.with(|conn| {
            conn.exec(&format!(
                "INSERT INTO rvbbit.oidc_state (state_hash, nonce, verifier, client_id, redirect_uri, code_challenge, client_state, callback, expires_at) \
                 VALUES ({}, {}, {}, {}, {}, {}, {}, {}, to_timestamp({exp}))",
                quote_lit(&sha256_hex(state.as_bytes())),
                quote_lit(&nonce),
                quote_lit(&verifier),
                quote_lit(client_id),
                quote_lit(redirect_uri),
                quote_lit(challenge),
                quote_lit(client_state),
                quote_lit(callback_url),
            ))
        })?;
        let mut url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}&code_challenge={}&code_challenge_method=S256",
            discovery.authorization_endpoint,
            form_escape(&cfg.client_id),
            form_escape(callback_url),
            form_escape("openid email"),
            form_escape(&state),
            form_escape(&nonce),
            form_escape(&upstream),
        );
        if cfg.issuer == GOOGLE_ISSUER {
            url.push_str("&prompt=select_account");
        }
        Ok(url)
    }

    pub fn finish_oidc(&self, code: &str, state: &str) -> Result<OidcFinish, String> {
        let cfg = self.oidc.as_ref().ok_or_else(|| "OIDC is not configured".to_string())?;
        if code.is_empty() || state.is_empty() {
            return Err("OIDC callback is missing code or state".into());
        }
        let rows = self.pool.with(|conn| {
            conn.query(&format!(
                "DELETE FROM rvbbit.oidc_state WHERE state_hash = {} AND expires_at > now() \
                 RETURNING nonce, verifier, client_id, redirect_uri, code_challenge, client_state, callback",
                quote_lit(&sha256_hex(state.as_bytes()))
            ))
        })?;
        if rows.rows.is_empty() {
            return Err("OIDC state is unknown or expired".into());
        }
        let row = &rows.rows[0];
        let nonce = row.first().and_then(|c| c.clone()).unwrap_or_default();
        let verifier = row.get(1).and_then(|c| c.clone()).unwrap_or_default();
        let client_id = row.get(2).and_then(|c| c.clone()).unwrap_or_default();
        let redirect_uri = row.get(3).and_then(|c| c.clone()).unwrap_or_default();
        let challenge = row.get(4).and_then(|c| c.clone()).unwrap_or_default();
        let client_state = row.get(5).and_then(|c| c.clone()).unwrap_or_default();
        let callback = row.get(6).and_then(|c| c.clone()).unwrap_or_default();
        let discovery = self.discovery(cfg)?;
        let id_token = exchange_code(
            &discovery.token_endpoint,
            &cfg.client_id,
            &cfg.client_secret,
            code,
            &verifier,
            &callback,
        )?;
        let claims = self.verify_id_token(&discovery, &cfg.client_id, &id_token, &nonce)?;
        if claims.email_verified == Some(false) {
            return Err("email is not verified".into());
        }
        let maps = self.role_maps()?;
        let mapped = resolve_role(&claims.email, claims.hd.as_deref(), &cfg.domains, &maps)?;
        if !super::oauth::ident_ok(&mapped.role) {
            return Err("mapped role is not an identifier".into());
        }
        let user = self.user_row(&mapped.role)?;
        let issued = self.issue_code(
            &client_id,
            &redirect_uri,
            &challenge,
            &mapped.role,
            &mapped.subject,
            user.credential_version,
            mapped.read_only,
        )?;
        Ok(OidcFinish { redirect_uri, code: issued, client_state })
    }

    fn role_maps(&self) -> Result<Vec<RoleMapping>, String> {
        let rows = self.pool.with(|conn| {
            conn.query("SELECT claim, value, role, read_only::text FROM rvbbit.oidc_role")
        })?;
        let mut maps = Vec::new();
        for row in &rows.rows {
            maps.push(RoleMapping {
                claim: row.first().and_then(|c| c.clone()).unwrap_or_default(),
                value: row.get(1).and_then(|c| c.clone()).unwrap_or_default(),
                role: row.get(2).and_then(|c| c.clone()).unwrap_or_default(),
                read_only: super::oauth::read_only_cell(row.get(3).and_then(|c| c.as_deref())),
            });
        }
        Ok(maps)
    }

    fn discovery(&self, cfg: &OidcConfig) -> Result<Discovery, String> {
        if let Some(cached) = self.discovery.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(cached);
        }
        let fresh = fetch_discovery(&cfg.issuer)?;
        *self.discovery.lock().unwrap_or_else(|e| e.into_inner()) = Some(fresh.clone());
        Ok(fresh)
    }

    fn verify_id_token(&self, discovery: &Discovery, audience: &str, token: &str, nonce: &str) -> Result<IdClaims, String> {
        let keys = self.jwks(&discovery.jwks_uri)?;
        match verify_id_token(token, &keys, &discovery.issuer, audience, nonce) {
            Ok(claims) => Ok(claims),
            Err(message) if message.contains("signature") => {
                *self.jwks.lock().unwrap_or_else(|e| e.into_inner()) = None;
                let keys = self.jwks(&discovery.jwks_uri)?;
                verify_id_token(token, &keys, &discovery.issuer, audience, nonce)
            }
            Err(message) => Err(message),
        }
    }

    fn jwks(&self, uri: &str) -> Result<Vec<Jwk>, String> {
        if let Some(cached) = self.jwks.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(cached);
        }
        let fresh = fetch_jwks(uri)?;
        *self.jwks.lock().unwrap_or_else(|e| e.into_inner()) = Some(fresh.clone());
        Ok(fresh)
    }
}

#[derive(Debug)]
struct IdClaims {
    email: String,
    hd: Option<String>,
    email_verified: Option<bool>,
}

fn verify_id_token(token: &str, keys: &[Jwk], issuer: &str, audience: &str, nonce: &str) -> Result<IdClaims, String> {
    let mut parts = token.split('.');
    let (Some(header_b64), Some(payload_b64), Some(sig_b64), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("id_token is not a signed JWT".into());
    };
    let header_bytes = b64url_decode(header_b64).map_err(|_| "id_token header is not base64url".to_string())?;
    let header: Value = serde_json::from_slice(&header_bytes).map_err(|_| "id_token header is not JSON".to_string())?;
    let alg = header.get("alg").and_then(|v| v.as_str()).unwrap_or("");
    if alg != "RS256" {
        return Err("id_token alg must be RS256".into());
    }
    let kid = header.get("kid").and_then(|v| v.as_str()).unwrap_or("");
    let sig = b64url_decode(sig_b64).map_err(|_| "id_token signature is not base64url".to_string())?;
    let message = format!("{header_b64}.{payload_b64}");
    let matched = keys.iter().filter(|key| kid.is_empty() || key.kid == kid);
    let mut signed = false;
    for key in matched {
        if rsa_verify(&key.n, &key.e, message.as_bytes(), &sig) {
            signed = true;
            break;
        }
    }
    if !signed {
        return Err("id_token signature does not match".into());
    }
    let payload_bytes = b64url_decode(payload_b64).map_err(|_| "id_token payload is not base64url".to_string())?;
    let payload: Value = serde_json::from_slice(&payload_bytes).map_err(|_| "id_token payload is not JSON".to_string())?;
    let iss = payload.get("iss").and_then(|v| v.as_str()).unwrap_or("");
    if !issuer_eq(iss, issuer) {
        return Err("id_token issuer does not match".into());
    }
    if !audience_has(&payload, audience) {
        return Err("id_token audience does not match".into());
    }
    let exp = json_u64(payload.get("exp")).unwrap_or(0);
    if exp < now_secs() {
        return Err("id_token is expired".into());
    }
    if let Some(nbf) = json_u64(payload.get("nbf")) {
        if nbf > now_secs() + 30 {
            return Err("id_token is not valid yet".into());
        }
    }
    let token_nonce = payload.get("nonce").and_then(|v| v.as_str()).unwrap_or("");
    if token_nonce.is_empty() || !constant_eq(token_nonce.as_bytes(), nonce.as_bytes()) {
        return Err("id_token nonce does not match".into());
    }
    let email = payload.get("email").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if email.is_empty() {
        return Err("id_token has no email".into());
    }
    let email_verified = payload.get("email_verified").and_then(|v| {
        v.as_bool().or_else(|| v.as_str().map(|s| s.eq_ignore_ascii_case("true")))
    });
    let hd = payload.get("hd").and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    Ok(IdClaims { email, hd, email_verified })
}

fn rsa_verify(n_b64: &str, e_b64: &str, message: &[u8], sig: &[u8]) -> bool {
    let Ok(n) = b64url_decode(n_b64) else {
        return false;
    };
    let Ok(e) = b64url_decode(e_b64) else {
        return false;
    };
    let key = ring::signature::RsaPublicKeyComponents { n: n.as_slice(), e: e.as_slice() };
    key.verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, message, sig).is_ok()
}

fn issuer_eq(got: &str, want: &str) -> bool {
    got.trim().trim_end_matches('/') == want.trim().trim_end_matches('/')
}

fn audience_has(payload: &Value, audience: &str) -> bool {
    match payload.get("aud") {
        Some(Value::String(s)) => s == audience,
        Some(Value::Array(items)) => items.iter().any(|v| v.as_str() == Some(audience)),
        _ => false,
    }
}

fn json_u64(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value.as_u64().or_else(|| value.as_i64().filter(|n| *n >= 0).map(|n| n as u64))
}

fn fetch_discovery(issuer: &str) -> Result<Discovery, String> {
    let url = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
    let body = http_text("GET", &url, None)?;
    let value: Value = serde_json::from_str(&body).map_err(|_| "OIDC discovery document is not JSON".to_string())?;
    let document_issuer = value.get("issuer").and_then(|v| v.as_str()).unwrap_or("");
    if !issuer_eq(document_issuer, issuer) {
        return Err("OIDC issuer does not match the discovery document".into());
    }
    Ok(Discovery {
        issuer: document_issuer.trim_end_matches('/').to_string(),
        authorization_endpoint: required_str(&value, "authorization_endpoint")?,
        token_endpoint: required_str(&value, "token_endpoint")?,
        jwks_uri: required_str(&value, "jwks_uri")?,
    })
}

fn fetch_jwks(uri: &str) -> Result<Vec<Jwk>, String> {
    let body = http_text("GET", uri, None)?;
    let value: Value = serde_json::from_str(&body).map_err(|_| "OIDC key set is not JSON".to_string())?;
    let keys = value.get("keys").and_then(|v| v.as_array()).ok_or("OIDC key set has no keys")?;
    let mut out = Vec::new();
    for key in keys {
        if key.get("kty").and_then(|v| v.as_str()) != Some("RSA") {
            continue;
        }
        let Some(n) = key.get("n").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(e) = key.get("e").and_then(|v| v.as_str()) else {
            continue;
        };
        out.push(Jwk {
            kid: key.get("kid").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            n: n.to_string(),
            e: e.to_string(),
        });
    }
    if out.is_empty() {
        return Err("OIDC key set has no RSA keys".into());
    }
    Ok(out)
}

fn exchange_code(
    token_endpoint: &str,
    client_id: &str,
    client_secret: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<String, String> {
    let body = format!(
        "grant_type=authorization_code&client_id={}&client_secret={}&code={}&code_verifier={}&redirect_uri={}",
        form_escape(client_id),
        form_escape(client_secret),
        form_escape(code),
        form_escape(verifier),
        form_escape(redirect_uri),
    );
    let text = http_text("POST", token_endpoint, Some(body))?;
    let value: Value = serde_json::from_str(&text).map_err(|_| "OIDC token response is not JSON".to_string())?;
    if value.get("error").is_some() {
        return Err("OIDC token endpoint rejected the login".into());
    }
    value
        .get("id_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| "OIDC token response has no id_token".into())
}

fn required_str(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("OIDC discovery is missing {key}"))
}

fn http_text(method: &str, url: &str, body: Option<String>) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(10)).redirects(0).build();
    let result = if method == "POST" {
        agent
            .post(url)
            .set("content-type", "application/x-www-form-urlencoded")
            .set("accept", "application/json")
            .send_string(body.as_deref().unwrap_or(""))
    } else {
        agent.get(url).set("accept", "application/json").call()
    };
    match result {
        Ok(resp) => resp.into_string().map_err(|_| "OIDC response was not text".into()),
        Err(_) => Err("OIDC request failed".into()),
    }
}

#[cfg(test)]
fn sign_rs(pair: &ring::signature::RsaKeyPair, payload: &str) -> String {
    let header = b64url(br#"{"alg":"RS256","kid":"test","typ":"JWT"}"#);
    let body = b64url(payload.as_bytes());
    let message = format!("{header}.{body}");
    let rng = ring::rand::SystemRandom::new();
    let mut sig = vec![0u8; pair.public().modulus_len()];
    pair.sign(&ring::signature::RSA_PKCS1_SHA256, &rng, message.as_bytes(), &mut sig)
        .expect("sign");
    format!("{message}.{}", b64url(&sig))
}

fn form_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_preset_and_domain_list() {
        let cfg = config_from_values(Some("google"), None, Some("client"), Some("secret"), Some("Example.com, acme.com")).unwrap();
        assert_eq!(cfg.issuer, GOOGLE_ISSUER);
        assert_eq!(cfg.label, "Google");
        assert_eq!(cfg.domains, vec!["example.com".to_string(), "acme.com".to_string()]);
        let explicit = config_from_values(None, Some("http://127.0.0.1:9/"), Some("id"), Some("sec"), None).unwrap();
        assert_eq!(explicit.issuer, "http://127.0.0.1:9");
        assert_eq!(explicit.label, "OIDC");
        assert!(config_from_values(Some("google"), None, Some(""), Some("secret"), None).is_none());
    }

    #[test]
    fn mapping_prefers_email_then_hd_and_denies_the_rest() {
        let maps = vec![
            RoleMapping { claim: "email".into(), value: "ada@example.com".into(), role: "analyst".into(), read_only: true },
            RoleMapping { claim: "hd".into(), value: "example.com".into(), role: "intern".into(), read_only: false },
        ];
        let email = resolve_role("Ada@Example.com", Some("example.com"), &[], &maps).unwrap();
        assert_eq!(email.role, "analyst");
        assert!(email.read_only);
        assert_eq!(email.subject, "ada@example.com");
        let hd = resolve_role("other@example.com", Some("Example.com"), &[], &maps).unwrap();
        assert_eq!(hd.role, "intern");
        assert!(!hd.read_only);
        let denied = resolve_role("ada@elsewhere.test", Some("elsewhere.test"), &[], &maps).unwrap_err();
        assert!(denied.contains("no role mapping"), "{denied}");
        let domain = resolve_role("ada@example.com", None, &["acme.com".into()], &maps).unwrap_err();
        assert!(domain.contains("not allowed"), "{domain}");
        let allowed = resolve_role("other@example.com", Some("example.com"), &["example.com".into()], &maps).unwrap();
        assert_eq!(allowed.role, "intern");
    }

    #[test]
    fn rs256_id_token_checks_signature_nonce_and_audience() {
        let dir = std::env::temp_dir().join(format!("rvbbit-oidc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pem = dir.join("key.pem");
        let pkcs8 = dir.join("key.pk8");
        let gen = std::process::Command::new("openssl")
            .args(["genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out"])
            .arg(&pem)
            .output()
            .expect("openssl");
        assert!(gen.status.success(), "{}", String::from_utf8_lossy(&gen.stderr));
        let conv = std::process::Command::new("openssl")
            .args(["pkcs8", "-topk8", "-nocrypt", "-outform", "DER", "-in"])
            .arg(&pem)
            .arg("-out")
            .arg(&pkcs8)
            .output()
            .expect("openssl pkcs8");
        assert!(conv.status.success(), "{}", String::from_utf8_lossy(&conv.stderr));
        let pair = ring::signature::RsaKeyPair::from_pkcs8(&std::fs::read(&pkcs8).unwrap()).unwrap();
        let public = ring::signature::RsaPublicKeyComponents::<Vec<u8>>::from(pair.public());
        let jwk = Jwk { kid: "test".into(), n: b64url(&public.n), e: b64url(&public.e) };
        let payload = serde_json::json!({
            "iss": "http://issuer.test",
            "aud": "client",
            "exp": now_secs() + 120,
            "nonce": "nonce-1",
            "email": "ada@example.com",
            "email_verified": true,
            "hd": "example.com",
        });
        let token = sign_rs(&pair, &payload.to_string());
        let ok = verify_id_token(&token, &[jwk.clone()], "http://issuer.test", "client", "nonce-1").unwrap();
        assert_eq!(ok.email, "ada@example.com");
        assert_eq!(ok.hd.as_deref(), Some("example.com"));
        let nonce = verify_id_token(&token, &[jwk.clone()], "http://issuer.test", "client", "other").unwrap_err();
        assert!(nonce.contains("nonce"), "{nonce}");
        let aud = verify_id_token(&token, &[jwk.clone()], "http://issuer.test", "nope", "nonce-1").unwrap_err();
        assert!(aud.contains("audience"), "{aud}");
        let mut bad = token;
        bad.pop();
        bad.push('A');
        assert!(verify_id_token(&bad, &[jwk], "http://issuer.test", "client", "nonce-1").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_debug_hides_the_secret() {
        let cfg = config_from_values(Some("google"), None, Some("client"), Some("super-secret"), None).unwrap();
        let text = format!("{cfg:?}");
        assert!(text.contains("<redacted>"), "{text}");
        assert!(!text.contains("super-secret"), "{text}");
    }
}
