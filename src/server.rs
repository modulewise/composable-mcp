//! MCP server: serves `tools/list` and `tools/call` over Streamable HTTP.
//!
//! On `tools/call`, the request flow is:
//!
//! 1. Build inbound Message from `arguments` (body) and `_meta` selected
//!    by `propagate-request-meta` (headers).
//! 2. For component-backed tools, the resolved `MessageMapper` applies
//!    the inbound side of the WIT-bridging pipeline (`param-mapping`,
//!    `param-encoding`), invokes the function, then applies the outbound
//!    side (`result-decoding`, `result-mapping`) to produce a reply
//!    Message. For channel-backed tools, the Message is published and
//!    the reply Message is awaited. The subscription on the other side
//!    runs the same pipeline.
//! 3. Build the `CallToolResult` from the reply Message: body becomes
//!    `structuredContent`, and headers selected by
//!    `propagate-result-meta` are emitted as `_meta` entries (also on
//!    the error path).
//!
//! Trace context is propagated through `_meta` `traceparent` on the
//! request side and through the `PROPAGATION_CONTEXT` task-local on the
//! component side.

use anyhow::Result;
use opentelemetry::KeyValue;
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{Span, SpanKind, Status, Tracer, TracerProvider as _};
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{BatchSpanProcessor, SdkTracerProvider};
use rmcp::{
    ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, Content, InitializeRequestParams, InitializeResult,
        JsonObject, ListToolsResult, Meta, PaginatedRequestParams, ServerCapabilities, ServerInfo,
        Tool,
    },
    service::{RequestContext, RoleServer},
    transport::StreamableHttpService,
    transport::streamable_http_server::session::local::LocalSessionManager,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::watch;

use crate::origin::{OriginPolicy, validate_origin};
use crate::service::{ResolvedTool, ResolvedToolTarget};
use composable_runtime::{
    ComponentHost, Message, MessageBuilder, MessageHeaders, MessagePublisher, PROPAGATED_HEADERS,
    PROPAGATION_CONTEXT, PropagatedHeader, PropagationContext, Val, schema,
};

#[derive(Clone)]
pub struct McpServer {
    tools: HashMap<String, ResolvedTool>,
    component_host: Arc<dyn ComponentHost>,
    publisher: Option<Arc<dyn MessagePublisher>>,
    addr: SocketAddr,
    origin_policy: OriginPolicy,
    tracer_provider: Option<Arc<SdkTracerProvider>>,
}

impl McpServer {
    pub fn new(
        tools: HashMap<String, ResolvedTool>,
        component_host: Arc<dyn ComponentHost>,
        publisher: Option<Arc<dyn MessagePublisher>>,
        addr: SocketAddr,
        origin_policy: OriginPolicy,
        tracer_provider: Option<SdkTracerProvider>,
    ) -> Self {
        Self {
            tools,
            component_host,
            publisher,
            addr,
            origin_policy,
            tracer_provider: tracer_provider.map(Arc::new),
        }
    }

    /// Run the MCP server, listening for HTTP requests until the shutdown signal fires.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let addr = self.addr;
        let origin_policy = self.origin_policy.clone();
        // Keep a handle to the tracer provider for shutdown.
        let tracer_provider = self.tracer_provider.clone();

        let service = StreamableHttpService::new(
            move || Ok(self.clone()),
            LocalSessionManager::default().into(),
            Default::default(),
        );

        let router = axum::Router::new().nest_service("/mcp", service).layer(
            axum::middleware::from_fn_with_state(origin_policy, validate_origin),
        );
        let tcp_listener = tokio::net::TcpListener::bind(addr).await?;

        tracing::info!("Streamable HTTP endpoint: http://{addr}/mcp");

        tokio::select! {
            result = axum::serve(tcp_listener, router.into_make_service_with_connect_info::<SocketAddr>()) => {
                if let Err(err) = result {
                    tracing::error!("Server error: {err}");
                }
            }
            _ = shutdown.changed() => {
                tracing::info!("MCP server on {addr} shutting down");
            }
        }

        // Shutdown via spawn_blocking since BatchSpanProcessor.shutdown() calls block_on.
        if let Some(provider) = tracer_provider {
            let _ = tokio::task::spawn_blocking(move || provider.shutdown()).await;
        }

