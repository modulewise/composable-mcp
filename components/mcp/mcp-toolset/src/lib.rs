//! MCP toolset component.
//!
//! Exports `composable:tools/toolset` for interacting with the set of tools
//! exposed on a remote MCP server by using `mcp-client`. The target server URL
//! comes from `wasi:config`.

wit_bindgen::generate!({
    path: "../wit",
    world: "mcp-toolset",
    generate_all,
});

use composable::mcp::client::Session;
use composable::mcp::types::{CallToolPayload, ListToolsPayload};
use composable::tools::types::{CallToolRequest, CallToolResult, ToolError, ToolMetadata};

struct McpToolset;

impl exports::composable::tools::toolset::Guest for McpToolset {
    async fn list() -> Result<Vec<ToolMetadata>, String> {
        let server_url = config_get("server-url")
            .ok_or_else(|| "missing required config: server-url".to_string())?;

        let session = Session::initialize(server_url, None).await?;
        let response = session.list_tools(None).await;
        session.close().await;

        match response?.payload {
            ListToolsPayload::Result(result) => Ok(result.tools),
            ListToolsPayload::Error(e) => {
                Err(format!("MCP protocol error {}: {}", e.code, e.message))
            }
        }
    }

    async fn call(request: CallToolRequest) -> Result<CallToolResult, ToolError> {
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

export!(McpToolset);
