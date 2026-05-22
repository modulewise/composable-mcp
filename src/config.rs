//! Parses `[server.*]` definitions where `type = "mcp"` and the
//! `[server.mcp.tool.*]` entries nested within.
//!
//! A tool target is either:
//!
//! - `Component`: invokes a WIT function directly. The tool definition
//!   may carry the four WIT-bridging mapping blocks (`param-mapping`,
//!   `param-encoding`, `result-decoding`, `result-mapping`) and an
//!   optional `input-schema` / `output-schema`. Both schemas are derived
//!   from the WIT signature (with the mapping blocks applied). If an
//!   explicit schema is also provided, it must structurally align with
//!   the derived shape and may layer additional constraints or metadata
//!   on top.
//! - `Channel`: publishes the request as a Message to a channel. The
//!   mapping blocks are NOT carried here since whatever subscription
//!   consumes from the channel owns them. Requires an explicit `input-schema`.
//!
//! Tools also accept `propagate-request-meta` and `propagate-result-meta`,
//! each a list of `"source"` or `"source as target"` entries. The
//! MCP-side name of each entry is validated against the `_meta` key
//! format from the MCP spec (2025-11-25).
//!
//! A `propagate-result-meta` target that collides with a
//! `result-mapping.headers` target from a different source is rejected
//! at config-time.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use composable_runtime::{
    CategoryClaim, Condition, ConfigHandler, MappingConfig, Operator, ParamEncoding, ParamMapping,
    PropagatedHeader, PropertyMap, ResultDecoding, Selector,
};

// Default component selector for auto-discovery: top-level components only.
const DEFAULT_COMPONENT_SELECTOR: &str = "!dependents";

/// How a tool is backed: direct component invocation or channel publish.
#[derive(Debug, Clone)]
pub enum ToolTarget {
    Component {
        component: String,
        function: String,
        /// The four mapping blocks (param-mapping, param-encoding,
        /// result-decoding, result-mapping) bundled in pipeline order.
        mapping: MappingConfig,
        /// Optional explicit input-schema. When provided, it must
        /// structurally align with the schema derived from the WIT
        /// signature (with `param-mapping` / `param-encoding` applied).
        /// Used to layer additional constraints or metadata on top of
        /// the derived shape.
        input_schema: Option<serde_json::Value>,
        /// Optional explicit output-schema. When absent, the schema is
        /// derived (from the WIT result, the result-mapping, or both with
        /// any result-decoding swap applied). When provided, it must
        /// structurally align with the derived schema.
        output_schema: Option<serde_json::Value>,
    },
    Channel {
        channel: String,
        input_schema: serde_json::Value,
        output_schema: Option<serde_json::Value>,
    },
}

/// Parsed tool within an MCP server.
#[derive(Debug, Clone)]
pub struct ToolConfig {
    pub name: String,
    pub target: ToolTarget,
    pub description: Option<String>,
    /// MCP `_meta` keys to lift into inbound Message headers when a
    /// tools/call request arrives. Each entry's source is a `_meta` key
    /// (e.g. `com.example.tools/tag`); target is the Message header name
    /// to write under (defaults to the source).
    pub propagate_request_meta: Vec<PropagatedHeader>,
    /// Reply Message headers to emit as MCP `_meta` entries on the
    /// `CallToolResult`. Each entry's source is a Message header name;
    /// target is the `_meta` key to write under (defaults to the source).
    pub propagate_result_meta: Vec<PropagatedHeader>,
}

/// Parsed MCP server definition.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub allowed_origins: Option<Vec<String>>,
    pub component_selector: Option<Selector>,
    pub tools: Vec<ToolConfig>,
    pub otlp_endpoint: Option<String>,
    pub otlp_protocol: String,
}

pub type SharedConfig = Arc<Mutex<Vec<McpServerConfig>>>;

pub fn shared_config() -> SharedConfig {
    Arc::new(Mutex::new(Vec::new()))
}

/// Create a default server config for auto-discovery of top-level components.
pub fn default_server() -> McpServerConfig {
    McpServerConfig {
        name: "mcp".to_string(),
        host: "127.0.0.1".to_string(),
        port: 3001,
        allowed_origins: None,
        component_selector: Some(
            Selector::parse(DEFAULT_COMPONENT_SELECTOR)
                .expect("default component selector is valid"),
        ),
        tools: Vec::new(),
        otlp_endpoint: None,
        otlp_protocol: "grpc".to_string(),
    }
}