        Ok(())
    }

    // Create an MCP server span following the gen_ai semantic conventions.
    // Returns the span and a propagation context map derived from it.
    //
    // Trace context is extracted from `_meta`.
    fn start_mcp_span(
        &self,
        method: &str,
        target: Option<&str>,
        mut attributes: Vec<KeyValue>,
        meta: Option<&Meta>,
    ) -> Option<(opentelemetry_sdk::trace::Span, HashMap<String, String>)> {
        let tp = self.tracer_provider.as_ref()?;
        let tracer = tp.tracer("modulewise.composable.mcp.server");

        let span_name = match target {
            Some(t) => format!("{method} {t}"),
            None => method.to_string(),
        };

        // Extract propagated context from _meta (MCP spec trace propagation).
        let mut context: HashMap<String, String> = HashMap::new();
        if let Some(m) = meta {
            for key in PROPAGATED_HEADERS {
                if let Some(val) = m.0.get(*key).and_then(|v| v.as_str()) {
                    context.insert(key.to_string(), val.to_string());
                }
            }
        }

        let parent_cx = if context.contains_key("traceparent") {
            Some(TraceContextPropagator::new().extract(&context))
        } else {
            None
        };

        attributes.push(KeyValue::new("mcp.method.name", method.to_string()));

        let builder = tracer
            .span_builder(span_name)
            .with_kind(SpanKind::Server)
            .with_attributes(attributes);

        let span = match parent_cx {
            Some(cx) => builder.start_with_context(&tracer, &cx),
            None => builder.start(&tracer),
        };

        // Derive traceparent from the span.
        let sc = span.span_context().clone();
        context.insert(
            "traceparent".to_string(),
            format!(
                "00-{:032x}-{:016x}-{:02x}",
                sc.trace_id(),
                sc.span_id(),
                sc.trace_flags()
            ),
        );

        Some((span, context))
    }

    async fn handle_tool_call(
        &self,
        tool_name: &str,
        arguments: &JsonObject,
        meta: Option<&Meta>,
        context: Option<HashMap<String, String>>,
    ) -> CallToolResult {
        let Some(resolved) = self.tools.get(tool_name) else {
            return CallToolResult::error(vec![Content::text(format!(
                "Tool not found: {tool_name}"
            ))]);
        };

        let args_value = serde_json::Value::Object(arguments.clone());
        if let Err(error) = resolved.input_validator.validate(&args_value) {
            return CallToolResult::error(vec![Content::text(format!(
                "Invalid arguments for tool '{tool_name}': {error}"
            ))]);
        }

        // Build the inbound Message:
        //   - body = arguments as JSON
        //   - headers = declared propagate-request-meta entries
        //     (plus the tracing keys merged within MessageBuilder)
        let dispatch = async {
            let message = match build_message_from_mcp_call(
                arguments,
                meta,
                &resolved.propagate_request_meta,
            ) {
                Ok(m) => m,
                Err(e) => return CallToolResult::error(vec![Content::text(e)]),
            };

            let reply = match &resolved.target {
                ResolvedToolTarget::Component {
                    component_name,
                    mapper,
                } => {
                    let invocation = match mapper.to_invocation(&message) {
                        Ok(inv) => inv,
                        Err(e) => return CallToolResult::error(vec![Content::text(e)]),
                    };
                    let wit_result = match self
                        .component_host
                        .invoke(
                            component_name,
                            invocation.function_key.as_str(),
                            invocation.args.into_iter().map(Val::Json).collect(),
                            None,
                        )
                        .await
                    {
                        Ok(v) => v,
                        Err(e) => {
                            return CallToolResult::error(vec![Content::text(e.to_string())]);
                        }
                    };
                    // The reply mapper is JSON-based.
                    let wit_result = match wit_result {
                        Some(value) => match value.into_json() {
                            Ok(json) => json,
                            Err(e) => {
                                return CallToolResult::error(vec![Content::text(e.to_string())]);
                            }
                        },
                        None => serde_json::Value::Null,
                    };
                    // Propagation entries (PROPAGATED_HEADERS) read from the
                    // inbound Message carry into the reply Message.
                    let propagated = propagated_headers(&message);
                    match mapper.from_invocation_result(&wit_result, propagated) {
                        Ok(m) => m,
                        Err(e) => return CallToolResult::error(vec![Content::text(e)]),
                    }
                }
                ResolvedToolTarget::Channel { channel } => {
                    let Some(publisher) = &self.publisher else {
                        return CallToolResult::error(vec![Content::text(
                            "Channel-backed tools require messaging support".to_string(),
                        )]);
                    };
                    let return_address = match publisher.publish_request(channel, message).await {
                        Ok(ra) => ra,
                        Err(e) => {
                            return CallToolResult::error(vec![Content::text(format!(
                                "Failed to publish to channel '{channel}': {e}"
                            ))]);
                        }
                    };
                    match return_address.take().await {
                        Ok(reply) => reply,
                        Err(e) => {
                            return CallToolResult::error(vec![Content::text(format!(
                                "Failed to receive reply for request to channel '{channel}': {e}"
                            ))]);
                        }
                    }
                }
            };

            build_mcp_result_from_message(
                reply,
                &resolved.tool,
                &resolved.output_validator,
                &resolved.propagate_result_meta,
            )
        };

        // Establish the propagation scope around dispatch so any Message
        // construction in the scope picks up tracing keys via MessageBuilder's
        // task-local auto-merge, and so downstream invocation has the context.
        match context {
            Some(entries) if !entries.is_empty() => {
                let ctx = PropagationContext { entries };
                PROPAGATION_CONTEXT.scope(Some(ctx), dispatch).await
            }
            _ => dispatch.await,
        }
    }
}

