use std::cell::RefCell;

use serde_json::json;

wit_bindgen::generate!({
    world: "mcp-client",
    path: "../wit",
    generate_all,
});

use composable::http::client::HttpResponse;
use composable::mcp::types::*;
use wasi::logging::logging::{Level, log};
use wit_bindgen::rt::async_support::spawn_local;

struct Client;

impl exports::composable::mcp::client::Guest for Client {
    type Session = Session;
}

pub struct Session {
    server_url: String,
    init_response: InitializeResponse,
    // The session-id and protocol-version are extracted at construction time
    // to avoid cloning from the response's result variant on every call.
    session_id: String,
    protocol_version: String,
    // Monotonic id counter for JSON-RPC requests.
    request_id: RefCell<i32>,
}

impl exports::composable::mcp::client::GuestSession for Session {
    async fn initialize(
        server_url: String,
        request: Option<InitializeRequest>,
    ) -> Result<exports::composable::mcp::client::Session, String> {
        validate_server_url(&server_url)?;
        log_info(&format!(
            "Initializing MCP session with server: {}",
            server_url
        ));
        let init_response = initialize_session(&server_url, request)
            .await
            .inspect_err(|e| {
                log_error(&format!("Failed to initialize MCP session: {}", e));
            })?;
        // Any error on initialize returns an Err result, not an error payload.
        let init_result = match &init_response.payload {
            InitializePayload::Result(r) => r,
            InitializePayload::Error(e) => {
                let message = format!(
                    "Initialize returned protocol error {}: {}",
                    e.code, e.message
                );
                log_error(&message);
                return Err(message);
            }
        };
        let session_id = init_result.session_id.clone();
        let protocol_version = init_result.protocol_version.clone();
        log_info(&format!(
            "MCP session initialized, session_id: {}",
            session_id
        ));
        Ok(exports::composable::mcp::client::Session::new(Session {
            server_url,
            init_response,
            session_id,
            protocol_version,
            request_id: RefCell::new(1),
        }))
    }

    fn initialize_response(&self) -> InitializeResponse {
        self.init_response.clone()
    }

