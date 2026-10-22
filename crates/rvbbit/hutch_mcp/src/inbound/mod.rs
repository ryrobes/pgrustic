//! Inbound MCP warehouse. OAuth 2.1 and the tool server share this process
//! with the outbound gateway, on a different listen mode (`--inbound`).

mod http;
mod oauth;
mod pg;
mod tools;

pub use http::{issue_key, serve, serve_stdio, InboundConfig};
pub use tools::bootstrap_sql;
