use rmcp::model::Tool;
use serde_json::json;

use composable_runtime::Function;

/// Mapper that converts core types to MCP Tools
pub struct McpMapper;

impl McpMapper {
    /// Convert a Function to an MCP Tool.
    ///
    /// `tool_name` is the name as it will appear to MCP clients.
    /// `description` overrides the function's docs when provided.
    pub fn function_to_tool(
        function: &Function,
        tool_name: &str,
        description: Option<&str>,
    ) -> Tool {
        let description = if let Some(desc) = description {
            desc.to_string()
        } else if function.docs().is_empty() {
            format!("Call {} function", function.function_name())
        } else {
            function.docs().to_string()
        };

        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();

        for param in function.params() {
            let mut param_schema = if param.is_optional {
                Self::flatten_schema_if_possible(&param.json_schema)
            } else {
                param.json_schema.clone()
            };

            if let serde_json::Value::Object(ref mut schema_obj) = param_schema {
                schema_obj.insert(
                    "description".to_string(),
                    serde_json::Value::String(format!("Parameter: {}", param.name)),
                );
            }
            properties.insert(param.name.clone(), param_schema);
            if !param.is_optional {
                required.push(param.name.clone());
            }
        }

        let input_schema = json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        });

        let mut tool = Tool::new_with_raw(
            tool_name.to_string(),
            Some(description.into()),
            input_schema.as_object().unwrap().clone(),
        )
        .with_title(tool_name.to_string());

        if let Some(output_schema) = Self::create_output_schema(function) {
            tool = tool.with_raw_output_schema(output_schema.into());
        }

        tool
    }

    /// Create an MCP Tool from a channel config with explicit input and optional output schemas.
    pub fn channel_tool(
        tool_name: &str,
        description: Option<&str>,
        input_schema: serde_json::Value,
        output_schema: Option<serde_json::Value>,
    ) -> Tool {
        let description = description
            .map(|d| d.to_string())
            .unwrap_or_else(|| format!("Call {tool_name}"));

        let input_schema = input_schema
            .as_object()
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"}).as_object().unwrap().clone());

        let mut tool = Tool::new_with_raw(
            tool_name.to_string(),
            Some(description.into()),
            input_schema,
        )
        .with_title(tool_name.to_string());

        if let Some(schema) = output_schema.and_then(|s| s.as_object().cloned()) {
            tool = tool.with_raw_output_schema(schema.into());
        }

        tool
    }

    fn flatten_schema_if_possible(schema: &serde_json::Value) -> serde_json::Value {
        if let Some(one_of) = schema.get("oneOf").and_then(|v| v.as_array())
            && one_of.len() == 2
        {
            let mut null_count = 0;
            let mut non_null_variant = None;
            for variant in one_of {
                if variant.get("type") == Some(&json!("null")) {
                    null_count += 1;
                } else {
                    non_null_variant = Some(variant.clone());
                }
            }
            if null_count == 1
                && let Some(variant) = non_null_variant
            {
                return variant;
            }
        }
        schema.clone()
    }

    fn create_output_schema(function: &Function) -> Option<rmcp::model::JsonObject> {
        function.result().and_then(Self::output_schema_for_type)
    }

    fn output_schema_for_type(schema: &serde_json::Value) -> Option<rmcp::model::JsonObject> {
        let obj = schema.as_object()?;
        if let Some(schema_type) = obj.get("type").and_then(|t| t.as_str()) {
            match schema_type {
                "object" => {
                    // WIT record -> use object schema directly
                    Some(obj.clone())
                }
                "array" => {
                    // WIT tuple -> use "tuple" (a singular composite value).
                    // WIT list -> use a property name derived from the item
                    // type (or "items" when no type name is available).
                    let property_name = if obj.contains_key("prefixItems") {
                        "tuple".to_string()
                    } else {
                        Self::derive_array_property_name(obj)
                    };
                    let wrapped = json!({
                        "type": "object",
                        "properties": {
                            property_name.clone(): obj.clone()
                        },
                        "required": [property_name]
                    });
                    Some(wrapped.as_object().unwrap().clone())
                }
                _ => {
                    // WIT primitive -> unstructured
                    None
                }
            }
        } else if obj.get("oneOf").and_then(|a| a.as_array()).is_some() {
            // WIT result<T, E> -> unwrap T (the success arm). An error arm
            // surfaces at the MCP response level via isError + text content.
            if let Some(ok_type) = Self::extract_result_ok_type(obj) {
                return Self::output_schema_for_type(ok_type);
            }
            // WIT variant -> emit as-is. The oneOf already describes a valid
            // object shape for each variant arm.
            if Self::is_variant_oneof(obj) {
                return Some(obj.clone());
            }
            // WIT option<T> -> wrap in object with nullable property
            let wrapped = json!({
                "type": "object",
                "properties": {
                    "result": serde_json::Value::Object(obj.clone())
                },
                "required": ["result"],
                "additionalProperties": false
            });
            Some(wrapped.as_object().unwrap().clone())
        } else {
            // Other schema types -> unstructured
            None
        }
    }

    // True if `obj`'s oneOf arms all look like variant cases: each is an
    // object schema with a "type" property keyed on a const discriminator.
    // Used to distinguish a WIT variant from option/result.
    fn is_variant_oneof(obj: &serde_json::Map<String, serde_json::Value>) -> bool {
        let Some(arms) = obj.get("oneOf").and_then(|a| a.as_array()) else {
            return false;
        };
        arms.iter().all(|arm| {
            arm.get("type").and_then(|t| t.as_str()) == Some("object")
                && arm
                    .get("properties")
                    .and_then(|p| p.as_object())
                    .and_then(|p| p.get("type"))
                    .and_then(|t| t.get("const"))
                    .is_some()
        })
    }

    // If `obj` is `result<T, E>`, return the inner schema of the `ok` arm.
    // Otherwise return None.
    fn extract_result_ok_type(
        obj: &serde_json::Map<String, serde_json::Value>,
    ) -> Option<&serde_json::Value> {
        let arms = obj.get("oneOf")?.as_array()?;
        if arms.len() != 2 {
            return None;
        }
        let mut ok_inner: Option<&serde_json::Value> = None;
        let mut has_error = false;
        for arm in arms {
            let props = arm.get("properties").and_then(|p| p.as_object())?;
            if props.len() != 1 {
                return None;
            }
            let (key, value) = props.iter().next()?;
            match key.as_str() {
                "ok" => ok_inner = Some(value),
                "error" => has_error = true,
                _ => return None,
            }
        }
        if has_error { ok_inner } else { None }
    }

    fn derive_array_property_name(
        array_schema: &serde_json::Map<String, serde_json::Value>,
    ) -> String {
        // Extract the item type name from the array schema
        if let Some(items) = array_schema.get("items")
            && let Some(items_obj) = items.as_object()
            && let Some(title) = items_obj.get("title")
            && let Some(type_name) = title.as_str()
        {
            return Self::pluralize(type_name);
        }
        // Fallback to generic name if no title found
        "items".to_string()
    }

    fn pluralize(singular: &str) -> String {
        if singular.ends_with("s")
            || singular.ends_with("x")
            || singular.ends_with("z")
            || singular.ends_with("ch")
            || singular.ends_with("sh")
        {
            format!("{singular}es")
        } else if singular.ends_with("y") && singular.len() > 1 {
            let chars: Vec<char> = singular.chars().collect();
            if let Some(penultimate) = chars.get(chars.len() - 2)
                && !"aeiou".contains(*penultimate)
            {
                return format!("{}ies", &singular[..singular.len() - 1]);
            }
            format!("{singular}s")
        } else {
            format!("{singular}s")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn schema(v: Value) -> Option<rmcp::model::JsonObject> {
        McpMapper::output_schema_for_type(&v)
    }

    #[test]
    fn record_passes_through() {
        let input = json!({
            "type": "object",
            "properties": { "x": { "type": "number" } },
            "required": ["x"],
            "additionalProperties": false,
            "title": "spot"
        });
        let out = schema(input.clone()).unwrap();
        assert_eq!(out["type"], "object");
        assert_eq!(out["properties"]["x"]["type"], "number");
        assert_eq!(out["title"], "spot");
    }

    #[test]
    fn list_of_named_type_uses_plural_property() {
        let input = json!({
            "type": "array",
            "items": { "type": "object", "title": "user" }
        });
        let out = schema(input).unwrap();
        assert_eq!(out["type"], "object");
        assert!(out["properties"].get("users").is_some());
        assert_eq!(out["required"][0], "users");
    }

    #[test]
    fn list_of_unnamed_type_falls_back_to_items() {
        let input = json!({ "type": "array", "items": { "type": "string" } });
        let out = schema(input).unwrap();
        assert!(out["properties"].get("items").is_some());
        assert_eq!(out["required"][0], "items");
    }

    #[test]
    fn tuple_uses_tuple_property() {
        let input = json!({
            "type": "array",
            "prefixItems": [{"type":"string"}, {"type":"number"}],
            "minItems": 2,
            "maxItems": 2
        });
        let out = schema(input.clone()).unwrap();
        assert!(out["properties"].get("tuple").is_some());
        assert_eq!(out["properties"]["tuple"], input);
        assert_eq!(out["required"][0], "tuple");
    }

    #[test]
    fn variant_passes_through() {
        let input = json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "type": { "const": "circle" },
                        "value": { "type": "number" }
                    },
                    "required": ["type", "value"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": { "type": { "const": "square" } },
                    "required": ["type"],
                    "additionalProperties": false
                }
            ]
        });
        let out = schema(input.clone()).unwrap();
        assert_eq!(out.get("oneOf"), input.get("oneOf"));
        assert!(out.get("properties").is_none());
    }

    #[test]
    fn option_wraps_under_result() {
        let input = json!({
            "oneOf": [
                { "type": "string" },
                { "type": "null" }
            ]
        });
        let out = schema(input.clone()).unwrap();
        assert_eq!(out["type"], "object");
        assert_eq!(out["properties"]["result"], input);
        assert_eq!(out["required"][0], "result");
        assert_eq!(out["additionalProperties"], false);
    }

    #[test]
    fn result_unwraps_to_ok_primitive() {
        // result<u32, string> (ok arm is a primitive) -> unstructured
        let input = json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": { "ok": { "type": "number" } },
                    "required": ["ok"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": { "error": { "type": "string" } },
                    "required": ["error"],
                    "additionalProperties": false
                }
            ]
        });
        assert!(schema(input).is_none());
    }

    #[test]
    fn result_unwraps_to_ok_record() {
        // result<record, string> (ok arm is a record) -> use directly
        let input = json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "ok": {
                            "type": "object",
                            "properties": { "id": { "type": "string" } },
                            "required": ["id"]
                        }
                    },
                    "required": ["ok"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": { "error": { "type": "string" } },
                    "required": ["error"],
                    "additionalProperties": false
                }
            ]
        });
        let out = schema(input).unwrap();
        assert_eq!(out["type"], "object");
        assert_eq!(out["properties"]["id"]["type"], "string");
    }

    #[test]
    fn primitive_returns_none() {
        assert!(schema(json!({ "type": "string" })).is_none());
        assert!(schema(json!({ "type": "number" })).is_none());
        assert!(schema(json!({ "type": "boolean" })).is_none());
    }

    #[test]
    fn enum_returns_none() {
        // Falls under primitive.
        let input = json!({ "type": "string", "enum": ["red", "green", "blue"] });
        assert!(schema(input).is_none());
    }
}