// Extract PROPAGATED_HEADERS from a Message into a HashMap.
fn propagated_headers(msg: &Message) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for key in PROPAGATED_HEADERS {
        if let Some(val) = msg.headers().get::<&str>(key) {
            out.insert((*key).to_string(), val.to_string());
        }
    }
    out
}

// Build a `Message` from an MCP `tools/call` invocation.
//
// - Body: `arguments` serialized as JSON, content-type `application/json`.
// - Headers: for each `propagate-request-meta` entry, look up the source
//   `_meta` key on the request and write its string value into the Message
//   headers under the entry's target name (the source if no rename).
//   Non-string `_meta` values are skipped with a warning.
//
// Well-known tracing keys (PROPAGATED_HEADERS) are not handled by this
// helper. They flow via `MessageBuilder::build`'s task-local auto-merge
// when the caller has established a `PROPAGATION_CONTEXT` scope around the
// build site.
fn build_message_from_mcp_call(
    arguments: &JsonObject,
    meta: Option<&Meta>,
    propagate_request_meta: &[PropagatedHeader],
) -> Result<Message, String> {
    let body = serde_json::to_vec(arguments)
        .map_err(|e| format!("failed to serialize MCP arguments: {e}"))?;

    let mut builder =
        MessageBuilder::new(body).header(MessageHeaders::CONTENT_TYPE, "application/json");

    if let Some(m) = meta {
        for entry in propagate_request_meta {
            match m.0.get(entry.source()) {
                Some(serde_json::Value::String(s)) => {
                    builder = builder.header(entry.target(), s.as_str());
                }
                Some(other) => {
                    tracing::warn!(
                        meta_key = %entry.source(),
                        value_type = ?other,
                        "skipping non-string _meta entry declared in propagate-request-meta"
                    );
                }
                None => {}
            }
        }
    }

    Ok(builder.build())
}

// Build a `CallToolResult` from a reply `Message`.
//
// The reply Message body is expected to be JSON. MCP publishes channel
// requests as JSON, and the subscription activator's
// `from_invocation_result` produces a JSON-bodied reply.
//
// Behavior:
// - If `tool.output_schema` is `None`: a text-content success result with
//   the body as the raw string when it is a JSON string, otherwise a
//   pretty-printed JSON form.
// - If `tool.output_schema` is `Some`: parse the body as JSON, apply
//   tolerant-reader coercion (`schema::coerce_value`), validate against the
//   provided `output_validator` if present, and return a structured success
//   result. When the output schema is a single `array`-typed or `oneOf`
//   property, the body value is wrapped under that property name.
//
// `result<T, E>` to `isError` mapping is NOT handled here. The runtime's
// `invoke` already returns `Err` for the WIT `err` arm; that surfaces in
// the caller as a dispatch-level error, not as a payload to this helper.
fn build_mcp_result_from_message(
    msg: Message,
    tool: &Tool,
    output_validator: &Option<jsonschema::Validator>,
    propagate_result_meta: &[PropagatedHeader],
) -> CallToolResult {
    // Compute the _meta entries to emit on the result, reading source Message
    // headers and writing them under the entry's target name.
    let mut result_meta_map = serde_json::Map::new();
    for entry in propagate_result_meta {
        if let Some(val) = msg.headers().get::<&str>(entry.source()) {
            result_meta_map.insert(
                entry.target().to_string(),
                serde_json::Value::String(val.to_string()),
            );
        }
    }
    let result_meta = if result_meta_map.is_empty() {
        None
    } else {
        Some(rmcp::model::Meta(result_meta_map))
    };

    let parsed: serde_json::Value = if msg.body().is_empty() {
        serde_json::Value::Null
    } else {
        match serde_json::from_slice(msg.body()) {
            Ok(v) => v,
            Err(e) => {
                return apply_meta(
                    CallToolResult::error(vec![Content::text(format!(
                        "reply body is not valid JSON: {e}"
                    ))]),
                    result_meta,
                );
            }
        }
    };

    let Some(schema) = tool.output_schema.as_ref() else {
        let text = match &parsed {
            serde_json::Value::String(s) => s.clone(),
            other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
        };
        return apply_meta(
            CallToolResult::success(vec![Content::text(text)]),
            result_meta,
        );
    };

    let schema_value = serde_json::Value::Object((**schema).clone());

    // McpMapper (schema-time) wraps WIT return types that JSON-render as
    // a bare array or variant (oneOf) under a property name, since MCP
    // `structuredContent` must be a JSON object. The property name comes
    // from a pluralized item type name when available, or a literal
    // fallback (`items`, `tuple`, `result`). When the advertised schema
    // shows that wrapping (a single array-typed or oneOf-shaped
    // property), wrap the runtime body the same way so coercion and
    // validation operate on a value whose shape matches the schema.
    let mut wrapped = if let Some(properties) =
        schema_value.get("properties").and_then(|p| p.as_object())
        && properties.len() == 1
        && let Some((property_name, property_schema)) = properties.iter().next()
        && (property_schema.get("type").and_then(|t| t.as_str()) == Some("array")
            || property_schema.get("oneOf").is_some())
    {
        serde_json::json!({ property_name: parsed })
    } else {
        parsed
    };

    if let Err(e) = schema::coerce_value(&mut wrapped, &schema_value) {
        return apply_meta(
            CallToolResult::error(vec![Content::text(format!(
                "failed to coerce reply body to output-schema: {e}"
            ))]),
            result_meta,
        );
    }

    if let Some(validator) = output_validator
        && let Err(error) = validator.validate(&wrapped)
    {
        return apply_meta(
            CallToolResult::error(vec![Content::text(format!(
                "reply body does not conform to output-schema: {error}"
            ))]),
            result_meta,
        );
    }

    apply_meta(CallToolResult::structured(wrapped), result_meta)
}

