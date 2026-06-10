use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use composable_runtime::{
    ComponentInvoker, ConfigHandler, MessageMapper, MessagePublisher, Service,
};
use rmcp::model::Tool;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use composable_runtime::schema;

use crate::config::{self, McpServerConfig, McpServerConfigHandler, SharedConfig, ToolTarget};
use crate::mapper::McpMapper;
use crate::origin::OriginPolicy;
use crate::server::McpServer;

pub struct McpService {
    config: SharedConfig,
    invoker: Mutex<Option<Arc<dyn ComponentInvoker>>>,
    publisher: Mutex<Option<Arc<dyn MessagePublisher>>>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Default for McpService {
    fn default() -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Self {
            config: config::shared_config(),
            invoker: Mutex::new(None),
            publisher: Mutex::new(None),
            shutdown_tx,
            shutdown_rx,
            tasks: Mutex::new(Vec::new()),
        }
    }
}

/// Resolved runtime representation of a tool's target.
#[derive(Clone)]
pub enum ResolvedToolTarget {
    Component {
        component_name: String,
        mapper: Arc<MessageMapper>,
    },
    Channel {
        channel: String,
    },
}

/// A fully resolved tool: MCP schema + target + validators + propagators.
#[derive(Clone)]
pub struct ResolvedTool {
    pub tool: Tool,
    pub target: ResolvedToolTarget,
    pub input_validator: jsonschema::Validator,
    pub output_validator: Option<jsonschema::Validator>,
    /// Inbound `_meta` lifts: each entry reads an MCP `_meta` key and writes
    /// the value under the target name on the inbound Message headers.
    pub propagate_request_meta: Vec<composable_runtime::PropagatedHeader>,
    /// Outbound `_meta` emits: each entry reads a reply Message header and
    /// writes the value under the target name on the `CallToolResult._meta`.
    pub propagate_result_meta: Vec<composable_runtime::PropagatedHeader>,
}

