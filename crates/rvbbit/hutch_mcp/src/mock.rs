//! Reference tools used by the mock suite and `scripts/rvbbit_mcp_demo.sh`.
//!
//! `echo` and `now` are ordinary scalars. `fail` is a tool error. `people`
//! returns an array of objects so IMPORT can build `RETURNS TABLE`.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, Json, ServiceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Default)]
pub struct DemoServer;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EchoArgs {
    /// Text to return.
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Person {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BagBody {
    pub k: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BagArgs {
    pub bag: BagBody,
}

#[tool_router(server_handler)]
impl DemoServer {
    #[tool(description = "Return the text argument")]
    fn echo(&self, Parameters(EchoArgs { text }): Parameters<EchoArgs>) -> String {
        text
    }

    #[tool(description = "Return the current UTC time")]
    fn now(&self) -> String {
        chrono_lite()
    }

    #[tool(description = "Fail on purpose")]
    fn fail(&self) -> Result<String, rmcp::ErrorData> {
        Err(rmcp::ErrorData::invalid_params("fail was called", None))
    }

    #[tool(description = "Echo one field of a JSON object")]
    fn bag(&self, Parameters(BagArgs { bag }): Parameters<BagArgs>) -> String {
        bag.k
    }

    /// The SQL name is longer than 63 bytes, so IMPORT records a warning
    /// and keeps going.
    #[tool(description = "Name does not fit in a Postgres identifier")]
    fn this_tool_name_is_deliberately_longer_than_sixty_three_characters_xx(&self) -> String {
        String::new()
    }

    #[tool(description = "Return two people")]
    fn people(&self) -> Json<Vec<Person>> {
        Json(vec![
            Person { id: 1, name: "Ada".into() },
            Person { id: 2, name: "Ken".into() },
        ])
    }
}

fn chrono_lite() -> String {
    // Avoid a time crate. The value only has to be non-empty and stable
    // enough for a mock. Seconds since the unix epoch from the system clock.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("1970-01-01T00:00:{secs:02}Z")
}

pub fn serve_stdio() -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let service = DemoServer
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|e| e.to_string())?;
        service.waiting().await.map_err(|e| e.to_string())?;
        Ok(())
    })
}

pub fn serve_http(addr: &str) -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let config = rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true);
        let service = rmcp::transport::streamable_http_server::StreamableHttpService::new(
            || Ok(DemoServer),
            rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default().into(),
            config,
        );
        let router = axum::Router::new().nest_service("/mcp", service);
        let listener = crate::http::bind_listener(addr).map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let listener = tokio::net::TcpListener::from_std(listener).map_err(|e| e.to_string())?;
        eprintln!("hutch-mcp mock http on {addr}");
        axum::serve(listener, router).await.map_err(|e| e.to_string())?;
        Ok(())
    })
}
