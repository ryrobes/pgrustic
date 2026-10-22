//! `hutch-mcp` is the process Postgres talks to. It owns the `rmcp`
//! client (stdio and streamable HTTP) and a small HTTP API. A later
//! inbound server can live in this binary and reuse [`mcp_core`] auth,
//! config, and the PostgreSQL session preamble.

mod client;
mod http;
mod mock;

pub use client::{call_many, call_tool, list_tools};
pub use http::{bind_listener, serve};
pub use mock::{serve_http as mock_http, serve_stdio as mock_stdio};

pub use mcp_core::{PgRole, PgSession, RecordingPg, ServerSpec, SessionAuth, SessionPreamble, UnconfiguredAuth};
