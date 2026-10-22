//! `hutch-mcp` is the process Postgres talks to, and the inbound MCP
//! warehouse. Outbound mode owns the `rmcp` client and the Postgres-facing
//! HTTP API. `--inbound` serves streamable HTTP at `/mcp` plus the OAuth
//! 2.1 endpoints. Both reuse [`mcp_core`] auth and the session preamble.

mod client;
mod http;
mod inbound;
mod mock;

pub use client::{call_many, call_tool, list_tools};
pub use http::{bind_listener, serve};
pub use inbound::{bootstrap_sql, issue_key, serve as serve_inbound, serve_stdio as serve_inbound_stdio, InboundConfig};
pub use mock::{serve_http as mock_http, serve_stdio as mock_stdio};

pub use mcp_core::{PgRole, PgSession, RecordingPg, ServerSpec, SessionAuth, SessionPreamble, UnconfiguredAuth};
