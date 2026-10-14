//! The `[tool]`, `[toolset]`, `[mcp-tool]` and `[mcp-toolset]` config
//! handlers.

use std::collections::HashMap;

use anyhow::{Result, bail};
use composable_runtime::{
    CategoryClaim, ConfigHandler, Definition, GenericDefinition, PropertyMap,
};
use serde_json::{Map, Value, json};

/// Adapts a `composable:runtime/function` to `composable:tools/tool`.
const TOOL_ADAPTER: &str = "oci://ghcr.io/modulewise/component/tool-adapter:0.1.0";

/// Generates a `composable:runtime/function` for a target's function.
const FUNCTION_FACTORY: &str = "oci://ghcr.io/modulewise/component/function-factory:0.5.1";

/// Maps between JSON and the WIT values of generated functions.
const JSON_MAPPER: &str = "oci://ghcr.io/modulewise/component/json-mapper:0.4.0";

/// Generates a `composable:tools/toolset` over named tools and toolsets.
const TOOLSET_FACTORY: &str = "oci://ghcr.io/modulewise/component/toolset-factory:0.1.2";

/// Exports `composable:tools/tool` for one tool of a remote MCP server.
const MCP_TOOL: &str = "oci://ghcr.io/modulewise/component/mcp-tool:0.3.0";

/// Exports `composable:tools/toolset` for the tools of a remote MCP server.
const MCP_TOOLSET: &str = "oci://ghcr.io/modulewise/component/mcp-toolset:0.3.0";

/// An MCP client.
const MCP_CLIENT: &str = "oci://ghcr.io/modulewise/component/mcp-client:0.3.0";

/// An HTTP client over `wasi:http`.
const HTTP_CLIENT: &str = "oci://ghcr.io/modulewise/component/http-client:0.4.0";

/// Drops log records below a configured level.
const LOG_LEVELS: &str = "oci://ghcr.io/componentized/logging/levels:0.3.0";

/// Writes log records to stdout.
const LOG_TO_STDOUT: &str = "oci://ghcr.io/componentized/logging/to-stdout:0.3.0";

/// Redirects stdout to stderr.
const STDOUT_TO_STDERR: &str = "oci://ghcr.io/componentized/cli/stdout-to-stderr:0.2.0";

/// The mapper every generated function imports, emitted once.
const JSON_MAPPER_NAME: &str = "_json-mapper";

/// The logger every MCP client imports, emitted once.
const MCP_LOGGER_NAME: &str = "_mcp-logger";

/// The writer behind the MCP logger, emitted once.
const MCP_LOG_WRITER_NAME: &str = "_mcp-log-writer";

/// The redirect of the MCP log writer's stdout to stderr, emitted once.
const MCP_STDOUT_TO_STDERR_NAME: &str = "_mcp-stdout-to-stderr";

/// The `wasi:http` capability every MCP HTTP client imports, emitted once.
const MCP_HTTP_NAME: &str = "_mcp-http";

/// The `wasi:cli` capability of the MCP log writer, writing to the host's
/// stderr, emitted once.
const MCP_CLI_NAME: &str = "_mcp-cli";

/// The `wasi:clocks` capability of the MCP log writer, emitted once.
const MCP_CLOCKS_NAME: &str = "_mcp-clocks";

/// The lowest level of the MCP clients' log records that is written.
const MCP_LOG_LEVEL: &str = "warn";

/// The function-factory options a `[tool]` passes through.
const FUNCTION_OPTIONS: [&str; 2] = ["function", "world"];

/// The tool-adapter options a `[tool]` passes through.
const ADAPTER_OPTIONS: [&str; 9] = [
    "description",
    "title",
    "read-only-hint",
    "destructive-hint",
    "idempotent-hint",
    "open-world-hint",
    "spec-version",
    "request-meta",
    "response-meta",
];

/// Handles `[tool.NAME]`, `[toolset.NAME]`, `[mcp-tool.NAME]` and
/// `[mcp-toolset.NAME]` definitions.
///
/// A tool is a tool-adapter named `NAME` over a function generated for its
/// `target`. A toolset is a component named `NAME`, generated over its `tools`
/// and/or `toolsets`. An MCP tool or toolset is a component named `NAME`
/// composed with an MCP client and an HTTP client, for its `server-url`.
#[derive(Default)]
pub(crate) struct ToolConfigHandler {
    json_mapper_emitted: bool,
    mcp_shared_emitted: bool,
}