    async fn list_tools(
        &self,
        request: Option<ListToolsRequest>,
    ) -> Result<ListToolsResponse, String> {
        let request_id = self.next_request_id();
        log_info(&format!(
            "Listing tools from server: {}, session_id: {}, request_id: {}",
            self.server_url, self.session_id, request_id
        ));

        let mut params = json!({});
        if let Some(ref req) = request {
            if let Some(ref cursor) = req.cursor {
                params["cursor"] = json!(cursor);
            }
            if let Some(ref meta) = req.meta {
                params["_meta"] = meta_to_json(meta);
            }
        }

        let request_body = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/list",
            "params": params
        })
        .to_string();

        log_debug(&format!("tools/list request body: {}", request_body));

        let HttpResponse {
            status,
            headers,
            body,
            ..
        } = post(
            &self.server_url,
            request_body.as_bytes(),
            Some(&self.session_id),
            Some(&self.protocol_version),
        )
        .await?;

        let body = body.collect().await;
        let response_body = String::from_utf8_lossy(&body);
        log_debug(&format!(
            "tools/list response body length: {}",
            response_body.len()
        ));

        check_http_status(status, &response_body)?;

        let content_type = get_content_type(&headers);
        let envelope = parse_response(&response_body, &content_type)?;
        let id = envelope_id(&envelope, request_id)?;

        if let Some(error_json) = envelope.get("error") {
            log_error(&format!("tools/list returned error: {}", error_json));
            return Ok(ListToolsResponse {
                id,
                payload: ListToolsPayload::Error(parse_response_error(error_json)),
            });
        }

        let result = &envelope["result"];
        let tools_array = result["tools"].as_array().ok_or_else(|| {
            log_error("No tools array in response");
            "No tools array in response"
        })?;

        log_info(&format!("Found {} tools from server", tools_array.len()));

        let tools = tools_array
            .iter()
            .map(parse_tool)
            .collect::<Result<Vec<_>, _>>()?;

        let next_cursor = result
            .get("nextCursor")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let meta = parse_meta(result.get("_meta"));

        Ok(ListToolsResponse {
            id,
            payload: ListToolsPayload::Result(ListToolsResult {
                tools,
                next_cursor,
                meta,
            }),
        })
    }

    async fn call_tool(&self, request: CallToolRequest) -> Result<CallToolResponse, String> {
        let request_id = self.next_request_id();
        log_info(&format!(
            "Calling tool '{}' on server: {}, session_id: {}, request_id: {}",
            request.name, self.server_url, self.session_id, request_id
        ));
        log_debug(&format!("Tool arguments: {:?}", request.arguments));

        let mut params = json!({ "name": request.name });
        if let Some(ref args_str) = request.arguments {
            let args: serde_json::Value =
                serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
            params["arguments"] = args;
        }
        if let Some(ref meta) = request.meta {
            params["_meta"] = meta_to_json(meta);
        }

        let request_body = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": params
        })
        .to_string();

        log_debug(&format!("tools/call request body: {}", request_body));

        let HttpResponse {
            status,
            headers,
            body,
            ..
        } = post(
            &self.server_url,
            request_body.as_bytes(),
            Some(&self.session_id),
            Some(&self.protocol_version),
        )
        .await?;

        let body = body.collect().await;
        let response_body = String::from_utf8_lossy(&body);
        log_debug(&format!("tools/call response body: {}", response_body));

        check_http_status(status, &response_body)?;

        let content_type = get_content_type(&headers);
        let envelope = parse_response(&response_body, &content_type)?;
        let id = envelope_id(&envelope, request_id)?;

        if let Some(error_json) = envelope.get("error") {
            log_error(&format!("tools/call returned error: {}", error_json));
            return Ok(CallToolResponse {
                id,
                payload: CallToolPayload::Error(parse_response_error(error_json)),
            });
        }

        log_info(&format!(
            "Tool '{}' call completed successfully",
            request.name
        ));
        let result = &envelope["result"];
        let call_result = parse_call_tool_result(result)?;
        Ok(CallToolResponse {
            id,
            payload: CallToolPayload::Result(call_result),
        })
    }

    async fn close(&self) {
        log_info(&format!(
            "Terminating MCP session {} on server: {}",
            self.session_id, self.server_url
        ));

        let headers = vec![("MCP-Session-Id".to_string(), self.session_id.clone())];

        match composable::http::client::delete(self.server_url.clone(), headers, None).await {
            Ok(response) => {
                log_debug(&format!("Terminate response status: {}", response.status));
            }
            Err(e) => {
                log_error(&format!("Terminate request failed: {}", e));
            }
        }
    }
}

impl Session {
    fn next_request_id(&self) -> i32 {
        let mut id = self.request_id.borrow_mut();
        *id += 1;
        *id
    }
}

// POST to the MCP server with the standard Content-Type and Accept headers.
// Include the MCP-Session-Id and MCP-Protocol-Version headers when available.
// The request body is streamed via the http-client's stream<u8> input.
async fn post(
    url: &str,
    body: &[u8],
    session_id: Option<&str>,
    protocol_version: Option<&str>,
) -> Result<HttpResponse, String> {
    use composable::http::client;

    log_debug(&format!("HTTP POST to URL: {}", url));

    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        (
            "Accept".to_string(),
            "application/json, text/event-stream".to_string(),
        ),
    ];
    if let Some(sid) = session_id {
        headers.push(("MCP-Session-Id".to_string(), sid.to_string()));
    }
    if let Some(pv) = protocol_version {
        headers.push(("MCP-Protocol-Version".to_string(), pv.to_string()));
    }

    log_debug(&format!("HTTP POST headers: {:?}", headers));

    let (mut body_tx, body_rx) = wit_stream::new::<u8>();
    let body_bytes = body.to_vec();
    spawn_local(async move {
        let _ = body_tx.write_all(body_bytes).await;
    });

    let response = client::post(url.to_string(), headers, body_rx, None)
        .await
        .map_err(|e| {
            log_error(&format!("HTTP request failed: {}", e));
            e
        })?;

    log_debug(&format!("HTTP response status: {}", response.status));

    Ok(response)
}