/// Claims `[server.*]` definitions where `type = "mcp"`.
pub struct McpServerConfigHandler {
    servers: SharedConfig,
}

impl McpServerConfigHandler {
    pub fn new(servers: SharedConfig) -> Self {
        Self { servers }
    }
}

impl ConfigHandler for McpServerConfigHandler {
    fn claimed_categories(&self) -> Vec<CategoryClaim> {
        vec![CategoryClaim::with_selector(
            "server",
            Selector {
                conditions: vec![Condition {
                    key: "type".to_string(),
                    operator: Operator::Equals("mcp".to_string()),
                }],
            },
        )]
    }

    fn claimed_properties(&self) -> HashMap<&str, &[&str]> {
        HashMap::from([(
            "server",
            [
                "type",
                "host",
                "port",
                "allowed-origins",
                "component-selector",
                "otlp-endpoint",
                "otlp-protocol",
                "tool",
            ]
            .as_slice(),
        )])
    }

    fn handle_category(
        &mut self,
        category: &str,
        name: &str,
        mut properties: PropertyMap,
    ) -> Result<()> {
        if category != "server" {
            return Err(anyhow::anyhow!(
                "McpServerConfigHandler received unexpected category '{category}'"
            ));
        }

        // type is only used by the selector
        properties.remove("type");

        let port = match properties.remove("port") {
            Some(serde_json::Value::Number(n)) => n
                .as_u64()
                .and_then(|p| u16::try_from(p).ok())
                .ok_or_else(|| {
                    anyhow::anyhow!("Server '{name}': 'port' must be a valid port number")
                })?,
            Some(got) => {
                return Err(anyhow::anyhow!(
                    "Server '{name}': 'port' must be a number, got {got}"
                ));
            }
            None => {
                return Err(anyhow::anyhow!(
                    "Server '{name}' missing required 'port' field"
                ));
            }
        };

        let host = match properties.remove("host") {
            Some(serde_json::Value::String(s)) => s,
            Some(got) => {
                return Err(anyhow::anyhow!(
                    "Server '{name}': 'host' must be a string, got {got}"
                ));
            }
            None => "127.0.0.1".to_string(),
        };

        let allowed_origins = match properties.remove("allowed-origins") {
            Some(serde_json::Value::Array(arr)) => {
                let mut origins = Vec::new();
                for item in arr {
                    match item {
                        serde_json::Value::String(s) => origins.push(s),
                        got => {
                            return Err(anyhow::anyhow!(
                                "Server '{name}': 'allowed-origins' items must be strings, got {got}"
                            ));
                        }
                    }
                }
                Some(origins)
            }
            Some(serde_json::Value::String(s)) if s == "*" => Some(vec!["*".to_string()]),
            Some(got) => {
                return Err(anyhow::anyhow!(
                    "Server '{name}': 'allowed-origins' must be an array or '*', got {got}"
                ));
            }
            None => None,
        };

        let component_selector = match properties.remove("component-selector") {
            Some(serde_json::Value::String(s)) => Some(Selector::parse(&s).map_err(|e| {
                anyhow::anyhow!("Server '{name}': invalid component-selector '{s}': {e}")
            })?),
            Some(got) => {
                return Err(anyhow::anyhow!(
                    "Server '{name}': 'component-selector' must be a string, got {got}"
                ));
            }
            None => None,
        };

        let otlp_endpoint = match properties.remove("otlp-endpoint") {
            Some(serde_json::Value::String(s)) => Some(s),
            Some(got) => {
                return Err(anyhow::anyhow!(
                    "Server '{name}': 'otlp-endpoint' must be a string, got {got}"
                ));
            }
            None => None,
        };

        let otlp_protocol = match properties.remove("otlp-protocol") {
            Some(serde_json::Value::String(s)) => s,
            Some(got) => {
                return Err(anyhow::anyhow!(
                    "Server '{name}': 'otlp-protocol' must be a string, got {got}"
                ));
            }
            None => "grpc".to_string(),
        };

        let tools = parse_tools(name, &mut properties)?;

        if component_selector.is_none() && tools.is_empty() {
            return Err(anyhow::anyhow!(
                "Server '{name}' has no tools and no component-selector. \
                 At least one must be specified."
            ));
        }

        if !properties.is_empty() {
            let unknown: Vec<_> = properties.keys().collect();
            return Err(anyhow::anyhow!(
                "Server '{name}' has unknown properties: {unknown:?}"
            ));
        }

        self.servers.lock().unwrap().push(McpServerConfig {
            name: name.to_string(),
            host,
            port,
            allowed_origins,
            component_selector,
            tools,
            otlp_endpoint,
            otlp_protocol,
        });
        Ok(())
    }
}