impl ConfigHandler for ToolConfigHandler {
    fn claimed_categories(&self) -> Vec<CategoryClaim> {
        vec![
            CategoryClaim::all("tool"),
            CategoryClaim::all("toolset"),
            CategoryClaim::all("mcp-tool"),
            CategoryClaim::all("mcp-toolset"),
        ]
    }

    fn claimed_properties(&self) -> HashMap<&str, &[&str]> {
        const TOOL: [&str; 12] = [
            "target",
            "function",
            "world",
            "description",
            "title",
            "read-only-hint",
            "destructive-hint",
            "idempotent-hint",
            "open-world-hint",
            "spec-version",
            "request-meta",
            "response-meta",
        ];
        HashMap::from([
            ("tool", TOOL.as_slice()),
            ("toolset", ["tools", "toolsets"].as_slice()),
            ("mcp-tool", ["server-url", "tool-name"].as_slice()),
            ("mcp-toolset", ["server-url"].as_slice()),
        ])
    }

    fn handle_definition(&mut self, definition: GenericDefinition) -> Result<Vec<Definition>> {
        let GenericDefinition {
            category,
            name,
            mut properties,
        } = definition;
        let definitions = match category.as_str() {
            "tool" => with_shared(
                &mut self.json_mapper_emitted,
                tool(&name, &mut properties)?,
                json_mapper,
            ),
            "toolset" => toolset(&name, &mut properties)?,
            "mcp-tool" => with_shared(
                &mut self.mcp_shared_emitted,
                mcp_tool(&name, &mut properties)?,
                mcp_shared,
            ),
            "mcp-toolset" => with_shared(
                &mut self.mcp_shared_emitted,
                mcp_toolset(&name, &mut properties)?,
                mcp_shared,
            ),
            other => bail!("ToolConfigHandler received unexpected category '{other}'"),
        };
        Ok(definitions)
    }
}

/// `definitions`, followed by the `shared` definitions unless `emitted` says
/// they were emitted already.
fn with_shared(
    emitted: &mut bool,
    mut definitions: Vec<Definition>,
    shared: fn() -> Vec<Definition>,
) -> Vec<Definition> {
    if !*emitted {
        *emitted = true;
        definitions.extend(shared());
    }
    definitions
}

fn tool(name: &str, properties: &mut PropertyMap) -> Result<Vec<Definition>> {
    let target = match properties.remove("target") {
        Some(Value::String(target)) => target,
        Some(got) => bail!("Tool '{name}': 'target' must be a string, got {got}"),
        None => bail!("Tool '{name}' is missing required 'target'"),
    };
    let function = format!("_{name}-function");
    let factory = format!("_{name}-function-factory");

    let mut factory_config =
        Map::from_iter([("wit".to_string(), json!(format!("${{wit({target})}}")))]);
    for option in FUNCTION_OPTIONS {
        if let Some(value) = properties.remove(option) {
            factory_config.insert(option.to_string(), value);
        }
    }
    let mut adapter_config = Map::from_iter([("name".to_string(), json!(name))]);
    for option in ADAPTER_OPTIONS {
        if let Some(value) = properties.remove(option) {
            adapter_config.insert(option.to_string(), value);
        }
    }

    Ok(vec![
        component(
            name,
            json!({ "uri": TOOL_ADAPTER, "imports": [function], "config": adapter_config }),
        ),
        component(
            &function,
            json!({ "uri": format!("factory:{factory}"), "imports": [target, JSON_MAPPER_NAME] }),
        ),
        component(
            &factory,
            json!({ "uri": FUNCTION_FACTORY, "config": factory_config }),
        ),
    ])
}

/// The mapper every generated function shares.
fn json_mapper() -> Vec<Definition> {
    vec![component(JSON_MAPPER_NAME, json!({ "uri": JSON_MAPPER }))]
}