fn validate_server_url(server_url: &str) -> Result<(), String> {
    url::Url::parse(server_url)
        .map(|_| ())
        .map_err(|e| format!("Invalid MCP server URL '{server_url}': {e}"))
}

#[allow(clippy::derivable_impls)]
impl Default for InitializeRequest {
    fn default() -> Self {
        Self {
            protocol_version: None,
            capabilities: None,
            client_info: None,
            meta: None,
        }
    }
}

// Initialize session with the MCP server.
async fn initialize_session(
    server_url: &str,
    request: Option<InitializeRequest>,
) -> Result<InitializeResponse, String> {
    log_debug(&format!(
        "Starting session initialization with server: {}",
        server_url
    ));

    let request = request.unwrap_or_default();

    let protocol_version = request
        .protocol_version
        .unwrap_or_else(|| "2025-06-18".to_string());

    let capabilities: serde_json::Value = request
        .capabilities
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_else(|| json!({}));

    let client_info = request.client_info.unwrap_or(Implementation {
        name: "composable-mcp-client".to_string(),
        version: "0.2.0".to_string(),
        title: None,
    });

    let mut client_info_json = json!({
        "name": client_info.name,
        "version": client_info.version,
    });
    if let Some(title) = &client_info.title {
        client_info_json["title"] = json!(title);
    }

    let mut params = json!({
        "protocolVersion": protocol_version,
        "capabilities": capabilities,
        "clientInfo": client_info_json
    });
    if let Some(ref meta) = request.meta {
        params["_meta"] = meta_to_json(meta);
    }

    let request_body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": params
    })
    .to_string();

    log_debug(&format!("Sending initialize request: {}", request_body));

    let HttpResponse {
        status,
        headers,
        body,
        ..
    } = post(server_url, request_body.as_bytes(), None, None).await?;

    let body = body.collect().await;
    let response_body = String::from_utf8_lossy(&body);
    log_debug(&format!(
        "Initialize response status: {}, body: {}",
        status, response_body
    ));

    if status != 200 {
        let message = format!("Initialize failed with status: {status}");
        log_error(&message);
        return Err(message);
    }

    log_debug(&format!("Response headers: {:?}", headers));
    let content_type = get_content_type(&headers);
    let envelope = parse_response(&response_body, &content_type)?;
    let id = envelope_id(&envelope, 1)?;

    // Protocol error returns Error payload.
    // No session-id, no initialized notification.
    if let Some(error_json) = envelope.get("error") {
        log_error(&format!("initialize returned error: {}", error_json));
        return Ok(InitializeResponse {
            id,
            payload: InitializePayload::Error(parse_response_error(error_json)),
        });
    }

    let session_id = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("mcp-session-id"))
        .map(|(_, value)| value.clone())
        .ok_or_else(|| {
            log_error("No MCP-Session-Id header in response");
            "No session ID in response".to_string()
        })?;

    log_debug(&format!("Got session ID: {}", session_id));

    let result = &envelope["result"];

    let server_protocol_version = result["protocolVersion"]
        .as_str()
        .ok_or("Missing protocolVersion in initialize result")?
        .to_string();

    let server_capabilities = result
        .get("capabilities")
        .map(|c| c.to_string())
        .unwrap_or_else(|| "{}".to_string());

    let server_info_json = result
        .get("serverInfo")
        .ok_or("Missing serverInfo in initialize result")?;
    let server_info = Implementation {
        name: server_info_json["name"]
            .as_str()
            .ok_or("Missing serverInfo.name")?
            .to_string(),
        version: server_info_json["version"]
            .as_str()
            .ok_or("Missing serverInfo.version")?
            .to_string(),
        title: server_info_json
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    };

    let instructions = result
        .get("instructions")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let meta = parse_meta(result.get("_meta"));

    // Send initialized notification.
    let notification_body = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    })
    .to_string();

    log_debug("Sending initialized notification");
    post(
        server_url,
        notification_body.as_bytes(),
        Some(&session_id),
        Some(&server_protocol_version),
    )
    .await?;

    log_debug("Session initialization complete");
    Ok(InitializeResponse {
        id,
        payload: InitializePayload::Result(InitializeResult {
            session_id,
            protocol_version: server_protocol_version,
            capabilities: server_capabilities,
            server_info,
            instructions,
            meta,
        }),
    })
}