fn parse_tools(server_name: &str, properties: &mut PropertyMap) -> Result<Vec<ToolConfig>> {
    let tool_table = match properties.remove("tool") {
        Some(serde_json::Value::Object(map)) => map,
        Some(got) => {
            return Err(anyhow::anyhow!(
                "Server '{server_name}': 'tool' must be a table, got {got}"
            ));
        }
        None => return Ok(Vec::new()),
    };

    let mut tools = Vec::new();
    for (tool_name, tool_value) in tool_table {
        let mut tool_props = match tool_value {
            serde_json::Value::Object(map) => map,
            got => {
                return Err(anyhow::anyhow!(
                    "Server '{server_name}': tool '{tool_name}' must be a table, got {got}"
                ));
            }
        };

        let component = match tool_props.remove("component") {
            Some(serde_json::Value::String(s)) => Some(s),
            Some(got) => {
                return Err(anyhow::anyhow!(
                    "Server '{server_name}': tool '{tool_name}' 'component' must be a string, got {got}"
                ));
            }
            None => None,
        };

        let function = match tool_props.remove("function") {
            Some(serde_json::Value::String(s)) => Some(s),
            Some(got) => {
                return Err(anyhow::anyhow!(
                    "Server '{server_name}': tool '{tool_name}' 'function' must be a string, got {got}"
                ));
            }
            None => None,
        };

        let channel = match tool_props.remove("channel") {
            Some(serde_json::Value::String(s)) => Some(s),
            Some(got) => {
                return Err(anyhow::anyhow!(
                    "Server '{server_name}': tool '{tool_name}' 'channel' must be a string, got {got}"
                ));
            }
            None => None,
        };

        let input_schema = tool_props.remove("input-schema");
        let output_schema = tool_props.remove("output-schema");

        let param_mapping = match tool_props.remove("param-mapping") {
            Some(serde_json::Value::Object(map)) => Some(map.into_iter().collect::<ParamMapping>()),
            Some(got) => {
                return Err(anyhow::anyhow!(
                    "Server '{server_name}': tool '{tool_name}' 'param-mapping' must be an object, got {got}"
                ));
            }
            None => None,
        };

        let param_encoding = match tool_props.remove("param-encoding") {
            Some(serde_json::Value::Object(map)) => {
                Some(ParamEncoding::parse(&map).map_err(|e| {
                    anyhow::anyhow!(
                        "Server '{server_name}': tool '{tool_name}' 'param-encoding': {e}"
                    )
                })?)
            }
            Some(got) => {
                return Err(anyhow::anyhow!(
                    "Server '{server_name}': tool '{tool_name}' 'param-encoding' must be an object, got {got}"
                ));
            }
            None => None,
        };

        let result_decoding = match tool_props.remove("result-decoding") {
            Some(serde_json::Value::Object(map)) => {
                Some(ResultDecoding::parse(&map).map_err(|e| {
                    anyhow::anyhow!(
                        "Server '{server_name}': tool '{tool_name}' 'result-decoding': {e}"
                    )
                })?)
            }
            Some(got) => {
                return Err(anyhow::anyhow!(
                    "Server '{server_name}': tool '{tool_name}' 'result-decoding' must be an object, got {got}"
                ));
            }
            None => None,
        };

        // The `result-mapping` is flexible JSON (object/array/string/literal).
        // The runtime `map_result` validates substitution at runtime.
        let result_mapping = tool_props.remove("result-mapping");

        let target = match (component, function, channel) {
            (Some(component), Some(function), None) => ToolTarget::Component {
                component,
                function,
                mapping: MappingConfig {
                    param_mapping,
                    param_encoding,
                    result_decoding,
                    result_mapping,
                },
                input_schema,
                output_schema,
            },
            (None, None, Some(channel)) => {
                if param_mapping.is_some()
                    || param_encoding.is_some()
                    || result_decoding.is_some()
                    || result_mapping.is_some()
                {
                    return Err(anyhow::anyhow!(
                        "Server '{server_name}': tool '{tool_name}' has 'param-mapping', \
                         'param-encoding', 'result-decoding', or 'result-mapping' but those \
                         apply to component-backed tools only; for channel-backed tools, \
                         subscriptions own any mappings"
                    ));
                }
                let input_schema = input_schema.ok_or_else(|| {
                    anyhow::anyhow!(
                        "Server '{server_name}': channel-backed tool '{tool_name}' requires 'input-schema'"
                    )
                })?;
                ToolTarget::Channel {
                    channel,
                    input_schema,
                    output_schema,
                }
            }
            (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
                return Err(anyhow::anyhow!(
                    "Server '{server_name}': tool '{tool_name}' cannot have both \
                     'component'/'function' and 'channel'"
                ));
            }
            (Some(_), None, None) => {
                return Err(anyhow::anyhow!(
                    "Server '{server_name}': tool '{tool_name}' has 'component' but missing 'function'"
                ));
            }
            (None, Some(_), None) => {
                return Err(anyhow::anyhow!(
                    "Server '{server_name}': tool '{tool_name}' has 'function' but missing 'component'"
                ));
            }
            (None, None, None) => {
                return Err(anyhow::anyhow!(
                    "Server '{server_name}': tool '{tool_name}' must have either \
                     'component'/'function' or 'channel'"
                ));
            }
        };

        let description = match tool_props.remove("description") {
            Some(serde_json::Value::String(s)) => Some(s),
            Some(got) => {
                return Err(anyhow::anyhow!(
                    "Server '{server_name}': tool '{tool_name}' 'description' must be a string, got {got}"
                ));
            }
            None => None,
        };

        let propagate_request_meta = parse_propagated_meta_list(
            &mut tool_props,
            "propagate-request-meta",
            PropagationDirection::Inbound,
            server_name,
            &tool_name,
        )?;
        let propagate_result_meta = parse_propagated_meta_list(
            &mut tool_props,
            "propagate-result-meta",
            PropagationDirection::Outbound,
            server_name,
            &tool_name,
        )?;

        // Cross-config check: a `result-mapping.headers` target would be
        // silently overridden if any `propagate-result-meta` entry has that
        // target name with a different source.
        if let ToolTarget::Component {
            mapping:
                MappingConfig {
                    result_mapping: Some(rm),
                    ..
                },
            ..
        } = &target
            && let Some(serde_json::Value::Object(headers)) = rm.get("headers")
        {
            let mapped_targets: std::collections::HashSet<&str> =
                headers.keys().map(|s| s.as_str()).collect();
            for entry in &propagate_result_meta {
                if mapped_targets.contains(entry.target()) && entry.source() != entry.target() {
                    return Err(anyhow::anyhow!(
                        "Server '{server_name}': tool '{tool_name}': '{}' is written by \
                         'result-mapping.headers' but overridden by 'propagate-result-meta' \
                         entry '{} as {}'",
                        entry.target(),
                        entry.source(),
                        entry.target(),
                    ));
                }
            }
        }

        if !tool_props.is_empty() {
            let unknown: Vec<_> = tool_props.keys().collect();
            return Err(anyhow::anyhow!(
                "Server '{server_name}': tool '{tool_name}' has unknown properties: {unknown:?}"
            ));
        }

        tools.push(ToolConfig {
            name: tool_name,
            target,
            description,
            propagate_request_meta,
            propagate_result_meta,
        });
    }

    Ok(tools)
}

