//! MCP tool component.
//!
//! Exports `composable:tools/tool` for interacting with a single tool on a
//! remote MCP server by using `mcp-client`. The target server URL and the tool
//! name come from `wasi:config`.

wit_bindgen::generate!({
    path: "../wit",
    world: "mcp-tool",
    generate_all,
});

use composable::mcp::client::Session;
use composable::mcp::types::{CallToolPayload, CallToolRequest, ListToolsPayload};
use composable::tools::types::{CallToolResult, MetaEntry, ToolError, ToolMetadata};

struct McpTool;

impl exports::composable::tools::tool::Guest for McpTool {
    async fn metadata() -> Result<ToolMetadata, String> {
        let server_url = config_get("server-url")
            .ok_or_else(|| "missing required config: server-url".to_string())?;
        let tool_name = config_get("tool-name")
            .ok_or_else(|| "missing required config: tool-name".to_string())?;

        let session = Session::initialize(server_url, None).await?;
        let response = session.list_tools(None).await;
        session.close().await;

        let result = match response?.payload {
            ListToolsPayload::Result(result) => result,
            ListToolsPayload::Error(e) => {
                return Err(format!("MCP protocol error {}: {}", e.code, e.message));
            }
        };

        result
            .tools
            .into_iter()
            .find(|t| t.name == tool_name)
            .ok_or_else(|| format!("tool '{tool_name}' not found on server"))
    }

    async fn call(arguments: String, meta: Vec<MetaEntry>) -> Result<CallToolResult, ToolError> {
        let request = CallToolRequest {
            name: required_config("tool-name")?,
            arguments,
            meta,
        };

        let session = Session::initialize(required_config("server-url")?, None)
            .await
            .map_err(transport_error)?;
        let response = session.call_tool(request).await;
        session.close().await;

        match response.map_err(transport_error)?.payload {
            CallToolPayload::Result(result) => Ok(result),
            CallToolPayload::Error(e) => Err(ToolError {
                code: e.code,
                message: e.message,
                data: e.data,
            }),
        }
    }
}

// Read a configuration value by key, returning None when unset.
fn config_get(key: &str) -> Option<String> {
    wasi::config::store::get(key).ok().flatten()
}

// A required configuration value, as a `tool-error` when unset.
fn required_config(key: &str) -> Result<String, ToolError> {
    config_get(key).ok_or_else(|| ToolError {
        code: -32000,
        message: format!("missing required config: {key}"),
        data: None,
    })
}

// A failure to reach the server, as distinct from one the server reported.
fn transport_error(message: String) -> ToolError {
    ToolError {
        code: -32000,
        message,
        data: None,
    }
}

export!(McpTool);