fn toolset(name: &str, properties: &mut PropertyMap) -> Result<Vec<Definition>> {
    let tools = names(name, "tools", properties)?;
    let toolsets = names(name, "toolsets", properties)?;
    let factory = format!("_{name}-factory");

    // The toolset-factory reads each as a comma-separated list, absent if empty.
    let mut factory_config = Map::new();
    for (key, members) in [("tools", &tools), ("toolsets", &toolsets)] {
        if !members.is_empty() {
            factory_config.insert(key.to_string(), json!(members.join(",")));
        }
    }
    let imports: Vec<&String> = tools.iter().chain(&toolsets).collect();

    Ok(vec![
        component(
            name,
            json!({ "uri": format!("factory:{factory}"), "imports": imports }),
        ),
        component(
            &factory,
            json!({ "uri": TOOLSET_FACTORY, "config": factory_config }),
        ),
    ])
}

/// A list of component names, or empty if absent.
fn names(name: &str, key: &str, properties: &mut PropertyMap) -> Result<Vec<String>> {
    match properties.remove(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .into_iter()
            .map(|item| match item {
                Value::String(member) => Ok(member),
                got => bail!("Toolset '{name}': '{key}' must list names, got {got}"),
            })
            .collect(),
        Some(got) => bail!("Toolset '{name}': '{key}' must be a list of names, got {got}"),
    }
}

fn mcp_tool(name: &str, properties: &mut PropertyMap) -> Result<Vec<Definition>> {
    let server_url = server_url("MCP tool", name, properties)?;
    let tool_name = match properties.remove("tool-name") {
        None => name.to_string(),
        Some(Value::String(tool_name)) => tool_name,
        Some(got) => bail!("MCP tool '{name}': 'tool-name' must be a string, got {got}"),
    };
    let config = Map::from_iter([
        ("server-url".to_string(), json!(server_url)),
        ("tool-name".to_string(), json!(tool_name)),
    ]);
    Ok(with_mcp_client(name, MCP_TOOL, config))
}

fn mcp_toolset(name: &str, properties: &mut PropertyMap) -> Result<Vec<Definition>> {
    let server_url = server_url("MCP toolset", name, properties)?;
    let config = Map::from_iter([("server-url".to_string(), json!(server_url))]);
    Ok(with_mcp_client(name, MCP_TOOLSET, config))
}

/// The logger and capabilities every MCP client shares.
fn mcp_shared() -> Vec<Definition> {
    vec![
        component(
            MCP_LOGGER_NAME,
            json!({
                "uri": LOG_LEVELS,
                "imports": [MCP_LOG_WRITER_NAME],
                "config": { "*": MCP_LOG_LEVEL },
            }),
        ),
        component(
            MCP_LOG_WRITER_NAME,
            json!({
                "uri": LOG_TO_STDOUT,
                "imports": [MCP_STDOUT_TO_STDERR_NAME, MCP_CLI_NAME, MCP_CLOCKS_NAME],
            }),
        ),
        component(
            MCP_STDOUT_TO_STDERR_NAME,
            json!({ "uri": STDOUT_TO_STDERR, "imports": [MCP_CLI_NAME] }),
        ),
        capability(MCP_HTTP_NAME, json!({ "type": "wasi:http" })),
        capability(
            MCP_CLI_NAME,
            json!({ "type": "wasi:cli", "inherit-stderr": true }),
        ),
        capability(MCP_CLOCKS_NAME, json!({ "type": "wasi:clocks" })),
    ]
}

/// The component named `name` from `uri` with `config`, with an MCP client
/// and an HTTP client of its own.
fn with_mcp_client(name: &str, uri: &str, config: Map<String, Value>) -> Vec<Definition> {
    let mcp_client = format!("_{name}-mcp-client");
    let http_client = format!("_{name}-http-client");
    vec![
        component(
            name,
            json!({ "uri": uri, "imports": [mcp_client], "config": config }),
        ),
        component(
            &mcp_client,
            json!({ "uri": MCP_CLIENT, "imports": [http_client, MCP_LOGGER_NAME] }),
        ),
        component(
            &http_client,
            json!({ "uri": HTTP_CLIENT, "imports": [MCP_HTTP_NAME] }),
        ),
    ]
}