// The direction of propagation. Determines which side of each entry
// (source / target) must conform to MCP `_meta` key syntax (the other side
// is a Message header name and has looser rules).
#[derive(Debug, Clone, Copy)]
enum PropagationDirection {
    // Inbound: source is the MCP `_meta` key on the request; target is the
    // Message header name written on the inbound Message.
    Inbound,
    // Outbound: source is the Message header name on the reply Message;
    // target is the MCP `_meta` key written on the result.
    Outbound,
}

// Parse a `propagate-{request,result}-meta` array into Vec<PropagatedHeader>.
// Each entry is a string in the `"source"` or `"source as target"` form.
// The direction determines which side must conform to MCP `_meta` key syntax.
fn parse_propagated_meta_list(
    tool_props: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    direction: PropagationDirection,
    server_name: &str,
    tool_name: &str,
) -> Result<Vec<PropagatedHeader>> {
    let raw = match tool_props.remove(key) {
        Some(serde_json::Value::Array(items)) => items
            .into_iter()
            .map(|v| match v {
                serde_json::Value::String(s) => Ok(s),
                other => Err(anyhow::anyhow!(
                    "Server '{server_name}': tool '{tool_name}' '{key}' entries must be strings, got {other}"
                )),
            })
            .collect::<Result<Vec<_>>>()?,
        Some(got) => {
            return Err(anyhow::anyhow!(
                "Server '{server_name}': tool '{tool_name}' '{key}' must be an array of strings, got {got}"
            ));
        }
        None => Vec::new(),
    };
    raw.into_iter()
        .map(|s| {
            let entry = PropagatedHeader::parse(&s).map_err(|e| {
                anyhow::anyhow!(
                    "Server '{server_name}': tool '{tool_name}' '{key}' entry '{s}': {e}"
                )
            })?;
            let meta_side = match direction {
                PropagationDirection::Inbound => entry.source(),
                PropagationDirection::Outbound => entry.target(),
            };
            validate_mcp_meta_key(meta_side).map_err(|e| {
                anyhow::anyhow!(
                    "Server '{server_name}': tool '{tool_name}' '{key}' entry '{s}': '{meta_side}' is not a valid MCP _meta key: {e}"
                )
            })?;
            Ok(entry)
        })
        .collect()
}

