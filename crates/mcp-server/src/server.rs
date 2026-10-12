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
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        InitializeRequestParams, InitializeResult, JsonObject, ListToolsResult, MetaObject,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
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
use crate::tools;
use composable_runtime::{
    ComponentError, ComponentHost, Message, MessageBuilder, MessageHeaders, MessagePublisher,
    PROPAGATED_HEADERS, PROPAGATION_CONTEXT, PropagatedHeader, PropagationContext, Val, schema,
};
use composable_tools::{TOOL_CALL, meta_entries};

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
        meta: Option<&MetaObject>,
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
        meta: Option<&MetaObject>,
        context: Option<HashMap<String, String>>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let Some(resolved) = self.tools.get(tool_name) else {
            return Err(rmcp::ErrorData::invalid_params(
                format!("Unknown tool: {tool_name}"),
                None,
            ));
        };

        // Input validation errors are tool execution errors, which the
        // model can correct.
        let args_value = serde_json::Value::Object(arguments.clone());
        if let Err(error) = resolved.input_validator.validate(&args_value) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Invalid arguments for tool '{tool_name}': {error}"
            ))])
            .into());
        }

        // Build the inbound Message:
        //   - body = arguments as JSON
        //   - headers = declared propagate-request-meta entries
        //     (plus the tracing keys merged within MessageBuilder)
        let dispatch = async {
            // A `tool` component takes the arguments and `_meta` directly and
            // responds with a result, so no Message or mapping applies.
            if let ResolvedToolTarget::Tool { component_name } = &resolved.target {
                return self
                    .call_tool_component(component_name, resolved, arguments, meta)
                    .await;
            }

            let message =
                build_message_from_mcp_call(arguments, meta, &resolved.propagate_request_meta)
                    .map_err(server_error)?;

            // Propagation entries (PROPAGATED_HEADERS) read from the inbound
            // Message carry over to the reply Message. A tool execution error
            // includes the same entries a successful reply would.
            let propagated = propagated_headers(&message);
            let execution_error = |text: String| -> Result<CallToolResponse, rmcp::ErrorData> {
                Ok(tool_execution_error(text, &propagated, &resolved.propagate_result_meta).into())
            };

            let reply = match &resolved.target {
                ResolvedToolTarget::Tool { .. } => unreachable!("handled above"),
                ResolvedToolTarget::Component {
                    component_name,
                    mapper,
                } => {
                    // Arguments that pass the input schema can still fail
                    // conversion to the function's WIT types, which the
                    // model can correct.
                    let invocation = match mapper.to_invocation(&message) {
                        Ok(invocation) => invocation,
                        Err(e) => return execution_error(e),
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
                        // The function's `err` is a tool execution error.
                        // Anything else is a server error.
                        Err(e) => {
                            return match e.downcast::<ComponentError>() {
                                Ok(error) => execution_error(error_text(&error)),
                                Err(e) => Err(server_error(e.to_string())),
                            };
                        }
                    };
                    // The reply mapper is JSON-based.
                    let wit_result = match wit_result {
                        Some(value) => {
                            value.into_json().map_err(|e| server_error(e.to_string()))?
                        }
                        None => serde_json::Value::Null,
                    };
                    mapper
                        .from_invocation_result(&wit_result, propagated.clone())
                        .map_err(server_error)?
                }
                ResolvedToolTarget::Channel { channel } => {
                    let Some(publisher) = &self.publisher else {
                        return Err(server_error(
                            "Channel-backed tools require messaging support",
                        ));
                    };
                    // The channel and its subscriber are the tool's backend:
                    // failing to reach them is a tool execution error, which
                    // may succeed on a retry.
                    let return_address = match publisher.publish_request(channel, message).await {
                        Ok(return_address) => return_address,
                        Err(e) => {
                            return execution_error(format!(
                                "Failed to publish to channel '{channel}': {e}"
                            ));
                        }
                    };
                    match return_address.take().await {
                        Ok(reply) => reply,
                        Err(e) => {
                            return execution_error(format!(
                                "Failed to receive reply for request to channel '{channel}': {e}"
                            ));
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
            .map(Into::into)
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

    /// Call a component exporting `composable:tools/tool`.
    async fn call_tool_component(
        &self,
        component_name: &str,
        resolved: &ResolvedTool,
        arguments: &JsonObject,
        meta: Option<&MetaObject>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let arguments = serde_json::to_string(arguments)
            .map_err(|e| server_error(format!("failed to serialize MCP arguments: {e}")))?;

        let entries = request_meta_entries(meta, &resolved.propagate_request_meta);

        let returned = self
            .component_host
            .invoke(
                component_name,
                TOOL_CALL,
                vec![
                    Val::Json(serde_json::Value::String(arguments)),
                    Val::Json(meta_entries(entries)),
                ],
                None,
            )
            .await;

        // A `tool-error` is the JSON-RPC error the tool reports. Anything that
        // breaks the `composable:tools/tool` contract is a server error.
        let returned = match returned {
            Ok(Some(value)) => value,
            Ok(None) => {
                return Err(server_error(format!(
                    "{component_name}: {TOOL_CALL} returned nothing"
                )));
            }
            Err(e) => {
                return Err(match e.downcast::<ComponentError>() {
                    Ok(ComponentError {
                        value: Some(Val::Json(error)),
                    }) => tools::tool_error(&error).unwrap_or_else(|e| {
                        server_error(format!("{component_name}: invalid tool-error: {e}"))
                    }),
                    Ok(error) => server_error(format!("{component_name}: {error}")),
                    Err(e) => server_error(format!("{component_name}: {e}")),
                });
            }
        };
        let returned = returned
            .into_json()
            .map_err(|e| server_error(format!("{component_name}: {e}")))?;
        let result = tools::call_tool_result(&returned)
            .map_err(|e| server_error(format!("{component_name}: {e}")))?;

        validate_structured_content(
            result,
            &resolved.output_validator,
            &resolved.propagate_result_meta,
        )
        .map(Into::into)
    }
}

// The `_meta` entries to forward to a tool, as declared by
// `propagate-request-meta`, plus the tracing keys in the propagation scope.
// Non-string `_meta` values are skipped with a warning since a `meta-entry` is
// a pair of strings.
fn request_meta_entries(
    meta: Option<&MetaObject>,
    propagate_request_meta: &[PropagatedHeader],
) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = PROPAGATION_CONTEXT
        .try_with(|ctx| {
            ctx.as_ref().map(|c| {
                c.entries
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
        })
        .ok()
        .flatten()
        .unwrap_or_default();

    if let Some(m) = meta {
        for entry in propagate_request_meta {
            match m.0.get(entry.source()) {
                Some(serde_json::Value::String(s)) => {
                    entries.push((entry.target().to_string(), s.clone()));
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

    entries
}

// Ensure a tool's structured content conforms to its advertised output schema,
// and select which `_meta` entries to include with the result. A result that
// does not conform is a server error.
fn validate_structured_content(
    mut result: CallToolResult,
    output_validator: &Option<jsonschema::Validator>,
    propagate_result_meta: &[PropagatedHeader],
) -> Result<CallToolResult, rmcp::ErrorData> {
    if let Some(validator) = output_validator
        && let Some(structured) = result.structured_content.as_ref()
        && let Err(error) = validator.validate(structured)
    {
        return Err(server_error(format!(
            "tool result does not conform to output-schema: {error}"
        )));
    }

    let emitted = result_meta(result.meta.as_ref(), propagate_result_meta);
    result.meta = None;
    Ok(apply_meta(result, emitted))
}

// A server error, reported as the JSON-RPC internal error.
fn server_error(message: impl Into<String>) -> rmcp::ErrorData {
    rmcp::ErrorData::internal_error(message.into(), None)
}

// An `isError` result for a tool execution error, with the `_meta` entries
// propagate-result-meta selects from the given headers.
fn tool_execution_error(
    text: String,
    headers: &HashMap<String, String>,
    propagate_result_meta: &[PropagatedHeader],
) -> CallToolResult {
    apply_meta(
        CallToolResult::error(vec![ContentBlock::text(text)]),
        selected_result_meta(|name| headers.get(name).cloned(), propagate_result_meta),
    )
}

// The `_meta` entries propagate-result-meta selects from a set of headers,
// each keyed by its target name.
fn selected_result_meta(
    header: impl Fn(&str) -> Option<String>,
    propagate_result_meta: &[PropagatedHeader],
) -> Option<MetaObject> {
    let mut map = serde_json::Map::new();
    for entry in propagate_result_meta {
        if let Some(value) = header(entry.source()) {
            map.insert(entry.target().to_string(), serde_json::Value::String(value));
        }
    }
    (!map.is_empty()).then_some(MetaObject(map))
}

// The text of an `isError` result for a function's WIT `err` value.
fn error_text(error: &ComponentError) -> String {
    match &error.value {
        Some(Val::Json(serde_json::Value::String(text))) => text.clone(),
        Some(Val::Json(value)) => value.to_string(),
        _ => error.to_string(),
    }
}

// The `_meta` to include with a result.
fn result_meta(
    returned: Option<&MetaObject>,
    propagate_result_meta: &[PropagatedHeader],
) -> Option<MetaObject> {
    let returned = returned?;
    let mut map = serde_json::Map::new();
    for entry in propagate_result_meta {
        if let Some(value) = returned.0.get(entry.source()) {
            map.insert(entry.target().to_string(), value.clone());
        }
    }
    (!map.is_empty()).then_some(MetaObject(map))
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
    meta: Option<&MetaObject>,
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
// - A body that is not JSON, cannot be coerced, or does not conform to the
//   output schema is a server error.
//
// `result<T, E>` to `isError` mapping is NOT handled here. The runtime's
// `invoke` returns a WIT `err` as a `ComponentError`, which the caller reports
// as an `isError` result before any reply Message exists.
fn build_mcp_result_from_message(
    msg: Message,
    tool: &Tool,
    output_validator: &Option<jsonschema::Validator>,
    propagate_result_meta: &[PropagatedHeader],
) -> Result<CallToolResult, rmcp::ErrorData> {
    // The _meta entries to emit on the result, read from the reply Message's
    // headers.
    let result_meta = selected_result_meta(
        |name| {
            msg.headers()
                .get::<&str>(name)
                .map(|value| value.to_string())
        },
        propagate_result_meta,
    );

    let parsed: serde_json::Value = if msg.body().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(msg.body())
            .map_err(|e| server_error(format!("reply body is not valid JSON: {e}")))?
    };

    let Some(schema) = tool.output_schema.as_ref() else {
        let text = match &parsed {
            serde_json::Value::String(s) => s.clone(),
            other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
        };
        return Ok(apply_meta(
            CallToolResult::success(vec![ContentBlock::text(text)]),
            result_meta,
        ));
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

    schema::coerce_value(&mut wrapped, &schema_value)
        .map_err(|e| server_error(format!("failed to coerce reply body to output-schema: {e}")))?;

    if let Some(validator) = output_validator
        && let Err(error) = validator.validate(&wrapped)
    {
        return Err(server_error(format!(
            "reply body does not conform to output-schema: {error}"
        )));
    }

    Ok(apply_meta(CallToolResult::structured(wrapped), result_meta))
}

// Attach the optional Meta to a CallToolResult.
fn apply_meta(mut result: CallToolResult, meta: Option<MetaObject>) -> CallToolResult {
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
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let tool_name = &request.name;
        let arguments = request.arguments.unwrap_or_default();

        // rmcp extracts _meta from params during deserialization and places it
        // in RequestContext.meta, not in CallToolRequestParams.meta.
        let meta = if context.meta.0.is_empty() {
            None
        } else {
            Some(&context.meta.0)
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
            match &result {
                Ok(CallToolResponse::Complete(complete)) if complete.is_error.unwrap_or(false) => {
                    span.set_status(Status::error(""));
                    span.set_attribute(KeyValue::new("error.type", "tool_error"));
                }
                Err(error) => {
                    span.set_status(Status::error(error.message.to_string()));
                    span.set_attribute(KeyValue::new("error.type", error.code.0.to_string()));
                }
                _ => {}
            }
            span.end();
        }

        result
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, rmcp::ErrorData> {
        let meta = if context.meta.0.is_empty() {
            None
        } else {
            Some(&context.meta.0)
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
            Some(&context.meta.0)
        };
        let span_ctx = self.start_mcp_span("tools/list", None, request_attributes(&context), meta);

        let tools = self.tools.values().map(|r| r.tool.clone()).collect();
        let result = ListToolsResult::with_all_items(tools);

        if let Some((mut span, _)) = span_ctx {
            span.end();
        }

        Ok(result)
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                rmcp::model::Implementation::new(
                    "composable-mcp-server",
                    env!("CARGO_PKG_VERSION"),
                )
                .with_title("Composable MCP Server")
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
    use rmcp::model::ClientConfig;
    use rmcp::{ClientHandler, ServiceExt};
    use std::io::Write as _;
    use tempfile::Builder;

    macro_rules! args {
        ($($json:tt)+) => {
            serde_json::json!($($json)+).as_object().unwrap().clone()
        };
    }

    fn meta(value: serde_json::Value) -> MetaObject {
        MetaObject(value.as_object().unwrap().clone())
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
        let r = build_mcp_result_from_message(msg, &tool, &None, &[]).unwrap();
        assert!(!r.is_error.unwrap_or(false));
        let text = r.content[0].as_text().unwrap().text.clone();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, serde_json::json!({ "value": 42 }));
    }

    #[test]
    fn build_result_no_schema_returns_string_body_raw() {
        let tool = tool_with_output_schema(None);
        let msg = reply_msg(serde_json::json!("hello, world"));
        let r = build_mcp_result_from_message(msg, &tool, &None, &[]).unwrap();
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
        let r = build_mcp_result_from_message(msg, &tool, &None, &[]).unwrap();
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
        let r = build_mcp_result_from_message(msg, &tool, &None, &[]).unwrap();
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
        let r = build_mcp_result_from_message(msg, &tool, &validator, &[]).unwrap();
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
        let r = build_mcp_result_from_message(msg, &tool, &validator, &[]).unwrap();
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
        let error = build_mcp_result_from_message(msg, &tool, &validator, &[]).unwrap_err();
        assert_eq!(error.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert!(
            error.message.contains("does not conform to output-schema"),
            "unexpected error message: {}",
            error.message
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
        let r = build_mcp_result_from_message(msg, &tool, &validator, &[]).unwrap();
        assert!(!r.is_error.unwrap_or(false));
    }

    #[test]
    fn build_result_invalid_json_body_is_error() {
        let tool = tool_with_output_schema(None);
        let msg = MessageBuilder::new(b"not json".to_vec())
            .header(MessageHeaders::CONTENT_TYPE, "application/json")
            .build();
        let error = build_mcp_result_from_message(msg, &tool, &None, &[]).unwrap_err();
        assert_eq!(error.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert!(
            error.message.contains("reply body is not valid JSON"),
            "unexpected error message: {}",
            error.message
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
        let r = build_mcp_result_from_message(msg, &tool, &None, &propagate).unwrap();
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
        let r = build_mcp_result_from_message(msg, &tool, &None, &propagate).unwrap();
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
        let r = build_mcp_result_from_message(msg, &tool, &None, &propagate).unwrap();
        assert!(r.meta.is_none());
    }

    #[test]
    fn propagate_result_meta_attaches_on_error_path() {
        // Even when the result is an error, propagate-result-meta entries
        // should still be emitted on the result's _meta.
        let headers = HashMap::from([("x-trace".to_string(), "abc".to_string())]);
        let propagate = vec![PropagatedHeader::parse("x-trace").unwrap()];
        let r = tool_execution_error("the tool failed".to_string(), &headers, &propagate);
        assert!(r.is_error.unwrap_or(false));
        let meta = r
            .meta
            .expect("expected result.meta to be set even on error");
        assert_eq!(
            meta.0.get("x-trace"),
            Some(&serde_json::Value::String("abc".to_string()))
        );
    }

    #[test]
    fn error_text_is_a_string_err_value_itself() {
        let error = ComponentError {
            value: Some(Val::Json(serde_json::json!("out of stock"))),
        };
        assert_eq!(error_text(&error), "out of stock");
    }

    #[test]
    fn error_text_is_a_structured_err_value_as_json() {
        let error = ComponentError {
            value: Some(Val::Json(serde_json::json!({ "code": 7 }))),
        };
        assert_eq!(error_text(&error), "{\"code\":7}");
    }

    #[test]
    fn error_text_without_an_err_value_describes_the_error() {
        let error = ComponentError { value: None };
        assert_eq!(error_text(&error), "Component returned error");
    }

    #[test]
    fn non_conformant_structured_content_is_a_server_error() {
        // The server must not return structured content that does not
        // conform to the tool's output schema.
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        });
        let validator = Some(jsonschema::validator_for(&schema).unwrap());
        let returned = CallToolResult::structured(serde_json::json!({ "wrong": "field" }));
        let error = validate_structured_content(returned, &validator, &[]).unwrap_err();
        assert_eq!(error.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert!(
            error.message.contains("does not conform to output-schema"),
            "unexpected error message: {}",
            error.message
        );
    }

    #[test]
    fn propagate_result_meta_attaches_to_a_tool_reported_error() {
        // A tool component reporting failure through `is-error` still gets
        // the propagate-result-meta entries from the `meta` it returned.
        let mut returned = CallToolResult::error(vec![ContentBlock::text("the tool failed")]);
        returned.meta = Some(meta(
            serde_json::json!({ "x-trace": "abc", "x-other": "1" }),
        ));
        let propagate = vec![PropagatedHeader::parse("x-trace").unwrap()];
        let r = validate_structured_content(returned, &None, &propagate).unwrap();
        assert!(r.is_error.unwrap_or(false));
        let meta = r
            .meta
            .expect("expected result.meta to be set even on error");
        assert_eq!(
            meta.0.get("x-trace"),
            Some(&serde_json::Value::String("abc".to_string()))
        );
        assert!(meta.0.get("x-other").is_none());
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

    // `check(x: s32) -> result<s32, string>`: `x` when it is not negative,
    // otherwise the `err` "x must not be negative". The result is written at
    // offset 0: the discriminant byte, then the payload at offset 4 (an s32,
    // or a string's pointer and length).
    fn check_wat() -> &'static str {
        r#"
        (component
            (core module $m
                (memory (export "memory") 1)
                (data (i32.const 64) "x must not be negative")
                (func $check (param i32) (result i32)
                    (if (i32.lt_s (local.get 0) (i32.const 0))
                        (then
                            (i32.store8 (i32.const 0) (i32.const 1))
                            (i32.store (i32.const 4) (i32.const 64))
                            (i32.store (i32.const 8) (i32.const 22)))
                        (else
                            (i32.store8 (i32.const 0) (i32.const 0))
                            (i32.store (i32.const 4) (local.get 0))))
                    (i32.const 0)
                )
                (export "check" (func $check))
            )
            (core instance $i (instantiate $m))
            (alias core export $i "memory" (core memory $mem))
            (func $f (param "x" s32) (result (result s32 (error string)))
                (canon lift (core func $i "check") (memory $mem)))
            (export "check" (func $f))
        )
        "#
    }

    // `fail(x: s32) -> s32`, which traps.
    fn trap_wat() -> &'static str {
        r#"
        (component
            (core module $m
                (func $fail (param i32) (result i32)
                    unreachable
                )
                (export "fail" (func $fail))
            )
            (core instance $i (instantiate $m))
            (func $f (param "x" s32) (result s32) (canon lift (core func $i "fail")))
            (export "fail" (func $f))
        )
        "#
    }

    // Build an McpServer from a Runtime by auto-discovering all components.
    fn build_test_server(runtime: &Runtime) -> McpServer {
        build_test_server_with(
            runtime,
            composable_runtime::MappingConfig::default(),
            Vec::new(),
            Vec::new(),
        )
    }

    // Build an McpServer from a Runtime by auto-discovering all components,
    // giving every tool the same mapping and `_meta` propagation.
    fn build_test_server_with(
        runtime: &Runtime,
        mapping: composable_runtime::MappingConfig,
        propagate_request_meta: Vec<PropagatedHeader>,
        propagate_result_meta: Vec<PropagatedHeader>,
    ) -> McpServer {
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
                    mapping.clone(),
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
                        propagate_request_meta: propagate_request_meta.clone(),
                        propagate_result_meta: propagate_result_meta.clone(),
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
        fn get_info(&self) -> ClientConfig {
            ClientConfig::default()
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

        // An unknown tool is a protocol error, not a tool execution error.
        let request = CallToolRequestParams::new("nonexistent-tool");
        let error = match client.call_tool(request).await {
            Err(rmcp::service::ServiceError::McpError(error)) => error,
            other => panic!("expected a JSON-RPC error, got {other:?}"),
        };
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert_eq!(error.message, "Unknown tool: nonexistent-tool");
    }

    // A request carrying `_meta` keys, for tests that propagate them.
    fn request_with_meta(
        name: std::borrow::Cow<'static, str>,
        arguments: JsonObject,
        meta_value: serde_json::Value,
    ) -> CallToolRequestParams {
        let mut request = CallToolRequestParams::new(name).with_arguments(arguments);
        request.meta = Some(rmcp::model::RequestMetaObject(meta(meta_value)));
        request
    }

    #[tokio::test]
    async fn test_function_err_is_a_tool_execution_error() {
        // A function's WIT `err` produces an `isError` result with the
        // propagate-result-meta entries a reply would carry.
        let wasm = create_wasm(check_wat());
        let runtime = build_runtime(wasm.path()).await;
        let baggage = vec![PropagatedHeader::parse("baggage").unwrap()];
        let server = build_test_server_with(
            &runtime,
            composable_runtime::MappingConfig::default(),
            baggage.clone(),
            baggage,
        );
        let client = setup_test_client(server).await;
        let tool = client.list_tools(None).await.unwrap().tools[0].name.clone();

        let ok = client
            .call_tool(request_with_meta(
                tool.clone(),
                args!({ "x": 3 }),
                serde_json::json!({ "baggage": "k=v" }),
            ))
            .await
            .unwrap();
        assert!(!ok.is_error.unwrap_or(false));
        assert_eq!(ok.content[0].as_text().unwrap().text, "3");

        let err = client
            .call_tool(request_with_meta(
                tool,
                args!({ "x": -1 }),
                serde_json::json!({ "baggage": "k=v" }),
            ))
            .await
            .unwrap();
        assert!(err.is_error.unwrap_or(false));
        assert_eq!(
            err.content[0].as_text().unwrap().text,
            "x must not be negative"
        );
        let meta = err
            .meta
            .expect("expected result.meta on the isError result");
        assert_eq!(
            meta.0.get("baggage"),
            Some(&serde_json::Value::String("k=v".to_string()))
        );
    }

    #[tokio::test]
    async fn test_trap_is_a_server_error() {
        // A trap is a fault in the component, not something the model can fix.
        // Produces a JSON-RPC internal error.
        let wasm = create_wasm(trap_wat());
        let runtime = build_runtime(wasm.path()).await;
        let client = setup_test_client(build_test_server(&runtime)).await;
        let tool = client.list_tools(None).await.unwrap().tools[0].name.clone();

        let request = CallToolRequestParams::new(tool).with_arguments(args!({ "x": 1 }));
        let error = match client.call_tool(request).await {
            Err(rmcp::service::ServiceError::McpError(error)) => error,
            other => panic!("expected a JSON-RPC error, got {other:?}"),
        };
        assert_eq!(error.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    }

    #[tokio::test]
    async fn test_argument_mapping_failure_is_a_tool_execution_error() {
        // Arguments that pass the input schema can still fail mapping to the
        // function's parameters, e.g. a param-mapping template whose `_meta`
        // source is missing. That is a tool execution error, which the caller
        // can correct.
        let wasm = create_wasm(add_two_wat());
        let runtime = build_runtime(wasm.path()).await;
        let mapping = composable_runtime::MappingConfig {
            param_mapping: Some([("x".to_string(), serde_json::json!("{headers.x-value}"))].into()),
            ..Default::default()
        };
        let server = build_test_server_with(
            &runtime,
            mapping,
            vec![PropagatedHeader::parse("x-value").unwrap()],
            Vec::new(),
        );
        let client = setup_test_client(server).await;
        let tool = client.list_tools(None).await.unwrap().tools[0].name.clone();

        // With the `_meta` source present, the mapping succeeds.
        let mapped = client
            .call_tool(request_with_meta(
                tool.clone(),
                args!({ "x": 0 }),
                serde_json::json!({ "x-value": "5" }),
            ))
            .await
            .unwrap();
        assert!(!mapped.is_error.unwrap_or(false));
        assert_eq!(mapped.content[0].as_text().unwrap().text, "7");

        let unmapped = client
            .call_tool(CallToolRequestParams::new(tool).with_arguments(args!({ "x": 0 })))
            .await
            .unwrap();
        assert!(unmapped.is_error.unwrap_or(false));
        let text = unmapped.content[0].as_text().unwrap().text.as_str();
        assert!(
            text.contains("template references unknown path"),
            "unexpected error: {text}"
        );
    }
}