// Resolve all tools for a server from both explicit tool configs and component-selector.
fn resolve_tools(
    server_config: &McpServerConfig,
    invoker: &dyn ComponentInvoker,
) -> Result<HashMap<String, ResolvedTool>> {
    let mut tools = HashMap::new();

    // Selector-discovered tools first (explicit tools take precedence on collision)
    if let Some(selector) = &server_config.component_selector {
        let components = invoker.list_components(Some(selector));
        for component in components {
            for function in component.functions.values() {
                let tool_name = format!("{}.{}", component.metadata.name, function.key());
                let tool = McpMapper::function_to_tool(function, &tool_name, None);
                let input_schema = serde_json::Value::Object((*tool.input_schema).clone());
                let input_validator = build_validator(
                    &server_config.name,
                    &tool_name,
                    "input-schema",
                    &input_schema,
                )?;
                let mapper = MessageMapper::from_component(
                    component,
                    Some(function.key()),
                    composable_runtime::MappingConfig::default(),
                )
                .map_err(|e| {
                    anyhow::anyhow!("Server '{}': tool '{}': {e}", server_config.name, tool_name)
                })?;
                let target = ResolvedToolTarget::Component {
                    component_name: component.metadata.name.clone(),
                    mapper: Arc::new(mapper),
                };
                tools.insert(
                    tool_name,
                    ResolvedTool {
                        tool,
                        input_validator,
                        // Currently a WIT-derived schema. Will add validator
                        // when config allows optional explicit output-schema.
                        output_validator: None,
                        target,
                        propagate_request_meta: Vec::new(),
                        propagate_result_meta: Vec::new(),
                    },
                );
            }
        }
    }

    // Explicit tool configs override selector-discovered tools on name collision
    for tool_config in &server_config.tools {
        let (name, entry) = match &tool_config.target {
            ToolTarget::Component {
                component,
                function,
                mapping,
                input_schema,
                output_schema,
            } => {
                let comp = invoker.get_component(component).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Server '{}': tool '{}' references unknown component '{}'",
                        server_config.name,
                        tool_config.name,
                        component,
                    )
                })?;
                let func = comp.functions.get(function).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Tool '{}': function '{}' not found in component '{}'",
                        tool_config.name,
                        function,
                        component,
                    )
                })?;

                // Start with the WIT-derived Tool schema.
                let mut tool = McpMapper::function_to_tool(
                    func,
                    &tool_config.name,
                    tool_config.description.as_deref(),
                );

                // Input-schema derivation runs when param-mapping or
                // param-encoding is declared (the mapping reshapes the input)
                // or when an explicit schema is supplied (to validate
                // alignment). Otherwise, the WIT-derived schema stands.
                let needs_derived_input = mapping.param_mapping.is_some()
                    || mapping.param_encoding.is_some()
                    || input_schema.is_some();
                if needs_derived_input {
                    let derived = schema::derive_input_schema(func, mapping).map_err(|e| {
                        anyhow::anyhow!(
                            "Server '{}': tool '{}': input-schema derivation failed: {e}",
                            server_config.name,
                            tool_config.name,
                        )
                    })?;
                    let final_input = if let Some(declared) = input_schema.as_ref() {
                        schema::validate_structural_alignment(declared, &derived).map_err(|e| {
                            anyhow::anyhow!(
                                "Server '{}': tool '{}': explicit input-schema does not align with derived schema: {e}",
                                server_config.name,
                                tool_config.name,
                            )
                        })?;
                        declared.clone()
                    } else {
                        derived
                    };
                    tool.input_schema = Arc::new(
                        final_input
                            .as_object()
                            .cloned()
                            .expect("input-schema is an object"),
                    );
                }

                let derived_output: Option<serde_json::Value> =
                    schema::derive_output_schema(func, mapping).map_err(|e| {
                        anyhow::anyhow!(
                            "Server '{}': tool '{}': output-schema derivation failed: {e}",
                            server_config.name,
                            tool_config.name,
                        )
                    })?;

                // Apply the final output schema: explicit (validated against
                // derived if both present) or derived.
                let final_output_schema: Option<serde_json::Value> = match (
                    output_schema.as_ref(),
                    derived_output.as_ref(),
                ) {
                    (Some(declared), Some(derived)) => {
                        schema::validate_structural_alignment(declared, derived).map_err(|e| {
                            anyhow::anyhow!(
                                "Server '{}': tool '{}': explicit output-schema does not align with derived schema: {e}",
                                server_config.name,
                                tool_config.name,
                            )
                        })?;
                        Some(declared.clone())
                    }
                    (Some(declared), None) => Some(declared.clone()),
                    (None, Some(derived)) => Some(derived.clone()),
                    (None, None) => None,
                };

                if let Some(schema) = final_output_schema
                    .as_ref()
                    .and_then(|s| s.as_object().cloned())
                {
                    tool = tool.with_raw_output_schema(schema.into());
                } else {
                    tool.output_schema = None;
                }

                let input_schema = serde_json::Value::Object((*tool.input_schema).clone());
                let input_validator = build_validator(
                    &server_config.name,
                    &tool_config.name,
                    "input-schema",
                    &input_schema,
                )?;
                let output_validator = tool
                    .output_schema
                    .as_ref()
                    .map(|s| {
                        let schema = serde_json::Value::Object((**s).clone());
                        build_validator(
                            &server_config.name,
                            &tool_config.name,
                            "output-schema",
                            &schema,
                        )
                    })
                    .transpose()?;
                let mapper =
                    MessageMapper::from_component(comp, Some(function.clone()), mapping.clone())
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "Server '{}': tool '{}': {e}",
                                server_config.name,
                                tool_config.name
                            )
                        })?;
                let target = ResolvedToolTarget::Component {
                    component_name: comp.metadata.name.clone(),
                    mapper: Arc::new(mapper),
                };
                (
                    tool_config.name.clone(),
                    ResolvedTool {
                        tool,
                        input_validator,
                        output_validator,
                        target,
                        propagate_request_meta: tool_config.propagate_request_meta.clone(),
                        propagate_result_meta: tool_config.propagate_result_meta.clone(),
                    },
                )
            }
            ToolTarget::Channel {
                channel,
                input_schema,
                output_schema,
            } => {
                let tool = McpMapper::channel_tool(
                    &tool_config.name,
                    tool_config.description.as_deref(),
                    input_schema.clone(),
                    output_schema.clone(),
                );
                let input_schema_val = serde_json::Value::Object((*tool.input_schema).clone());
                let input_validator = build_validator(
                    &server_config.name,
                    &tool_config.name,
                    "input-schema",
                    &input_schema_val,
                )?;
                let output_validator = tool
                    .output_schema
                    .as_ref()
                    .map(|s| {
                        let schema = serde_json::Value::Object((**s).clone());
                        build_validator(
                            &server_config.name,
                            &tool_config.name,
                            "output-schema",
                            &schema,
                        )
                    })
                    .transpose()?;
                let target = ResolvedToolTarget::Channel {
                    channel: channel.clone(),
                };
                (
                    tool_config.name.clone(),
                    ResolvedTool {
                        tool,
                        input_validator,
                        output_validator,
                        target,
                        propagate_request_meta: tool_config.propagate_request_meta.clone(),
                        propagate_result_meta: tool_config.propagate_result_meta.clone(),
                    },
                )
            }
        };
        tools.insert(name, entry);
    }

    Ok(tools)
}