// Validate an MCP `_meta` key per the spec (2025-11-25).
fn validate_mcp_meta_key(key: &str) -> std::result::Result<(), String> {
    if key.is_empty() {
        return Err("empty key".to_string());
    }
    let (prefix, name) = match key.rsplit_once('/') {
        Some((p, n)) => (Some(p), n),
        None => (None, key),
    };
    if let Some(prefix) = prefix {
        validate_meta_prefix(prefix)?;
    }
    validate_meta_name(name)?;
    Ok(())
}

fn validate_meta_prefix(prefix: &str) -> std::result::Result<(), String> {
    if prefix.is_empty() {
        return Err("prefix before '/' is empty".to_string());
    }
    for label in prefix.split('.') {
        validate_meta_label(label)?;
    }
    Ok(())
}

fn validate_meta_label(label: &str) -> std::result::Result<(), String> {
    let bytes = label.as_bytes();
    if bytes.is_empty() {
        return Err("empty prefix label".to_string());
    }
    if !bytes[0].is_ascii_alphabetic() {
        return Err(format!("prefix label '{label}' must start with a letter"));
    }
    if bytes.len() == 1 {
        return Ok(());
    }
    let last = *bytes.last().unwrap();
    if !last.is_ascii_alphanumeric() {
        return Err(format!(
            "prefix label '{label}' must end with a letter or digit"
        ));
    }
    for b in &bytes[1..bytes.len() - 1] {
        if !(b.is_ascii_alphanumeric() || *b == b'-') {
            return Err(format!(
                "prefix label '{label}' has invalid interior character"
            ));
        }
    }
    Ok(())
}

