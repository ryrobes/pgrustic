//! Inbound session auth. M10 does not accept MCP clients. M11 maps an
//! OAuth 2.1 access token (PKCE, dynamic client registration) through
//! [`SessionAuth`] onto a PostgreSQL role.

/// Role applied for one inbound MCP session.
#[derive(Clone, Debug, PartialEq)]
pub struct PgRole {
    pub role: String,
    pub read_only: bool,
    pub max_cost: Option<f64>,
}

pub trait SessionAuth: Send + Sync {
    fn role_for_token(&self, token: &str) -> Result<PgRole, String>;
}

/// Placeholder until M11. Every token is rejected.
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
