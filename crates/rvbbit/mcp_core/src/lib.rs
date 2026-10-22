//! Shared MCP types for outbound `hutch-mcp` and a later inbound server.
//!
//! M10 uses this crate as the catalog and client contract: server specs,
//! JSON Schema to SQL, secret references, a read-only tool cache, and the
//! session pieces M11 will reuse (token to role, `SET LOCAL ROLE`,
//! read-only, `semantic.max_cost`). No tokio and no `rmcp` here, so the
//! Postgres backend does not link the MCP SDK.

mod auth;
mod cache;
mod pg;
mod schema;
mod secrets;

pub use auth::{PgRole, SessionAuth, UnconfiguredAuth};
pub use cache::ToolCache;
pub use pg::{quote_ident, RecordingPg, SessionPreamble, PgSession};
pub use schema::{
    arguments_object, json_names, schema_hash, sql_args, sql_ident, tool_fn_body, ArgSpec,
    ToolShape, RETURNS_TABLE,
};
pub use secrets::{parse_assignments, reject_plaintext, resolve_ref, resolve_map};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One outbound MCP server. Header and env values in the catalog are
/// `secret:NAME` references. [`ServerSpec::resolve_secrets`] is the only
/// step that reads `RVBBIT_SECRET_<NAME>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerSpec {
    pub name: String,
    pub transport: String,
    pub url: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub headers: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
    pub timeout_ms: u64,
    pub retries: u32,
}

impl ServerSpec {
    pub fn validate_stored(&self) -> Result<(), String> {
        match self.transport.as_str() {
            "http" => {
                if self.url.as_deref().unwrap_or("").trim().is_empty() {
                    return Err("http MCP server needs a url".into());
                }
            }
            "stdio" => {
                if self.command.as_deref().unwrap_or("").trim().is_empty() {
                    return Err("stdio MCP server needs a command".into());
                }
            }
            other => return Err(format!("MCP transport {other} is not http or stdio")),
        }
        reject_plaintext(&self.headers)?;
        reject_plaintext(&self.env)?;
        Ok(())
    }

    /// Copy with header and env values loaded from the environment.
    pub fn resolve_secrets(&self) -> Result<ServerSpec, String> {
        self.validate_stored()?;
        Ok(ServerSpec {
            headers: resolve_map(&self.headers)?,
            env: resolve_map(&self.env)?,
            ..self.clone()
        })
    }
}

/// Options map from `CREATE SERVER ... OPTIONS (...)`.
pub fn server_from_options(name: &str, options: &BTreeMap<String, String>) -> Result<ServerSpec, String> {
    let transport = options
        .get("transport")
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "http".into());
    let args = match options.get("args") {
        None => Vec::new(),
        Some(raw) if raw.trim().starts_with('[') => serde_json::from_str(raw)
            .map_err(|e| format!("args is not a JSON array: {e}"))?,
        Some(raw) => raw.split_whitespace().map(|s| s.to_string()).collect(),
    };
    let headers = match options.get("headers") {
        None => BTreeMap::new(),
        Some(raw) => parse_assignments(raw)?,
    };
    let env = match options.get("env") {
        None => BTreeMap::new(),
        Some(raw) => parse_assignments(raw)?,
    };
    let timeout_ms = match options.get("timeout") {
        None => 30_000,
        Some(raw) => parse_timeout_ms(raw)?,
    };
    let retries = match options.get("retries") {
        None => 2,
        Some(raw) => raw
            .parse::<u32>()
            .map_err(|_| format!("retries {raw} is not an integer"))?,
    };
    let spec = ServerSpec {
        name: name.to_string(),
        transport,
        url: options.get("url").cloned(),
        command: options.get("command").cloned(),
        args,
        headers,
        env,
        timeout_ms,
        retries,
    };
    spec.validate_stored()?;
    Ok(spec)
}

pub fn parse_timeout_ms(raw: &str) -> Result<u64, String> {
    let t = raw.trim();
    if let Some(rest) = t.strip_suffix("ms") {
        return rest
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("bad timeout {raw}"));
    }
    if let Some(rest) = t.strip_suffix('s') {
        let n: f64 = rest.trim().parse().map_err(|_| format!("bad timeout {raw}"))?;
        return Ok((n * 1000.0) as u64);
    }
    if let Some(rest) = t.strip_suffix('m') {
        let n: f64 = rest.trim().parse().map_err(|_| format!("bad timeout {raw}"))?;
        return Ok((n * 60_000.0) as u64);
    }
    t.parse::<u64>().map_err(|_| format!("bad timeout {raw}"))
}

/// Tool row returned by a list call, before it becomes a SQL function.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ListedTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
    pub schema_hash: String,
    /// MCP `readOnlyHint`. IMPORT still defaults to VOLATILE unless the
    /// caller sets `read_only`. The hint is recorded so ALTER can adopt it.
    #[serde(default)]
    pub read_only_hint: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_and_stdio_options_keep_secret_refs() {
        let http = server_from_options(
            "fetch",
            &BTreeMap::from([
                ("transport".into(), "http".into()),
                ("url".into(), "http://127.0.0.1:9/mcp".into()),
                ("headers".into(), "Authorization=secret:FETCH_TOKEN".into()),
            ]),
        )
        .unwrap();
        assert_eq!(http.headers["Authorization"], "secret:FETCH_TOKEN");
        assert!(http.resolve_secrets().is_err());

        let stdio = server_from_options(
            "local",
            &BTreeMap::from([
                ("transport".into(), "stdio".into()),
                ("command".into(), "hutch-mcp".into()),
                ("args".into(), "--mock-stdio".into()),
                ("env".into(), "REGION=secret:REGION".into()),
            ]),
        )
        .unwrap();
        assert_eq!(stdio.args, vec!["--mock-stdio".to_string()]);
        assert_eq!(stdio.env["REGION"], "secret:REGION");
    }

    #[test]
    fn plaintext_header_is_rejected() {
        let err = server_from_options(
            "fetch",
            &BTreeMap::from([
                ("transport".into(), "http".into()),
                ("url".into(), "http://127.0.0.1/mcp".into()),
                ("headers".into(), "Authorization=Bearer secret-token".into()),
            ]),
        )
        .unwrap_err();
        assert!(err.contains("plaintext"), "{err}");
    }
}
