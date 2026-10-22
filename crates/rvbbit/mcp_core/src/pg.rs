//! PostgreSQL session preamble for an inbound MCP call.
//!
//! The warehouse opens a transaction, runs [`SessionPreamble::sql`], then
//! serves the tool. `RecordingPg` is the stand-in used by tests.

use crate::auth::PgRole;

#[derive(Clone, Debug, PartialEq)]
pub struct SessionPreamble {
    pub role: String,
    pub read_only: bool,
    pub max_cost: Option<f64>,
    pub statement_timeout_ms: u64,
}

impl SessionPreamble {
    pub fn from_role(role: &PgRole) -> Self {
        Self {
            role: role.role.clone(),
            read_only: role.read_only,
            max_cost: role.max_cost,
            statement_timeout_ms: role.statement_timeout_ms,
        }
    }

    /// Statements applied inside the inbound transaction, after `BEGIN`.
    /// `SET LOCAL ROLE` is what GRANTs and RLS see. `transaction_read_only`
    /// rejects writes. `statement_timeout` and `semantic.max_cost` come
    /// from the token's role.
    pub fn sql(&self) -> String {
        let mut out = format!("SET LOCAL ROLE {}", quote_ident(&self.role));
        if self.read_only {
            out.push_str("; SET LOCAL transaction_read_only = on");
        }
        if self.statement_timeout_ms > 0 {
            out.push_str(&format!(
                "; SET LOCAL statement_timeout = '{}ms'",
                self.statement_timeout_ms
            ));
        }
        if let Some(cost) = self.max_cost {
            out.push_str(&format!("; SET LOCAL semantic.max_cost = '{cost}'"));
        }
        out
    }
}

pub trait PgSession: Send {
    fn apply(&mut self, preamble: &SessionPreamble) -> Result<(), String>;
    fn query(&mut self, sql: &str) -> Result<String, String>;
}

/// Test double. Records preamble SQL and queries. Does not connect.
#[derive(Clone, Debug, Default)]
pub struct RecordingPg {
    pub log: Vec<String>,
}

impl PgSession for RecordingPg {
    fn apply(&mut self, preamble: &SessionPreamble) -> Result<(), String> {
        self.log.push(preamble.sql());
        Ok(())
    }

    fn query(&mut self, sql: &str) -> Result<String, String> {
        self.log.push(sql.to_string());
        Ok(String::new())
    }
}

pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preamble_sets_role_readonly_and_max_cost() {
        let sql = SessionPreamble {
            role: "analyst".into(),
            read_only: true,
            max_cost: Some(0.25),
            statement_timeout_ms: 5000,
        }
        .sql();
        assert!(sql.contains("SET LOCAL ROLE \"analyst\""), "{sql}");
        assert!(sql.contains("transaction_read_only = on"), "{sql}");
        assert!(sql.contains("statement_timeout = '5000ms'"), "{sql}");
        assert!(sql.contains("semantic.max_cost = '0.25'"), "{sql}");
        let mut pg = RecordingPg::default();
        pg.apply(&SessionPreamble {
            role: "analyst".into(),
            read_only: true,
            max_cost: None,
            statement_timeout_ms: 0,
        })
        .unwrap();
        assert_eq!(pg.log.len(), 1);
    }
}
