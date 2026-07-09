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

use serde_json::json;

use composable::mcp::client::Session;
use composable::mcp::types::{
    AudioContent, CallToolPayload, CallToolRequest, CallToolResult, ContentBlock, EmbeddedResource,
    ImageContent, ListToolsPayload, ResourceContents, ResourceLink, TextContent, Tool,
    ToolAnnotations as McpToolAnnotations,
};
use composable::tools::types::{ToolAnnotations, ToolMetadata};

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
            .iter()
            .find(|t| t.name == tool_name)
            .map(tool_to_metadata)
            .ok_or_else(|| format!("tool '{tool_name}' not found on server"))
    }

    async fn call(input: String) -> Result<String, String> {
        let server_url = config_get("server-url")
            .ok_or_else(|| "missing required config: server-url".to_string())?;
        let tool_name = config_get("tool-name")
            .ok_or_else(|| "missing required config: tool-name".to_string())?;

        let session = Session::initialize(server_url, None).await?;

        let request = CallToolRequest {
            name: tool_name,
            arguments: Some(input),
            meta: None,
        };
        let response = session.call_tool(request).await;
        session.close().await;

        let response = response?;
        match response.payload {
            CallToolPayload::Result(result) => Ok(call_tool_result_to_json(&result).to_string()),
            CallToolPayload::Error(e) => {
                Err(format!("MCP protocol error {}: {}", e.code, e.message))
            }
        }
    }
}

// Read a configuration value by key, returning None when unset.
fn config_get(key: &str) -> Option<String> {
    wasi::config::store::get(key).ok().flatten()
}

// Map an MCP tools/list entry to composable:tools tool-metadata.
fn tool_to_metadata(tool: &Tool) -> ToolMetadata {
    ToolMetadata {
        name: tool.name.clone(),
        title: tool.title.clone(),
        description: tool.description.clone(),
        input_schema: tool.input_schema.clone(),
        output_schema: tool.output_schema.clone(),
        annotations: tool.annotations.as_ref().map(annotations_to_tools),
    }
}

fn annotations_to_tools(a: &McpToolAnnotations) -> ToolAnnotations {
    ToolAnnotations {
        title: a.title.clone(),
        read_only_hint: a.read_only_hint,
        destructive_hint: a.destructive_hint,
        idempotent_hint: a.idempotent_hint,
        open_world_hint: a.open_world_hint,
    }
}

// Serialize a WIT call-tool-result back to an MCP CallToolResult JSON object.
// The `structuredContent` is included when the remote server provides it.
fn call_tool_result_to_json(result: &CallToolResult) -> serde_json::Value {
    let content: Vec<serde_json::Value> =
        result.content.iter().map(content_block_to_json).collect();

    let mut obj = json!({
        "content": content,
        "isError": result.is_error,
    });

    if let Some(structured) = &result.structured_content {
        let value: serde_json::Value =
            serde_json::from_str(structured).unwrap_or(serde_json::Value::Null);
        obj["structuredContent"] = value;
    }

    obj
}

fn content_block_to_json(block: &ContentBlock) -> serde_json::Value {
    match block {
        ContentBlock::Text(TextContent { text, .. }) => json!({ "type": "text", "text": text }),
        ContentBlock::Image(ImageContent {
            data, mime_type, ..
        }) => json!({ "type": "image", "data": data, "mimeType": mime_type }),
        ContentBlock::Audio(AudioContent {
            data, mime_type, ..
        }) => json!({ "type": "audio", "data": data, "mimeType": mime_type }),
        ContentBlock::ResourceLink(ResourceLink {
            uri,
            name,
            description,
            mime_type,
            ..
        }) => {
            let mut obj = json!({ "type": "resource_link", "uri": uri });
            if let Some(name) = name {
                obj["name"] = json!(name);
            }
            if let Some(description) = description {
                obj["description"] = json!(description);
            }
            if let Some(mime_type) = mime_type {
                obj["mimeType"] = json!(mime_type);
            }
            obj
        }
        ContentBlock::Resource(EmbeddedResource { resource_data, .. }) => {
            json!({ "type": "resource", "resource": resource_contents_to_json(resource_data) })
        }
    }
}

fn resource_contents_to_json(contents: &ResourceContents) -> serde_json::Value {
    match contents {
        ResourceContents::Text(t) => {
            let mut obj = json!({ "uri": t.uri, "text": t.text });
            if let Some(mime_type) = &t.mime_type {
                obj["mimeType"] = json!(mime_type);
            }
            obj
        }
        ResourceContents::Blob(b) => {
            let mut obj = json!({ "uri": b.uri, "blob": b.blob });
            if let Some(mime_type) = &b.mime_type {
                obj["mimeType"] = json!(mime_type);
            }
            obj
        }
    }
}

export!(McpTool);