// Attach the optional Meta to a CallToolResult.
fn apply_meta(mut result: CallToolResult, meta: Option<rmcp::model::Meta>) -> CallToolResult {
    if meta.is_some() {
        result.meta = meta;
    }
    result
}

// Extract gen_ai semantic convention attributes from the request context.
fn request_attributes(context: &RequestContext<RoleServer>) -> Vec<KeyValue> {
    let mut attrs = vec![
        KeyValue::new("jsonrpc.request.id", context.id.to_string()),
        KeyValue::new("network.transport", "tcp"),
        KeyValue::new("network.protocol.name", "http"),
    ];

    if let Some(parts) = context.extensions.get::<axum::http::request::Parts>() {
        if let Some(session_id) = parts
            .headers
            .get("MCP-Session-Id")
            .and_then(|v| v.to_str().ok())
        {
            attrs.push(KeyValue::new("mcp.session.id", session_id.to_string()));
        }
        if let Some(version) = parts
            .headers
            .get("MCP-Protocol-Version")
            .and_then(|v| v.to_str().ok())
        {
            attrs.push(KeyValue::new("mcp.protocol.version", version.to_string()));
        }
        if let Some(connect_info) = parts
            .extensions
            .get::<axum::extract::ConnectInfo<SocketAddr>>()
        {
            attrs.push(KeyValue::new(
                "client.address",
                connect_info.0.ip().to_string(),
            ));
            attrs.push(KeyValue::new("client.port", connect_info.0.port() as i64));
        }
    }

    attrs
}

impl ServerHandler for McpServer {
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tool_name = &request.name;
        let arguments = request.arguments.unwrap_or_default();

        // rmcp extracts _meta from params during deserialization and places it
        // in RequestContext.meta, not in CallToolRequestParams.meta.
        let meta = if context.meta.0.is_empty() {
            None
        } else {
            Some(&context.meta)
        };

        let mut attrs = vec![
            KeyValue::new("gen_ai.operation.name", "execute_tool"),
            KeyValue::new("gen_ai.tool.name", tool_name.to_string()),
        ];
        attrs.extend(request_attributes(&context));

        let span_ctx = self.start_mcp_span("tools/call", Some(tool_name), attrs, meta);

        let context = span_ctx.as_ref().map(|(_, ctx)| ctx.clone());

        let (mut span, result) = {
            let result = self
                .handle_tool_call(tool_name, &arguments, meta, context)
                .await;
            (span_ctx.map(|(span, _)| span), result)
        };

        if let Some(ref mut span) = span {
            if result.is_error.unwrap_or(false) {
                span.set_status(Status::error(""));
                span.set_attribute(KeyValue::new("error.type", "tool_error"));
            }
            span.end();
        }

        Ok(result)
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, rmcp::ErrorData> {
        let meta = if context.meta.0.is_empty() {
            None
        } else {
            Some(&context.meta)
        };
        let span_ctx = self.start_mcp_span("initialize", None, request_attributes(&context), meta);

        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        let result = self.get_info();

        if let Some((mut span, _)) = span_ctx {
            span.end();
        }

        Ok(result)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let meta = if context.meta.0.is_empty() {
            None
        } else {
            Some(&context.meta)
        };
        let span_ctx = self.start_mcp_span("tools/list", None, request_attributes(&context), meta);

        let tools = self.tools.values().map(|r| r.tool.clone()).collect();
        let result = ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        };

        if let Some((mut span, _)) = span_ctx {
            span.end();
        }

        Ok(result)
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                rmcp::model::Implementation::new("modulewise-toolbelt", env!("CARGO_PKG_VERSION"))
                    .with_title("Modulewise Toolbelt")
                    .with_website_url("https://github.com/modulewise/composable-mcp"),
            )
            .with_instructions(format!(
                "This server provides {} tools. \
                Each tool has typed inputs and outputs described by its schema. \
                Call tools with their required parameters.",
                self.tools.len()
            ))
    }
}

