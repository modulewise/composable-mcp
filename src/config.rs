//! The `[tool]` and `[toolset]` config handlers.

use std::collections::HashMap;

use anyhow::{Result, bail};
use composable_runtime::{
    CategoryClaim, ConfigHandler, Definition, GenericDefinition, PropertyMap,
};
use serde_json::{Map, Value, json};

/// Adapts a `composable:runtime/function` to `composable:tools/tool`.
const TOOL_ADAPTER: &str = "oci://ghcr.io/modulewise/component/tool-adapter:0.1.0";

/// Generates a `composable:runtime/function` for a target's function.
const FUNCTION_FACTORY: &str = "oci://ghcr.io/modulewise/component/function-factory:0.5.0";

/// Maps between JSON and the WIT values of generated functions.
const JSON_MAPPER: &str = "oci://ghcr.io/modulewise/component/json-mapper:0.4.0";

/// Generates a `composable:tools/toolset` over named tools and toolsets.
const TOOLSET_FACTORY: &str = "oci://ghcr.io/modulewise/component/toolset-factory:0.1.0";

/// The mapper every generated function imports, emitted once.
const JSON_MAPPER_NAME: &str = "_json-mapper";

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

/// Handles `[tool.NAME]` and `[toolset.NAME]` definitions.
///
/// A tool is a tool-adapter named `NAME` over a function generated for its
/// `target`. A toolset is a component named `NAME`, generated over its `tools`
/// and/or `toolsets`.
#[derive(Default)]
pub(crate) struct ToolConfigHandler {
    json_mapper_emitted: bool,
}

impl ConfigHandler for ToolConfigHandler {
    fn claimed_categories(&self) -> Vec<CategoryClaim> {
        vec![CategoryClaim::all("tool"), CategoryClaim::all("toolset")]
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
        ])
    }

    fn handle_definition(&mut self, definition: GenericDefinition) -> Result<Vec<Definition>> {
        let GenericDefinition {
            category,
            name,
            mut properties,
        } = definition;
        match category.as_str() {
            "tool" => self.tool(&name, &mut properties),
            "toolset" => toolset(&name, &mut properties),
            other => bail!("ToolConfigHandler received unexpected category '{other}'"),
        }
    }
}

impl ToolConfigHandler {
    fn tool(&mut self, name: &str, properties: &mut PropertyMap) -> Result<Vec<Definition>> {
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

        let mut definitions = vec![
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
        ];
        if !self.json_mapper_emitted {
            self.json_mapper_emitted = true;
            definitions.push(component(JSON_MAPPER_NAME, json!({ "uri": JSON_MAPPER })));
        }
        Ok(definitions)
    }
}

fn toolset(name: &str, properties: &mut PropertyMap) -> Result<Vec<Definition>> {
    let tools = names(name, "tools", properties)?;
    let toolsets = names(name, "toolsets", properties)?;
    if tools.is_empty() && toolsets.is_empty() {
        bail!("Toolset '{name}' requires at least one of 'tools' or 'toolsets'");
    }
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

/// A `[component.NAME]` definition, for the core component handler.
fn component(name: &str, properties: Value) -> Definition {
    let Value::Object(properties) = properties else {
        unreachable!("component properties are an object");
    };
    Definition::Generic(GenericDefinition {
        category: "component".to_string(),
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
    fn components(produced: Vec<Definition>) -> HashMap<String, Value> {
        produced
            .into_iter()
            .map(|produced| match produced {
                Definition::Generic(definition) if definition.category == "component" => (
                    definition.name,
                    Value::Object(definition.properties.into_iter().collect()),
                ),
                other => panic!("expected a component definition, got {other:?}"),
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
        let components = components(produced);

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
        assert_eq!(components.len(), 4);
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
        let components = components(produced);

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

        assert!(components(first).contains_key("_json-mapper"));
        let second = components(second);
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
        let components = components(produced);

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
        assert_eq!(components.len(), 2);
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
            components(produced)["_outlook-factory"]["config"],
            json!({ "tools": "forecast" })
        );
    }

    #[test]
    fn a_toolset_requires_at_least_one_member() {
        let error = ToolConfigHandler::default()
            .handle_definition(definition("toolset", "outlook", json!({ "tools": [] })))
            .unwrap_err()
            .to_string();
        assert!(error.contains("requires at least one"), "{error}");
    }
}