// Check HTTP status code and return a descriptive error for non-success cases.
fn check_http_status(status: u16, body: &str) -> Result<(), String> {
    match status {
        200 => Ok(()),
        202 => Ok(()),
        400 => {
            log_error("Bad request (400)");
            Err(format!("Bad request: {}", body.trim()))
        }
        404 => {
            log_error("Session not found (404), must re-initialize");
            Err(
                "Session not found (404): session may have expired, re-initialize required"
                    .to_string(),
            )
        }
        405 => {
            log_error("Method not allowed (405)");
            Err("Method not allowed (405)".to_string())
        }
        406 => {
            log_error("Not acceptable (406)");
            Err(format!("Not acceptable: {}", body.trim()))
        }
        _ => {
            log_error(&format!("HTTP error {}", status));
            Err(format!("HTTP error {}: {}", status, body.trim()))
        }
    }
}

// Extract Content-Type from response headers.
fn get_content_type(headers: &[(String, String)]) -> String {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.clone())
        .unwrap_or_default()
}

// Parse an HTTP response body as a JSON-RPC response, handling both
// application/json and text/event-stream (SSE) response formats.
// Per the MCP spec, a client MUST support both.
fn parse_response(body: &str, content_type: &str) -> Result<serde_json::Value, String> {
    if content_type.contains("text/event-stream") {
        parse_sse_response(body)
    } else {
        let value: serde_json::Value = serde_json::from_str(body.trim()).map_err(|e| {
            log_error(&format!("Failed to parse JSON response: {}", e));
            e.to_string()
        })?;
        if is_jsonrpc_response(&value) {
            Ok(value)
        } else {
            log_error("JSON body was not a JSON-RPC response");
            Err("JSON body was not a JSON-RPC response".to_string())
        }
    }
}

// Extract the JSON-RPC envelope's `id` and verify it matches the id we sent.
fn envelope_id(envelope: &serde_json::Value, expected: i32) -> Result<i32, String> {
    let raw = envelope.get("id").ok_or("Missing JSON-RPC response id")?;
    let id = raw
        .as_i64()
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| format!("JSON-RPC response id is not a valid integer: {raw}"))?;
    if id != expected {
        return Err(format!(
            "JSON-RPC response id {id} does not match request id {expected}"
        ));
    }
    Ok(id)
}

// Parse a JSON-RPC error object into the WIT ResponseError record.
fn parse_response_error(error_json: &serde_json::Value) -> ResponseError {
    let code = error_json
        .get("code")
        .and_then(|v| v.as_i64())
        .and_then(|n| i32::try_from(n).ok())
        .unwrap_or(0);
    let message = error_json
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let data = error_json.get("data").map(|v| v.to_string());
    ResponseError {
        code,
        message,
        data,
    }
}

// True when `value` is a JSON-RPC response: has `result` or `error` and no
// `method` (which would indicate a request or notification).
fn is_jsonrpc_response(value: &serde_json::Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    !obj.contains_key("method") && (obj.contains_key("result") || obj.contains_key("error"))
}