/// The required `server-url`, an `http` or `https` URL.
fn server_url(kind: &str, name: &str, properties: &mut PropertyMap) -> Result<String> {
    match properties.remove("server-url") {
        Some(Value::String(url)) if url.starts_with("http://") || url.starts_with("https://") => {
            Ok(url)
        }
        Some(Value::String(url)) => {
            bail!("{kind} '{name}': 'server-url' must be an http or https URL, got '{url}'")
        }
        Some(got) => bail!("{kind} '{name}': 'server-url' must be a string, got {got}"),
        None => bail!("{kind} '{name}' is missing required 'server-url'"),
    }
}

/// A `[component.NAME]` definition, for the core component handler.
fn component(name: &str, properties: Value) -> Definition {
    generic("component", name, properties)
}

/// A `[capability.NAME]` definition, for the core capability handler.
fn capability(name: &str, properties: Value) -> Definition {
    generic("capability", name, properties)
}

/// A `[CATEGORY.NAME]` definition, for the handler of its category.
fn generic(category: &str, name: &str, properties: Value) -> Definition {
    let Value::Object(properties) = properties else {
        unreachable!("{category} properties are an object");
    };
    Definition::Generic(GenericDefinition {
        category: category.to_string(),
        name: name.to_string(),
        properties: properties.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(category: &str, name: &str, properties: Value) -> GenericDefinition {
        let Value::Object(properties) = properties else {
            panic!("properties must be an object");
        };
        GenericDefinition {
            category: category.to_string(),
            name: name.to_string(),
            properties: properties.into_iter().collect(),
        }
    }

    /// The `[component.*]` definitions produced, by name.
    fn components(produced: &[Definition]) -> HashMap<String, Value> {
        produced_in("component", produced)
    }

    /// The `[capability.*]` definitions produced, by name.
    fn capabilities(produced: &[Definition]) -> HashMap<String, Value> {
        produced_in("capability", produced)
    }

    /// The `[CATEGORY.*]` definitions produced, by name.
    fn produced_in(category: &str, produced: &[Definition]) -> HashMap<String, Value> {
        produced
            .iter()
            .filter_map(|produced| match produced {
                Definition::Generic(definition) if definition.category == category => Some((
                    definition.name.clone(),
                    Value::Object(definition.properties.clone().into_iter().collect()),
                )),
                Definition::Generic(_) => None,
                other => panic!("expected a generic definition, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_tool_is_an_adapter_over_a_generated_function() {
        let produced = ToolConfigHandler::default()
            .handle_definition(definition(
                "tool",
                "forecast",
                json!({ "target": "weather", "description": "The forecast" }),
            ))
            .unwrap();
        let components = components(&produced);

        assert_eq!(
            components["forecast"],
            json!({
                "uri": TOOL_ADAPTER,
                "imports": ["_forecast-function"],
                "config": { "name": "forecast", "description": "The forecast" },
            })
        );
        assert_eq!(
            components["_forecast-function"],
            json!({
                "uri": "factory:_forecast-function-factory",
                "imports": ["weather", "_json-mapper"],
            })
        );
        assert_eq!(
            components["_forecast-function-factory"],
            json!({ "uri": FUNCTION_FACTORY, "config": { "wit": "${wit(weather)}" } })
        );
        assert_eq!(components["_json-mapper"], json!({ "uri": JSON_MAPPER }));
        assert_eq!(produced.len(), 4);
    }

    #[test]
    fn a_tool_passes_its_options_to_the_factory_and_the_adapter() {
        let produced = ToolConfigHandler::default()
            .handle_definition(definition(
                "tool",
                "forecast",
                json!({
                    "target": "weather",
                    "function": "daily",
                    "world": "forecaster",
                    "read-only-hint": true,
                    "request-meta": { "units": "x-units" },
                }),
            ))
            .unwrap();
        let components = components(&produced);

        assert_eq!(
            components["_forecast-function-factory"]["config"],
            json!({ "wit": "${wit(weather)}", "function": "daily", "world": "forecaster" })
        );
        assert_eq!(
            components["forecast"]["config"],
            json!({
                "name": "forecast",
                "read-only-hint": true,
                "request-meta": { "units": "x-units" },
            })
        );
    }

    #[test]
    fn the_json_mapper_is_emitted_only_once_and_shared_by_all_tools() {
        let mut handler = ToolConfigHandler::default();
        let first = handler
            .handle_definition(definition(
                "tool",
                "forecast",
                json!({ "target": "weather" }),
            ))
            .unwrap();
        let second = handler
            .handle_definition(definition("tool", "alerts", json!({ "target": "weather" })))
            .unwrap();

        assert!(components(&first).contains_key("_json-mapper"));
        let second = components(&second);
        assert!(!second.contains_key("_json-mapper"));
        assert_eq!(
            second["_alerts-function"]["imports"],
            json!(["weather", "_json-mapper"])
        );
    }

    #[test]
    fn a_tool_requires_a_target() {
        let error = ToolConfigHandler::default()
            .handle_definition(definition("tool", "forecast", json!({})))
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing required 'target'"), "{error}");
    }

    #[test]
    fn a_toolset_is_generated_over_its_members() {
        let produced = ToolConfigHandler::default()
            .handle_definition(definition(
                "toolset",
                "outlook",
                json!({ "tools": ["forecast", "alerts"], "toolsets": ["warnings"] }),
            ))
            .unwrap();
        let components = components(&produced);

        assert_eq!(
            components["outlook"],
            json!({
                "uri": "factory:_outlook-factory",
                "imports": ["forecast", "alerts", "warnings"],
            })
        );
        assert_eq!(
            components["_outlook-factory"],
            json!({
                "uri": TOOLSET_FACTORY,
                "config": { "tools": "forecast,alerts", "toolsets": "warnings" },
            })
        );
        assert_eq!(produced.len(), 2);
    }

    #[test]
    fn a_toolset_leaves_out_an_empty_member_list() {
        let produced = ToolConfigHandler::default()
            .handle_definition(definition(
                "toolset",
                "outlook",
                json!({ "tools": ["forecast"] }),
            ))
            .unwrap();
        assert_eq!(
            components(&produced)["_outlook-factory"]["config"],
            json!({ "tools": "forecast" })
        );
    }

    #[test]
    fn a_toolset_can_be_completely_empty() {
        for properties in [json!({}), json!({ "tools": [], "toolsets": [] })] {
            let produced = ToolConfigHandler::default()
                .handle_definition(definition("toolset", "outlook", properties.clone()))
                .unwrap();
            let components = components(&produced);

            assert_eq!(
                components["outlook"],
                json!({ "uri": "factory:_outlook-factory", "imports": [] }),
                "{properties}"
            );
            assert_eq!(
                components["_outlook-factory"],
                json!({ "uri": TOOLSET_FACTORY, "config": {} }),
                "{properties}"
            );
            assert_eq!(produced.len(), 2, "{properties}");
        }
    }

    #[test]
    fn an_mcp_tool_is_a_tool_composed_with_mcp_and_http_clients() {
        let produced = ToolConfigHandler::default()
            .handle_definition(definition(
                "mcp-tool",
                "multiply",
                json!({ "server-url": "http://localhost:3001/mcp" }),
            ))
            .unwrap();
        let components = components(&produced);

        assert_eq!(
            components["multiply"],
            json!({
                "uri": MCP_TOOL,
                "imports": ["_multiply-mcp-client"],
                "config": { "server-url": "http://localhost:3001/mcp", "tool-name": "multiply" },
            })
        );
        assert_eq!(
            components["_multiply-mcp-client"],
            json!({ "uri": MCP_CLIENT, "imports": ["_multiply-http-client", MCP_LOGGER_NAME] })
        );
        assert_eq!(
            components["_multiply-http-client"],
            json!({ "uri": HTTP_CLIENT, "imports": [MCP_HTTP_NAME] })
        );
        // The first MCP definition also produces the shared logger and
        // capabilities.
        assert_eq!(produced.len(), 3 + mcp_shared().len());
    }

    #[test]
    fn an_mcp_tool_is_named_on_the_server_by_its_tool_name() {
        let produced = ToolConfigHandler::default()
            .handle_definition(definition(
                "mcp-tool",
                "sum",
                json!({ "server-url": "https://example.com/mcp", "tool-name": "get-sum" }),
            ))
            .unwrap();
        assert_eq!(
            components(&produced)["sum"]["config"],
            json!({ "server-url": "https://example.com/mcp", "tool-name": "get-sum" })
        );
    }

    #[test]
    fn an_mcp_tool_requires_a_string_tool_name() {
        let error = ToolConfigHandler::default()
            .handle_definition(definition(
                "mcp-tool",
                "multiply",
                json!({ "server-url": "http://localhost:3001/mcp", "tool-name": ["multiply"] }),
            ))
            .unwrap_err()
            .to_string();
        assert!(error.contains("'tool-name' must be a string"), "{error}");
    }

    #[test]
    fn an_mcp_toolset_is_a_toolset_composed_with_mcp_and_http_clients() {
        let produced = ToolConfigHandler::default()
            .handle_definition(definition(
                "mcp-toolset",
                "calculator",
                json!({ "server-url": "http://localhost:3001/mcp" }),
            ))
            .unwrap();
        let components = components(&produced);

        assert_eq!(
            components["calculator"],
            json!({
                "uri": MCP_TOOLSET,
                "imports": ["_calculator-mcp-client"],
                "config": { "server-url": "http://localhost:3001/mcp" },
            })
        );
        assert_eq!(
            components["_calculator-mcp-client"],
            json!({ "uri": MCP_CLIENT, "imports": ["_calculator-http-client", MCP_LOGGER_NAME] })
        );
        assert_eq!(
            components["_calculator-http-client"],
            json!({ "uri": HTTP_CLIENT, "imports": [MCP_HTTP_NAME] })
        );
        // The first MCP definition also produces the shared logger and
        // capabilities.
        assert_eq!(produced.len(), 3 + mcp_shared().len());
    }

    #[test]
    fn the_mcp_logger_and_capabilities_are_emitted_only_once_and_shared_by_all_clients() {
        let mut handler = ToolConfigHandler::default();
        let first = handler
            .handle_definition(definition(
                "mcp-tool",
                "multiply",
                json!({ "server-url": "http://localhost:3001/mcp" }),
            ))
            .unwrap();
        let second = handler
            .handle_definition(definition(
                "mcp-toolset",
                "calculator",
                json!({ "server-url": "http://localhost:3001/mcp" }),
            ))
            .unwrap();

        let shared_components = components(&first);
        assert_eq!(
            shared_components[MCP_LOGGER_NAME],
            json!({
                "uri": LOG_LEVELS,
                "imports": [MCP_LOG_WRITER_NAME],
                "config": { "*": "warn" },
            })
        );
        assert_eq!(
            shared_components[MCP_LOG_WRITER_NAME],
            json!({
                "uri": LOG_TO_STDOUT,
                "imports": [MCP_STDOUT_TO_STDERR_NAME, MCP_CLI_NAME, MCP_CLOCKS_NAME],
            })
        );
        assert_eq!(
            shared_components[MCP_STDOUT_TO_STDERR_NAME],
            json!({ "uri": STDOUT_TO_STDERR, "imports": [MCP_CLI_NAME] })
        );
        let shared_capabilities = capabilities(&first);
        assert_eq!(
            shared_capabilities[MCP_HTTP_NAME],
            json!({ "type": "wasi:http" })
        );
        assert_eq!(
            shared_capabilities[MCP_CLI_NAME],
            json!({ "type": "wasi:cli", "inherit-stderr": true })
        );
        assert_eq!(
            shared_capabilities[MCP_CLOCKS_NAME],
            json!({ "type": "wasi:clocks" })
        );

        assert_eq!(second.len(), 3);
        let second = components(&second);
        assert_eq!(
            second["_calculator-mcp-client"]["imports"],
            json!(["_calculator-http-client", MCP_LOGGER_NAME])
        );
        assert_eq!(
            second["_calculator-http-client"]["imports"],
            json!([MCP_HTTP_NAME])
        );
    }

    #[test]
    fn an_mcp_tool_or_toolset_requires_an_http_server_url() {
        for category in ["mcp-tool", "mcp-toolset"] {
            for (properties, expected) in [
                (json!({}), "missing required 'server-url'"),
                (
                    json!({ "server-url": 3001 }),
                    "'server-url' must be a string",
                ),
                (
                    json!({ "server-url": "localhost:3001/mcp" }),
                    "'server-url' must be an http or https URL",
                ),
            ] {
                let error = ToolConfigHandler::default()
                    .handle_definition(definition(category, "calculator", properties))
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(expected), "{category}: {error}");
            }
        }
    }
}
