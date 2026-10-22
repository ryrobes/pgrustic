//! Role applied to one inbound call. The warehouse server checks the
//! bearer itself. [`UnconfiguredAuth`] is the stand-in for a process
//! that has no token store.

/// Role applied for one inbound MCP session.
#[derive(Clone, Debug, PartialEq)]
pub struct PgRole {
    pub role: String,
    pub read_only: bool,
    pub max_cost: Option<f64>,
    /// `SET LOCAL statement_timeout`. `0` leaves the session default.
    pub statement_timeout_ms: u64,
}

pub trait SessionAuth: Send + Sync {
    fn role_for_token(&self, token: &str) -> Result<PgRole, String>;
}

/// Stand-in for a process that has no token store. The inbound server does
/// not use this type: it checks JWTs and hashed API keys in Postgres.
#[derive(Clone, Debug, Default)]
pub struct UnconfiguredAuth;

impl SessionAuth for UnconfiguredAuth {
    fn role_for_token(&self, _token: &str) -> Result<PgRole, String> {
        Err("inbound MCP auth is not configured".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfigured_auth_rejects_tokens() {
        let err = UnconfiguredAuth.role_for_token("tok").unwrap_err();
        assert!(err.contains("not configured"), "{err}");
    }
}