fn validate_meta_name(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() {
        // Empty name is permitted.
        return Ok(());
    }
    let bytes = name.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() {
        return Err(format!(
            "name '{name}' must begin with an alphanumeric character"
        ));
    }
    if bytes.len() == 1 {
        return Ok(());
    }
    let last = *bytes.last().unwrap();
    if !last.is_ascii_alphanumeric() {
        return Err(format!(
            "name '{name}' must end with an alphanumeric character"
        ));
    }
    for b in &bytes[1..bytes.len() - 1] {
        if !(b.is_ascii_alphanumeric() || matches!(*b, b'-' | b'_' | b'.')) {
            return Err(format!("name '{name}' has invalid interior character"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_handler() -> (McpServerConfigHandler, SharedConfig) {
        let config = shared_config();
        let handler = McpServerConfigHandler::new(Arc::clone(&config));
        (handler, config)
    }

    fn props(pairs: Vec<(&str, serde_json::Value)>) -> PropertyMap {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn parse_basic_server() {
        let (mut handler, config) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "add-two": {
                        "component": "math",
                        "function": "add-two"
                    }
                }),
            ),
        ]);

        handler
            .handle_category("server", "mcp", properties)
            .unwrap();

        let servers = config.lock().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "mcp");
        assert_eq!(servers[0].host, "127.0.0.1");
        assert_eq!(servers[0].port, 3001);
        assert!(servers[0].allowed_origins.is_none());
        assert_eq!(servers[0].tools.len(), 1);
        assert_eq!(servers[0].tools[0].name, "add-two");
        assert!(
            matches!(&servers[0].tools[0].target, ToolTarget::Component { component, function, .. }
                if component == "math" && function == "add-two")
        );
        assert!(servers[0].tools[0].description.is_none());
    }

    #[test]
    fn parse_server_with_all_options() {
        let (mut handler, config) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("host", serde_json::json!("0.0.0.0")),
            ("port", serde_json::json!(8080)),
            (
                "allowed-origins",
                serde_json::json!(["example.com", "localhost"]),
            ),
            (
                "tool",
                serde_json::json!({
                    "greet": {
                        "component": "greeter",
                        "function": "greet",
                        "description": "Greet someone by name"
                    }
                }),
            ),
        ]);

        handler
            .handle_category("server", "api", properties)
            .unwrap();

        let servers = config.lock().unwrap();
        assert_eq!(servers[0].host, "0.0.0.0");
        assert_eq!(servers[0].port, 8080);
        assert_eq!(
            servers[0].allowed_origins.as_deref(),
            Some(["example.com".to_string(), "localhost".to_string()].as_slice())
        );
        assert_eq!(
            servers[0].tools[0].description.as_deref(),
            Some("Greet someone by name")
        );
    }

    #[test]
    fn missing_port() {
        let (mut handler, _) = make_handler();
        let properties = props(vec![("type", serde_json::json!("mcp"))]);

        let result = handler.handle_category("server", "mcp", properties);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing required 'port'")
        );
    }

    #[test]
    fn function_without_component() {
        let (mut handler, _) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "bad": {
                        "function": "do-stuff"
                    }
                }),
            ),
        ]);

        let result = handler.handle_category("server", "mcp", properties);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("'function' but missing 'component'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn component_without_function() {
        let (mut handler, _) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "bad": {
                        "component": "math"
                    }
                }),
            ),
        ]);

        let result = handler.handle_category("server", "mcp", properties);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("'component' but missing 'function'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn no_component_no_channel() {
        let (mut handler, _) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "bad": {
                        "description": "unreachable tool"
                    }
                }),
            ),
        ]);

        let result = handler.handle_category("server", "mcp", properties);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("must have either"), "unexpected error: {err}");
    }

    #[test]
    fn channel_and_component_conflict() {
        let (mut handler, _) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "bad": {
                        "component": "math",
                        "function": "add",
                        "channel": "work-queue"
                    }
                }),
            ),
        ]);

        let result = handler.handle_category("server", "mcp", properties);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("cannot have both"), "unexpected error: {err}");
    }

    #[test]
    fn component_tool_with_param_and_result_mapping() {
        let (mut handler, config) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "shaped": {
                        "component": "service",
                        "function": "fetch",
                        "param-mapping": { "url": "https://example.com/{id}" },
                        "result-mapping": { "data": "{body}" }
                    }
                }),
            ),
        ]);
        handler
            .handle_category("server", "mcp", properties)
            .unwrap();
        let servers = config.lock().unwrap();
        match &servers[0].tools[0].target {
            ToolTarget::Component {
                mapping,
                output_schema,
                ..
            } => {
                assert!(mapping.param_mapping.is_some());
                assert!(mapping.result_mapping.is_some());
                assert!(output_schema.is_none());
            }
            other => panic!("expected Component target, got {other:?}"),
        }
    }

    #[test]
    fn component_tool_with_explicit_output_schema_allowed() {
        let (mut handler, config) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "describe-add": {
                        "component": "math",
                        "function": "add-two",
                        "output-schema": { "type": "object", "properties": {} }
                    }
                }),
            ),
        ]);
        handler
            .handle_category("server", "mcp", properties)
            .unwrap();
        let servers = config.lock().unwrap();
        assert!(matches!(
            &servers[0].tools[0].target,
            ToolTarget::Component {
                output_schema: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn component_tool_with_explicit_input_schema_allowed() {
        let (mut handler, config) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "add2": {
                        "component": "math",
                        "function": "add-two",
                        "input-schema": { "type": "object", "properties": {} }
                    }
                }),
            ),
        ]);
        handler
            .handle_category("server", "mcp", properties)
            .unwrap();
        let servers = config.lock().unwrap();
        assert!(matches!(
            &servers[0].tools[0].target,
            ToolTarget::Component {
                input_schema: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn channel_tool_with_mapping_is_error() {
        let (mut handler, _) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "bad": {
                        "channel": "events",
                        "input-schema": { "type": "object" },
                        "param-mapping": { "x": "{body}" }
                    }
                }),
            ),
        ]);
        let err = handler
            .handle_category("server", "mcp", properties)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("apply to component-backed tools only"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn tool_with_propagate_request_meta() {
        let (mut handler, config) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "propagator": {
                        "component": "c",
                        "function": "f",
                        "propagate-request-meta": [
                            "com.example.x/foo",
                            "com.example.y/bar as tracked-bar"
                        ]
                    }
                }),
            ),
        ]);
        handler
            .handle_category("server", "mcp", properties)
            .unwrap();
        let servers = config.lock().unwrap();
        let entries = &servers[0].tools[0].propagate_request_meta;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].source(), "com.example.x/foo");
        assert_eq!(entries[0].target(), "com.example.x/foo");
        assert_eq!(entries[1].source(), "com.example.y/bar");
        assert_eq!(entries[1].target(), "tracked-bar");
    }

    #[test]
    fn tool_with_propagate_result_meta() {
        let (mut handler, config) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "emitter": {
                        "component": "c",
                        "function": "f",
                        "propagate-result-meta": [
                            "x-ratelimit-remaining as com.example.tools/ratelimit-remaining"
                        ]
                    }
                }),
            ),
        ]);
        handler
            .handle_category("server", "mcp", properties)
            .unwrap();
        let servers = config.lock().unwrap();
        let entries = &servers[0].tools[0].propagate_result_meta;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source(), "x-ratelimit-remaining");
        assert_eq!(entries[0].target(), "com.example.tools/ratelimit-remaining");
    }

    #[test]
    fn propagate_request_meta_non_string_entry_is_error() {
        let (mut handler, _) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "bad": {
                        "component": "c",
                        "function": "f",
                        "propagate-request-meta": ["num", 42]
                    }
                }),
            ),
        ]);
        let err = handler
            .handle_category("server", "mcp", properties)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("'propagate-request-meta' entries must be strings"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn propagate_request_meta_invalid_meta_key_source_is_error() {
        let (mut handler, _) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "bad": {
                        "component": "c",
                        "function": "f",
                        "propagate-request-meta": ["-bad-prefix/x"]
                    }
                }),
            ),
        ]);
        let err = handler
            .handle_category("server", "mcp", properties)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not a valid MCP _meta key"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn propagate_result_meta_invalid_meta_key_target_is_error() {
        let (mut handler, _) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "bad": {
                        "component": "c",
                        "function": "f",
                        "propagate-result-meta": ["x-source as bad-name-"]
                    }
                }),
            ),
        ]);
        let err = handler
            .handle_category("server", "mcp", properties)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not a valid MCP _meta key"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn propagate_meta_accepts_reverse_dns_keys() {
        let (mut handler, _) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "ok": {
                        "component": "c",
                        "function": "f",
                        "propagate-request-meta": ["com.example.tools/tag"],
                        "propagate-result-meta": ["x-ratelimit-remaining as com.example.tools/ratelimit-remaining"]
                    }
                }),
            ),
        ]);
        handler
            .handle_category("server", "mcp", properties)
            .unwrap();
    }

    #[test]
    fn propagate_meta_accepts_reserved_mcp_prefix() {
        let (mut handler, _) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "ok": {
                        "component": "c",
                        "function": "f",
                        "propagate-request-meta": ["io.modelcontextprotocol/related-task"]
                    }
                }),
            ),
        ]);
        handler
            .handle_category("server", "mcp", properties)
            .unwrap();
    }

    #[test]
    fn result_mapping_header_overridden_by_propagate_result_meta_is_error() {
        let (mut handler, _) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "bad": {
                        "component": "c",
                        "function": "f",
                        "result-mapping": {
                            "headers": { "x-tag": "{label}" }
                        },
                        "propagate-result-meta": ["other-source as x-tag"]
                    }
                }),
            ),
        ]);
        let err = handler
            .handle_category("server", "mcp", properties)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("written by 'result-mapping.headers'") && err.contains("overridden"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn result_mapping_header_with_matching_propagate_result_meta_identity_is_ok() {
        let (mut handler, _) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "ok": {
                        "component": "c",
                        "function": "f",
                        "result-mapping": {
                            "headers": { "x-tag": "{label}" }
                        },
                        "propagate-result-meta": ["x-tag"]
                    }
                }),
            ),
        ]);
        handler
            .handle_category("server", "mcp", properties)
            .unwrap();
    }

    #[test]
    fn channel_tool_with_schema() {
        let (mut handler, config) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "process": {
                        "channel": "work-queue",
                        "description": "Submit work",
                        "input-schema": {
                            "type": "object",
                            "properties": {
                                "task": { "type": "string" }
                            },
                            "required": ["task"]
                        }
                    }
                }),
            ),
        ]);

        handler
            .handle_category("server", "mcp", properties)
            .unwrap();

        let servers = config.lock().unwrap();
        assert_eq!(servers[0].tools.len(), 1);
        assert_eq!(servers[0].tools[0].name, "process");
        assert_eq!(
            servers[0].tools[0].description.as_deref(),
            Some("Submit work")
        );
        assert!(matches!(
            &servers[0].tools[0].target,
            ToolTarget::Channel { channel, input_schema, .. }
                if channel == "work-queue"
                && input_schema.get("properties").is_some()
        ));
    }

    #[test]
    fn channel_tool_requires_schema() {
        let (mut handler, _) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "process": {
                        "channel": "work-queue",
                        "description": "Submit work"
                    }
                }),
            ),
        ]);

        let result = handler.handle_category("server", "mcp", properties);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("requires 'input-schema'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn no_tools_no_selector_errors() {
        let (mut handler, _) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
        ]);

        let result = handler.handle_category("server", "mcp", properties);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no tools and no component-selector")
        );
    }

    #[test]
    fn selector_only_valid() {
        let (mut handler, config) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            ("component-selector", serde_json::json!("!dependents")),
        ]);

        handler
            .handle_category("server", "mcp", properties)
            .unwrap();

        let servers = config.lock().unwrap();
        assert!(servers[0].component_selector.is_some());
        assert!(servers[0].tools.is_empty());
    }

    #[test]
    fn selector_matches_mcp_type() {
        let handler = McpServerConfigHandler::new(shared_config());
        let claims = handler.claimed_categories();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].category, "server");
        assert!(claims[0].selector.is_some());

        let selector = claims[0].selector.as_ref().unwrap();
        let mut matching = HashMap::new();
        matching.insert("type".to_string(), Some("mcp".to_string()));
        assert!(selector.matches(&matching));

        let mut non_matching = HashMap::new();
        non_matching.insert("type".to_string(), Some("http".to_string()));
        assert!(!selector.matches(&non_matching));
    }

    #[test]
    fn unknown_tool_property() {
        let (mut handler, _) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            (
                "tool",
                serde_json::json!({
                    "bad": {
                        "component": "math",
                        "function": "add-two",
                        "bogus": "value"
                    }
                }),
            ),
        ]);

        let result = handler.handle_category("server", "mcp", properties);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unknown properties")
        );
    }

    #[test]
    fn wildcard_allowed_origins() {
        let (mut handler, config) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            ("allowed-origins", serde_json::json!("*")),
            ("component-selector", serde_json::json!("!dependents")),
        ]);

        handler
            .handle_category("server", "mcp", properties)
            .unwrap();

        let servers = config.lock().unwrap();
        assert_eq!(
            servers[0].allowed_origins.as_deref(),
            Some(["*".to_string()].as_slice())
        );
    }

    #[test]
    fn selector_and_tools_coexist() {
        let (mut handler, config) = make_handler();
        let properties = props(vec![
            ("type", serde_json::json!("mcp")),
            ("port", serde_json::json!(3001)),
            ("component-selector", serde_json::json!("labels.domain=api")),
            (
                "tool",
                serde_json::json!({
                    "custom-tool": {
                        "component": "math",
                        "function": "add-two",
                        "description": "Custom description"
                    }
                }),
            ),
        ]);

        handler
            .handle_category("server", "mcp", properties)
            .unwrap();

        let servers = config.lock().unwrap();
        assert!(servers[0].component_selector.is_some());
        assert_eq!(servers[0].tools.len(), 1);
    }
}
