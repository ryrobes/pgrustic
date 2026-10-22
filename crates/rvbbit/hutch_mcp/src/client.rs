//! Outbound MCP client. stdio spawns `command`. HTTP uses streamable HTTP.
//! One connection serves a whole batch. stdio and HTTP both overlap up to
//! the requested concurrency: MCP allows concurrent JSON-RPC request ids
//! on one session.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use futures::future::join_all;
use mcp_core::{ListedTool, ServerSpec};
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ServiceExt, RoleClient};
use serde_json::{Map, Value};

type Client = rmcp::service::RunningService<RoleClient, ()>;

pub fn list_tools(spec: &ServerSpec) -> Result<Vec<ListedTool>, String> {
    let spec = spec.clone();
    block(async move {
        let client = connect(&spec).await?;
        let tools = client.list_all_tools().await.map_err(|e| e.to_string())?;
        let _ = client.cancel().await;
        Ok(tools.into_iter().map(listed).collect())
    })
}

pub fn call_tool(
    spec: &ServerSpec,
    tool: &str,
    arguments: Value,
    timeout: Duration,
) -> Result<Value, String> {
    let spec = spec.clone();
    let tool = tool.to_string();
    block(async move {
        let client = connect(&spec).await?;
        let result = call_one(&client, &tool, arguments, timeout).await;
        let _ = client.cancel().await;
        result
    })
}

pub fn call_many(
    spec: &ServerSpec,
    calls: Vec<(String, Value)>,
    timeout: Duration,
    concurrency: usize,
) -> Result<Vec<Result<Value, String>>, String> {
    let spec = spec.clone();
    block(async move {
        let client = connect(&spec).await?;
        let conc = concurrency.max(1);
        let mut out = Vec::with_capacity(calls.len());
        for chunk in calls.chunks(conc) {
            if conc == 1 {
                for (name, args) in chunk {
                    out.push(call_one(&client, name, args.clone(), timeout).await);
                }
            } else {
                let futs = chunk
                    .iter()
                    .map(|(name, args)| call_one(&client, name, args.clone(), timeout));
                out.extend(join_all(futs).await);
            }
        }
        let _ = client.cancel().await;
        Ok(out)
    })
}

async fn call_one(client: &Client, tool: &str, arguments: Value, timeout: Duration) -> Result<Value, String> {
    let arguments = match arguments {
        Value::Object(map) => Some(map),
        Value::Null => None,
        other => {
            let mut map = Map::new();
            map.insert("value".into(), other);
            Some(map)
        }
    };
    let params = match arguments {
        Some(map) => CallToolRequestParams::new(tool.to_string()).with_arguments(map),
        None => CallToolRequestParams::new(tool.to_string()),
    };
    let result = tokio::time::timeout(timeout, client.call_tool(params))
        .await
        .map_err(|_| format!("tool {tool} timed out after {}ms", timeout.as_millis()))?
        .map_err(|e| e.to_string())?;
    value_of(result)
}

fn value_of(result: CallToolResult) -> Result<Value, String> {
    if result.is_error == Some(true) {
        // Prefix marks a tool failure so the gateway does not retry it.
        // Transport errors stay unprefixed and are retried.
        let text = text_of(&result.content).unwrap_or_else(|| "tool error".into());
        return Err(format!("mcp-tool: {text}"));
    }
    if let Some(value) = result.structured_content {
        return Ok(value);
    }
    let text = text_of(&result.content).unwrap_or_default();
    if text.is_empty() {
        return Ok(Value::Null);
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

fn text_of(content: &[ContentBlock]) -> Option<String> {
    let mut parts = Vec::new();
    for block in content {
        if let ContentBlock::Text(text) = block {
            parts.push(text.text.clone());
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn listed(tool: rmcp::model::Tool) -> ListedTool {
    let input = Value::Object((*tool.input_schema).clone());
    let output = tool
        .output_schema
        .as_ref()
        .map(|schema| Value::Object((**schema).clone()))
        .unwrap_or(Value::Null);
    let read_only_hint = tool
        .annotations
        .as_ref()
        .and_then(|a| a.read_only_hint)
        .unwrap_or(false);
    let schema_hash = mcp_core::schema_hash(&tool.name, &input, &output);
    ListedTool {
        name: tool.name.to_string(),
        description: tool.description.map(|d| d.to_string()).unwrap_or_default(),
        input_schema: input,
        output_schema: output,
        schema_hash,
        read_only_hint,
    }
}

async fn connect(spec: &ServerSpec) -> Result<Client, String> {
    let resolved = spec.resolve_secrets()?;
    match resolved.transport.as_str() {
        "stdio" => connect_stdio(&resolved).await,
        "http" => connect_http(&resolved).await,
        other => Err(format!("MCP transport {other} is not http or stdio")),
    }
}

async fn connect_stdio(spec: &ServerSpec) -> Result<Client, String> {
    let command = spec.command.clone().unwrap_or_default();
    let mut cmd = tokio::process::Command::new(&command);
    cmd.args(&spec.args);
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
    cmd.kill_on_drop(true);
    let transport = rmcp::transport::TokioChildProcess::new(cmd).map_err(|e| e.to_string())?;
    ().serve(transport).await.map_err(|e| e.to_string())
}

async fn connect_http(spec: &ServerSpec) -> Result<Client, String> {
    let url = spec.url.clone().unwrap_or_default();
    let mut headers = HashMap::new();
    for (name, value) in &spec.headers {
        let header_name = http::HeaderName::from_bytes(name.as_bytes()).map_err(|e| e.to_string())?;
        let header_value = http::HeaderValue::from_str(value).map_err(|e| e.to_string())?;
        headers.insert(header_name, header_value);
    }
    let config = StreamableHttpClientTransportConfig::with_uri(url).custom_headers(headers);
    let transport = rmcp::transport::StreamableHttpClientTransport::from_config(config);
    ().serve(transport).await.map_err(|e| e.to_string())
}

fn block<T: Send>(fut: impl std::future::Future<Output = Result<T, String>> + Send) -> Result<T, String> {
    runtime().block_on(fut)
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("tokio runtime"))
}