// Parse an SSE response body and return the JSON-RPC response.
// Per the MCP streamable-HTTP transport, a server may emit notifications or
// server-initiated requests on the stream before the response, so those are
// skipped (logged at debug level). Per the WHATWG SSE spec, events are
// separated by blank lines and multiple `data:` lines within an event are
// concatenated with '\n'.
fn parse_sse_response(body: &str) -> Result<serde_json::Value, String> {
    let mut current: Vec<&str> = Vec::new();
    let mut events: Vec<String> = Vec::new();
    for line in body.lines() {
        if line.is_empty() {
            if !current.is_empty() {
                events.push(current.join("\n"));
                current.clear();
            }
        } else if let Some(data) = line.strip_prefix("data:") {
            current.push(data.strip_prefix(' ').unwrap_or(data));
        }
    }
    if !current.is_empty() {
        events.push(current.join("\n"));
    }

    for event in &events {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(event) else {
            log_debug(&format!("Skipping non-JSON SSE event data: {}", event));
            continue;
        };
        if is_jsonrpc_response(&value) {
            return Ok(value);
        }
        log_debug(&format!(
            "Skipping non-response JSON-RPC message in SSE stream: {}",
            value
        ));
    }

    log_error("No JSON-RPC response found in SSE stream");
    Err("No JSON-RPC response found in SSE stream".to_string())
}

// Parse MCP Tool JSON into WIT Tool structure.
fn parse_tool(tool_json: &serde_json::Value) -> Result<Tool, String> {
    Ok(Tool {
        name: tool_json["name"]
            .as_str()
            .ok_or("Missing tool name")?
            .to_string(),
        description: tool_json["description"].as_str().map(|s| s.to_string()),
        title: tool_json["title"].as_str().map(|s| s.to_string()),
        input_schema: tool_json["inputSchema"].to_string(),
        output_schema: tool_json.get("outputSchema").map(|s| s.to_string()),
        annotations: parse_tool_annotations(tool_json.get("annotations")),
        meta: parse_meta(tool_json.get("_meta")),
    })
}

// Parse the ToolAnnotations into the WIT record.
fn parse_tool_annotations(value: Option<&serde_json::Value>) -> Option<ToolAnnotations> {
    let obj = value?.as_object()?;
    Some(ToolAnnotations {
        title: obj
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        read_only_hint: obj.get("readOnlyHint").and_then(|v| v.as_bool()),
        destructive_hint: obj.get("destructiveHint").and_then(|v| v.as_bool()),
        idempotent_hint: obj.get("idempotentHint").and_then(|v| v.as_bool()),
        open_world_hint: obj.get("openWorldHint").and_then(|v| v.as_bool()),
    })
}