fn build_validator(
    server_name: &str,
    tool_name: &str,
    schema_name: &str,
    schema: &serde_json::Value,
) -> Result<jsonschema::Validator> {
    jsonschema::validator_for(schema).map_err(|e| {
        anyhow::anyhow!("Server '{server_name}': tool '{tool_name}' has invalid {schema_name}: {e}")
    })
}

impl Service for McpService {
    fn config_handler(&self) -> Option<Box<dyn ConfigHandler>> {
        Some(Box::new(McpServerConfigHandler::new(Arc::clone(
            &self.config,
        ))))
    }

    fn set_invoker(&self, invoker: Arc<dyn ComponentInvoker>) {
        *self.invoker.lock().unwrap() = Some(invoker);
    }

    fn set_publisher(&self, publisher: Arc<dyn MessagePublisher>) {
        *self.publisher.lock().unwrap() = Some(publisher);
    }

    fn start(&self) -> Result<()> {
        let mut server_configs = {
            let mut config = self.config.lock().unwrap();
            std::mem::take(&mut *config)
        };

        let invoker = self
            .invoker
            .lock()
            .unwrap()
            .clone()
            .expect("set_invoker must be called before start");

        let publisher = self.publisher.lock().unwrap().clone();

        if server_configs.is_empty() {
            tracing::info!(
                "No MCP server configured. Starting default on 127.0.0.1:3001 \
                 with auto-discovered top-level components."
            );
            server_configs.push(config::default_server());
        }

        let mut handles = Vec::new();

        for server_config in server_configs {
            let tools = resolve_tools(&server_config, &*invoker)?;

            let tool_count = tools.len();
            let origin_policy = OriginPolicy::from_config(
                server_config.allowed_origins.as_deref(),
                &server_config.host,
            );

            let addr: SocketAddr = format!("{}:{}", server_config.host, server_config.port)
                .parse()
                .map_err(|e| {
                    anyhow::anyhow!(
                        "Server '{}': invalid address '{}:{}': {e}",
                        server_config.name,
                        server_config.host,
                        server_config.port,
                    )
                })?;

            let tracer_provider = server_config
                .otlp_endpoint
                .as_deref()
                .map(|ep| {
                    crate::server::build_tracer_provider(
                        ep,
                        &server_config.otlp_protocol,
                        &server_config.name,
                    )
                })
                .transpose()?;

            let server = McpServer::new(
                tools,
                Arc::clone(&invoker),
                publisher.clone(),
                addr,
                origin_policy,
                tracer_provider,
            );

            tracing::info!(
                server_name = server_config.name,
                "Starting MCP server with {tool_count} {}",
                if tool_count == 1 { "tool" } else { "tools" },
            );

            let shutdown_rx = self.shutdown_rx.clone();
            handles.push(tokio::spawn(async move {
                if let Err(err) = server.run(shutdown_rx).await {
                    tracing::error!(server_name = server_config.name, "MCP server error: {err}");
                }
            }));
        }

        *self.tasks.lock().unwrap() = handles;
        Ok(())
    }

    fn shutdown(&self) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async {
            let _ = self.shutdown_tx.send(true);
            let handles: Vec<_> = {
                let mut tasks = self.tasks.lock().unwrap();
                std::mem::take(&mut *tasks)
            };
            for handle in handles {
                let _ = handle.await;
            }
        })
    }
}