pub fn build_tracer_provider(
    endpoint: &str,
    protocol: &str,
    service_name: &str,
) -> Result<SdkTracerProvider> {
    let exporter = match protocol {
        "http/protobuf" => SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build span exporter: {e}"))?,
        _ => {
            if protocol != "grpc" {
                tracing::warn!(protocol, "unrecognized OTLP protocol, defaulting to grpc");
            }
            SpanExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint)
                .build()
                .map_err(|e| anyhow::anyhow!("failed to build span exporter: {e}"))?
        }
    };
    let resource = opentelemetry_sdk::Resource::builder()
        .with_attribute(opentelemetry::KeyValue::new(
            "service.name",
            service_name.to_string(),
        ))
        .build();
    let processor = BatchSpanProcessor::builder(exporter).build();
    Ok(SdkTracerProvider::builder()
        .with_resource(resource)
        .with_span_processor(processor)
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapper::McpMapper;
    use composable_runtime::Runtime;
    use rmcp::model::ClientInfo;
    use rmcp::{ClientHandler, ServiceExt};
    use std::io::Write as _;
    use tempfile::Builder;

    macro_rules! args {
        ($($json:tt)+) => {
            serde_json::json!($($json)+).as_object().unwrap().clone()
        };
    }

    fn meta(value: serde_json::Value) -> Meta {
        Meta(value.as_object().unwrap().clone())
    }

    #[test]
    fn build_message_serializes_arguments_as_json_body() {
        let arguments = args!({ "x": 5, "y": "hi" });
        let msg = build_message_from_mcp_call(&arguments, None, &[]).unwrap();
        assert_eq!(msg.headers().content_type(), Some("application/json"));
        let parsed: serde_json::Value = serde_json::from_slice(msg.body()).unwrap();
        assert_eq!(parsed, serde_json::json!({ "x": 5, "y": "hi" }));
    }

    #[test]
    fn build_message_attaches_declared_propagate_meta_entries() {
        let arguments = args!({});
        let m = meta(serde_json::json!({
            "io.example.tools/foo": "bar",
            "com.example.auth/token": "tok-123",
            "dev.unrelated/key": "ignored"
        }));
        let propagate = vec![
            PropagatedHeader::parse("io.example.tools/foo").unwrap(),
            PropagatedHeader::parse("com.example.auth/token").unwrap(),
        ];
        let msg = build_message_from_mcp_call(&arguments, Some(&m), &propagate).unwrap();
        assert_eq!(
            msg.headers().get::<&str>("io.example.tools/foo"),
            Some("bar")
        );
        assert_eq!(
            msg.headers().get::<&str>("com.example.auth/token"),
            Some("tok-123")
        );
        assert!(msg.headers().get::<&str>("dev.unrelated/key").is_none());
    }

    #[test]
    fn build_message_skips_non_string_propagate_meta_entries() {
        let arguments = args!({});
        let m = meta(serde_json::json!({
            "io.example/object-value": { "nested": 1 },
            "io.example/string-value": "ok"
        }));
        let propagate = vec![
            PropagatedHeader::parse("io.example/object-value").unwrap(),
            PropagatedHeader::parse("io.example/string-value").unwrap(),
        ];
        let msg = build_message_from_mcp_call(&arguments, Some(&m), &propagate).unwrap();
        assert!(
            msg.headers()
                .get::<&str>("io.example/object-value")
                .is_none()
        );
        assert_eq!(
            msg.headers().get::<&str>("io.example/string-value"),
            Some("ok")
        );
    }

    #[test]
    fn build_message_with_no_meta_and_no_propagate() {
        let arguments = args!({ "a": 1 });
        let msg = build_message_from_mcp_call(&arguments, None, &[]).unwrap();
        // body and content-type only (plus id + timestamp from MessageBuilder).
        assert_eq!(msg.headers().content_type(), Some("application/json"));
        let parsed: serde_json::Value = serde_json::from_slice(msg.body()).unwrap();
        assert_eq!(parsed, serde_json::json!({ "a": 1 }));
    }

    // Build a Tool with the given output schema. Input schema is a trivial
    // empty object since these tests don't exercise input validation.
    fn tool_with_output_schema(output_schema: Option<serde_json::Value>) -> Tool {
        let input_schema = serde_json::json!({ "type": "object" })
            .as_object()
            .unwrap()
            .clone();
        let mut tool = Tool::new_with_raw("test".to_string(), Some("test".into()), input_schema)
            .with_title("test".to_string());
        if let Some(schema) = output_schema.and_then(|s| s.as_object().cloned()) {
            tool = tool.with_raw_output_schema(schema.into());
        }
        tool
    }

    // Build a Message with the given JSON body.
    fn reply_msg(body: serde_json::Value) -> Message {
        let bytes = serde_json::to_vec(&body).unwrap();
        MessageBuilder::new(bytes)
            .header(MessageHeaders::CONTENT_TYPE, "application/json")
            .build()
    }

    #[test]
    fn build_result_no_schema_returns_text() {
        let tool = tool_with_output_schema(None);
        let msg = reply_msg(serde_json::json!({ "value": 42 }));
        let r = build_mcp_result_from_message(msg, &tool, &None, &[]);
        assert!(!r.is_error.unwrap_or(false));
        let text = r.content[0].as_text().unwrap().text.clone();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, serde_json::json!({ "value": 42 }));
    }

    #[test]
    fn build_result_no_schema_returns_string_body_raw() {
        let tool = tool_with_output_schema(None);
        let msg = reply_msg(serde_json::json!("hello, world"));
        let r = build_mcp_result_from_message(msg, &tool, &None, &[]);
        let text = r.content[0].as_text().unwrap().text.clone();
        // A JSON string body returns the raw string, not JSON-quoted.
        assert_eq!(text, "hello, world");
    }

    #[test]
    fn build_result_with_object_schema_returns_structured() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        });
        let tool = tool_with_output_schema(Some(schema));
        let msg = reply_msg(serde_json::json!({ "name": "Alice" }));
        let r = build_mcp_result_from_message(msg, &tool, &None, &[]);
        assert!(!r.is_error.unwrap_or(false));
        let structured = r.structured_content.unwrap();
        assert_eq!(structured, serde_json::json!({ "name": "Alice" }));
    }

    #[test]
    fn build_result_coerces_via_schema_then_returns_structured() {
        // Schema declares `kv` as string; body has it as an object.
        // schema::coerce_value stringifies that nested value.
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "kv": { "type": "string" }
            }
        });
        let tool = tool_with_output_schema(Some(schema));
        let msg = reply_msg(serde_json::json!({
            "name": "Alice",
            "kv": { "k": 1 }
        }));
        let r = build_mcp_result_from_message(msg, &tool, &None, &[]);
        assert!(!r.is_error.unwrap_or(false));
        let structured = r.structured_content.unwrap();
        assert_eq!(structured["name"], serde_json::json!("Alice"));
        assert_eq!(structured["kv"], serde_json::json!("{\"k\":1}"));
    }

    #[test]
    fn build_result_with_oneof_wrapper_schema_and_validator_succeeds() {
        // Wrapper shape from McpMapper for `option<T>`: single property
        // (`result`) whose schema is oneOf [T, null]. The body returned
        // by the WIT call is the bare oneOf value (e.g. a string or null);
        // detection + wrap must produce `{ "result": <body> }` so it
        // validates against the wrapped schema.
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "result": {
                    "oneOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                }
            },
            "required": ["result"]
        });
        let validator = Some(jsonschema::validator_for(&schema).unwrap());
        let tool = tool_with_output_schema(Some(schema));
        let body = serde_json::json!("hello");
        let msg = reply_msg(body.clone());
        let r = build_mcp_result_from_message(msg, &tool, &validator, &[]);
        assert!(
            !r.is_error.unwrap_or(false),
            "expected success, got error: {:?}",
            r.content
                .first()
                .and_then(|c| c.as_text().map(|t| t.text.clone())),
        );
        let structured = r.structured_content.unwrap();
        assert_eq!(structured, serde_json::json!({ "result": body }));
    }

    #[test]
    fn build_result_with_array_wrapper_schema_and_validator_succeeds() {
        // A WIT `list<T>` returns a bare array, but the schema advertised to
        // MCP is the wrapped object (single property of array type).
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "users": {
                    "type": "array",
                    "items": { "type": "object" }
                }
            },
            "required": ["users"]
        });
        let validator = Some(jsonschema::validator_for(&schema).unwrap());
        let tool = tool_with_output_schema(Some(schema));
        let body = serde_json::json!([{ "name": "Alice" }, { "name": "Bob" }]);
        let msg = reply_msg(body.clone());
        let r = build_mcp_result_from_message(msg, &tool, &validator, &[]);
        assert!(
            !r.is_error.unwrap_or(false),
            "expected success, got error: {:?}",
            r.content
                .first()
                .and_then(|c| c.as_text().map(|t| t.text.clone())),
        );
        let structured = r.structured_content.unwrap();
        assert_eq!(structured, serde_json::json!({ "users": body }));
    }

    #[test]
    fn build_result_with_validator_rejects_non_conformant() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        });
        let validator = Some(jsonschema::validator_for(&schema).unwrap());
        let tool = tool_with_output_schema(Some(schema));
        // Body lacks the required `name` property.
        let msg = reply_msg(serde_json::json!({ "wrong": "field" }));
        let r = build_mcp_result_from_message(msg, &tool, &validator, &[]);
        assert!(r.is_error.unwrap_or(false));
        let text = r.content[0].as_text().unwrap().text.clone();
        assert!(
            text.contains("does not conform to output-schema"),
            "unexpected error text: {text}"
        );
    }

    #[test]
    fn build_result_with_validator_accepts_conformant() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        });
        let validator = Some(jsonschema::validator_for(&schema).unwrap());
        let tool = tool_with_output_schema(Some(schema));
        let msg = reply_msg(serde_json::json!({ "name": "Alice" }));
        let r = build_mcp_result_from_message(msg, &tool, &validator, &[]);
        assert!(!r.is_error.unwrap_or(false));
    }

    #[test]
    fn build_result_invalid_json_body_is_error() {
        let tool = tool_with_output_schema(None);
        let msg = MessageBuilder::new(b"not json".to_vec())
            .header(MessageHeaders::CONTENT_TYPE, "application/json")
            .build();
        let r = build_mcp_result_from_message(msg, &tool, &None, &[]);
        assert!(r.is_error.unwrap_or(false));
        let text = r.content[0].as_text().unwrap().text.clone();
        assert!(
            text.contains("reply body is not valid JSON"),
            "unexpected error text: {text}"
        );
    }

    #[test]
    fn build_result_propagates_result_meta_identity() {
        let tool = tool_with_output_schema(None);
        // Reply Message carries `x-ratelimit-remaining` as a header;
        // propagate-result-meta lifts it onto the CallToolResult `_meta` under
        // the same name.
        let msg = MessageBuilder::new(b"{}".to_vec())
            .header(MessageHeaders::CONTENT_TYPE, "application/json")
            .header("x-ratelimit-remaining", "42")
            .build();
        let propagate = vec![PropagatedHeader::parse("x-ratelimit-remaining").unwrap()];
        let r = build_mcp_result_from_message(msg, &tool, &None, &propagate);
        let meta = r.meta.expect("expected result.meta to be set");
        assert_eq!(
            meta.0.get("x-ratelimit-remaining"),
            Some(&serde_json::Value::String("42".to_string()))
        );
    }

    #[test]
    fn build_result_propagates_result_meta_with_rename() {
        let tool = tool_with_output_schema(None);
        let msg = MessageBuilder::new(b"{}".to_vec())
            .header(MessageHeaders::CONTENT_TYPE, "application/json")
            .header("x-ratelimit-remaining", "42")
            .build();
        let propagate = vec![
            PropagatedHeader::parse(
                "x-ratelimit-remaining as com.example.tools/ratelimit-remaining",
            )
            .unwrap(),
        ];
        let r = build_mcp_result_from_message(msg, &tool, &None, &propagate);
        let meta = r.meta.expect("expected result.meta to be set");
        assert_eq!(
            meta.0.get("com.example.tools/ratelimit-remaining"),
            Some(&serde_json::Value::String("42".to_string()))
        );
        assert!(meta.0.get("x-ratelimit-remaining").is_none());
    }

    #[test]
    fn build_result_propagate_result_meta_skips_missing_headers() {
        let tool = tool_with_output_schema(None);
        let msg = MessageBuilder::new(b"{}".to_vec())
            .header(MessageHeaders::CONTENT_TYPE, "application/json")
            .build();
        // Declared but no matching header on the reply Message; no _meta
        // entry should be emitted (and result.meta should stay None).
        let propagate = vec![PropagatedHeader::parse("x-not-present").unwrap()];
        let r = build_mcp_result_from_message(msg, &tool, &None, &propagate);
        assert!(r.meta.is_none());
    }

    #[test]
    fn build_result_propagate_result_meta_attaches_on_error_path() {
        // Even when the result is an error, propagate-result-meta entries
        // should still be emitted on the result's _meta.
        let tool = tool_with_output_schema(None);
        let msg = MessageBuilder::new(b"not json".to_vec())
            .header(MessageHeaders::CONTENT_TYPE, "application/json")
            .header("x-trace", "abc")
            .build();
        let propagate = vec![PropagatedHeader::parse("x-trace").unwrap()];
        let r = build_mcp_result_from_message(msg, &tool, &None, &propagate);
        assert!(r.is_error.unwrap_or(false));
        let meta = r
            .meta
            .expect("expected result.meta to be set even on error");
        assert_eq!(
            meta.0.get("x-trace"),
            Some(&serde_json::Value::String("abc".to_string()))
        );
    }

    fn create_wasm(wat_content: &str) -> tempfile::NamedTempFile {
        let bytes = wat::parse_str(wat_content).unwrap();
        let mut f = Builder::new().suffix(".wasm").tempfile().unwrap();
        f.write_all(&bytes).unwrap();
        f
    }

    fn add_two_wat() -> &'static str {
        r#"
        (component
            (core module $m
                (func $add_two (param i32) (result i32)
                    local.get 0
                    i32.const 2
                    i32.add
                )
                (export "add-two" (func $add_two))
            )
            (core instance $i (instantiate $m))
            (func $f (param "x" s32) (result s32) (canon lift (core func $i "add-two")))
            (export "add-two" (func $f))
        )
        "#
    }

    // Build an McpServer from a Runtime by auto-discovering all components.
    fn build_test_server(runtime: &Runtime) -> McpServer {
        let component_host = runtime.host();
        let mut tools = HashMap::new();

        for component in runtime.list_components(None) {
            for function in component.functions.values() {
                let tool_name = format!("{}.{}", component.metadata.name, function.key());
                let tool = McpMapper::function_to_tool(function, &tool_name, None);
                let schema = serde_json::Value::Object((*tool.input_schema).clone());
                let input_validator = jsonschema::validator_for(&schema).unwrap();
                let mapper = composable_runtime::MessageMapper::from_component(
                    component,
                    Some(function.key()),
                    composable_runtime::MappingConfig::default(),
                )
                .unwrap();
                let target = ResolvedToolTarget::Component {
                    component_name: component.metadata.name.clone(),
                    mapper: Arc::new(mapper),
                };
                tools.insert(
                    tool_name,
                    ResolvedTool {
                        tool,
                        input_validator,
                        output_validator: None,
                        target,
                        propagate_request_meta: Vec::new(),
                        propagate_result_meta: Vec::new(),
                    },
                );
            }
        }

        let dummy_addr = "127.0.0.1:0".parse().unwrap();
        McpServer::new(
            tools,
            component_host,
            None,
            dummy_addr,
            OriginPolicy::AllowAll,
            None,
        )
    }

    #[derive(Debug, Clone, Default)]
    struct TestClientHandler;

    impl ClientHandler for TestClientHandler {
        fn get_info(&self) -> ClientInfo {
            ClientInfo::default()
        }
    }

    struct TestClient {
        client: Option<rmcp::service::RunningService<rmcp::RoleClient, TestClientHandler>>,
        server_handle: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    }

    impl std::ops::Deref for TestClient {
        type Target = rmcp::service::RunningService<rmcp::RoleClient, TestClientHandler>;
        fn deref(&self) -> &Self::Target {
            self.client.as_ref().unwrap()
        }
    }

    impl Drop for TestClient {
        fn drop(&mut self) {
            if let Some(client) = &self.client {
                client.cancellation_token().cancel();
            }
            if let Some(handle) = self.server_handle.take() {
                handle.abort();
            }
        }
    }

    async fn setup_test_client(server_handler: McpServer) -> TestClient {
        let (server_transport, client_transport) = tokio::io::duplex(4096);

        let server_handle = tokio::spawn(async move {
            server_handler
                .serve(server_transport)
                .await?
                .waiting()
                .await?;
            anyhow::Ok(())
        });

        let client = TestClientHandler.serve(client_transport).await.unwrap();

        TestClient {
            client: Some(client),
            server_handle: Some(server_handle),
        }
    }

    async fn build_runtime(wasm_path: &std::path::Path) -> Runtime {
        Runtime::builder()
            .from_path(wasm_path.to_path_buf())
            .build()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_tool_invocation() {
        let wasm = create_wasm(add_two_wat());
        let runtime = build_runtime(wasm.path()).await;
        let client = setup_test_client(build_test_server(&runtime)).await;

        let tools_result = client.list_tools(None).await.unwrap();
        assert_eq!(tools_result.tools.len(), 1);

        let tool = &tools_result.tools[0];
        assert!(
            tool.name.ends_with(".add-two"),
            "Tool name should end with .add-two, got: {}",
            tool.name
        );

        let input_schema = &tool.input_schema;
        assert_eq!(input_schema.get("type").unwrap(), "object");

        let properties = input_schema.get("properties").unwrap().as_object().unwrap();
        assert!(properties.contains_key("x"));

        let required = input_schema.get("required").unwrap().as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "x");

        let request = CallToolRequestParams::new(tool.name.clone()).with_arguments(args!({"x": 5}));
        let result = client.call_tool(request).await.unwrap();
        assert!(!result.is_error.unwrap_or(false));

        let result_value: i32 = result.content[0]
            .as_text()
            .unwrap()
            .text
            .trim()
            .parse()
            .unwrap();
        assert_eq!(result_value, 7);
    }

    #[tokio::test]
    async fn test_missing_required_parameter() {
        let wasm = create_wasm(add_two_wat());
        let runtime = build_runtime(wasm.path()).await;
        let client = setup_test_client(build_test_server(&runtime)).await;

        let tools_result = client.list_tools(None).await.unwrap();
        let tool = &tools_result.tools[0];

        let request = CallToolRequestParams::new(tool.name.clone()).with_arguments(args!({}));
        let result = client.call_tool(request).await.unwrap();
        assert!(result.is_error.unwrap_or(false));

        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(
            text.contains("\"x\" is a required property"),
            "unexpected error: {text}"
        );
    }

    #[tokio::test]
    async fn test_tool_not_found() {
        let wasm = create_wasm(add_two_wat());
        let runtime = build_runtime(wasm.path()).await;
        let client = setup_test_client(build_test_server(&runtime)).await;

        let request = CallToolRequestParams::new("nonexistent-tool");
        let result = client.call_tool(request).await.unwrap();
        assert!(result.is_error.unwrap_or(false));

        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("Tool not found"));
        assert!(text.contains("nonexistent-tool"));
    }
}