// Parse the typed Annotations from MCP spec into the WIT record.
fn parse_annotations(value: Option<&serde_json::Value>) -> Option<Annotations> {
    let obj = value?.as_object()?;
    let audience = obj.get("audience").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| match v.as_str()? {
                "user" => Some(Role::User),
                "assistant" => Some(Role::Assistant),
                _ => None,
            })
            .collect()
    });
    Some(Annotations {
        audience,
        priority: obj.get("priority").and_then(|v| v.as_f64()),
        last_modified: obj
            .get("lastModified")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

// Parse CallToolResult from MCP response.
fn parse_call_tool_result(result: &serde_json::Value) -> Result<CallToolResult, String> {
    let content_array = result["content"]
        .as_array()
        .ok_or("Missing content array")?;

    let content = content_array
        .iter()
        .map(parse_content_item)
        .collect::<Result<Vec<_>, _>>()?;

    // Extract structured content if present.
    let structured_content = result.get("structuredContent").map(|v| v.to_string());

    Ok(CallToolResult {
        content,
        is_error: result["isError"].as_bool().unwrap_or(false),
        structured_content,
        meta: parse_meta(result.get("_meta")),
    })
}

// Parse a single content item.
fn parse_content_item(item: &serde_json::Value) -> Result<ContentBlock, String> {
    match item["type"].as_str().ok_or("Missing content type")? {
        "text" => Ok(ContentBlock::Text(TextContent {
            text: item["text"].as_str().ok_or("Missing text")?.to_string(),
            annotations: parse_annotations(item.get("annotations")),
            meta: parse_meta(item.get("_meta")),
        })),
        "image" => Ok(ContentBlock::Image(ImageContent {
            data: item["data"]
                .as_str()
                .ok_or("Missing image data")?
                .to_string(),
            mime_type: item["mimeType"]
                .as_str()
                .ok_or("Missing mime type")?
                .to_string(),
            annotations: parse_annotations(item.get("annotations")),
            meta: parse_meta(item.get("_meta")),
        })),
        "audio" => Ok(ContentBlock::Audio(AudioContent {
            data: item["data"]
                .as_str()
                .ok_or("Missing audio data")?
                .to_string(),
            mime_type: item["mimeType"]
                .as_str()
                .ok_or("Missing mime type")?
                .to_string(),
            annotations: parse_annotations(item.get("annotations")),
            meta: parse_meta(item.get("_meta")),
        })),
        "resource_link" => Ok(ContentBlock::ResourceLink(ResourceLink {
            uri: item["uri"].as_str().ok_or("Missing URI")?.to_string(),
            name: item
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            description: item
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            mime_type: item
                .get("mimeType")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            annotations: parse_annotations(item.get("annotations")),
            meta: parse_meta(item.get("_meta")),
        })),
        "resource" => {
            let resource = item["resource"]
                .as_object()
                .ok_or("Missing resource object")?;
            let resource_value = serde_json::Value::Object(resource.clone());
            let resource_contents = if let Some(text) = resource.get("text") {
                ResourceContents::Text(TextResourceContents {
                    uri: resource["uri"].as_str().ok_or("Missing URI")?.to_string(),
                    mime_type: resource
                        .get("mimeType")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    text: text.as_str().ok_or("Missing text")?.to_string(),
                    meta: parse_meta(resource_value.get("_meta")),
                })
            } else if let Some(blob) = resource.get("blob") {
                ResourceContents::Blob(BlobResourceContents {
                    uri: resource["uri"].as_str().ok_or("Missing URI")?.to_string(),
                    mime_type: resource
                        .get("mimeType")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    blob: blob.as_str().ok_or("Missing blob")?.to_string(),
                    meta: parse_meta(resource_value.get("_meta")),
                })
            } else {
                return Err("Resource must have text or blob".to_string());
            };

            Ok(ContentBlock::Resource(EmbeddedResource {
                resource_data: resource_contents,
                annotations: parse_annotations(item.get("annotations")),
                meta: parse_meta(item.get("_meta")),
            }))
        }
        t => Err(format!("Unknown content type: {t}")),
    }
}

fn meta_to_json(meta: &[(String, String)]) -> serde_json::Value {
    let map: serde_json::Map<String, serde_json::Value> = meta
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    serde_json::Value::Object(map)
}

// Parse the spec's `_meta` JSON object into the WIT meta-entry list.
// String values are passed through. Other JSON values are serialized.
fn parse_meta(value: Option<&serde_json::Value>) -> Option<Vec<(String, String)>> {
    let obj = value?.as_object()?;
    Some(
        obj.iter()
            .map(|(k, v)| {
                let s = v
                    .as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| v.to_string());
                (k.clone(), s)
            })
            .collect(),
    )
}

fn log_debug(message: &str) {
    log(Level::Debug, "mcp-client", message);
}

fn log_info(message: &str) {
    log(Level::Info, "mcp-client", message);
}

fn log_error(message: &str) {
    log(Level::Error, "mcp-client", message);
}

export!(Client);
