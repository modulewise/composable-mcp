//! Conversion between `composable:tools` values and the rmcp model, for
//! components that export `composable:tools/tool`. The component's
//! `metadata()` reports its input and output schemas, and `call()` invokes a
//! function and returns a result.

use std::sync::Arc;

use rmcp::model::{
    AnnotateAble, Annotations, CallToolResult, Content, Meta, RawContent, RawEmbeddedResource,
    RawImageContent, RawTextContent, ResourceContents, Role, Tool, ToolAnnotations,
};
use serde_json::Value;

/// The interface a component exports to be hosted as a tool. Matching
/// includes the version separator, since `tool` is a prefix of `toolset`.
pub const TOOL_EXPORT: &str = "composable:tools/tool@";

/// The function key for `metadata` on that interface.
pub const METADATA_FUNCTION: &str = "tool.metadata";

/// The function key for `call`.
pub const CALL_FUNCTION: &str = "tool.call";

/// Whether a component contributes a tool through `composable:tools/tool`.
pub fn exports_tool(component: &composable_runtime::Component) -> bool {
    component
        .metadata
        .exports
        .iter()
        .any(|export| export.starts_with(TOOL_EXPORT))
}

/// Build an rmcp `Tool` from `tool.metadata()`.
pub fn tool_from_metadata(value: &Value, name: &str) -> Result<Tool, String> {
    let object = as_object(value, "tool-metadata")?;
    let mut tool = Tool::new_with_raw(
        name.to_string(),
        optional_string(object, "description")?.map(Into::into),
        Arc::new(input_schema(object, "input-schema")?),
    );
    tool.title = optional_string(object, "title")?;
    tool.output_schema = optional_schema(object, "output-schema")?.map(Arc::new);
    tool.annotations = optional_field(object, "annotations", tool_annotations)?;
    tool.meta = meta(object.get("meta"))?;
    Ok(tool)
}

/// Build an rmcp `CallToolResult` from what `tool.call()` returns.
pub fn call_tool_result(value: &Value) -> Result<CallToolResult, String> {
    let object = as_object(value, "call-tool-result")?;
    let content = match object.get("content") {
        Some(Value::Array(items)) => items
            .iter()
            .map(content_block)
            .collect::<Result<Vec<_>, _>>()?,
        Some(Value::Null) | None => Vec::new(),
        Some(other) => {
            return Err(format!("content must be a list, got {}", type_of(other)));
        }
    };
    let is_error = required_bool(object, "is-error")?;
    let mut result = match (optional_json(object, "structured-content")?, is_error) {
        (Some(structured), false) => CallToolResult::structured(structured),
        (Some(structured), true) => CallToolResult::structured_error(structured),
        (None, false) => CallToolResult::success(content),
        (None, true) => CallToolResult::error(content),
    };
    result.meta = meta(object.get("meta"))?;
    Ok(result)
}

/// The `_meta` entries to send a tool, as an array of `[key, value]` pairs.
pub fn meta_entries(entries: impl IntoIterator<Item = (String, String)>) -> Value {
    Value::Array(
        entries
            .into_iter()
            .map(|(key, value)| Value::Array(vec![Value::String(key), Value::String(value)]))
            .collect(),
    )
}

fn content_block(value: &Value) -> Result<Content, String> {
    let object = as_object(value, "content-block")?;
    let case = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "content-block needs a \"type\" naming its case".to_string())?;
    let payload = object
        .get("value")
        .ok_or_else(|| format!("content-block case '{case}' needs a \"value\""))?;

    let raw = match case {
        "text" => {
            let payload = as_object(payload, "text-content")?;
            RawContent::Text(RawTextContent {
                text: required_string(payload, "text")?,
                meta: meta(payload.get("meta"))?,
            })
        }
        "image" => {
            let payload = as_object(payload, "image-content")?;
            RawContent::Image(RawImageContent {
                data: required_string(payload, "data")?,
                mime_type: required_string(payload, "mime-type")?,
                meta: meta(payload.get("meta"))?,
            })
        }
        "audio" => {
            let payload = as_object(payload, "audio-content")?;
            // The 3.x upgrade will allow this.
            if meta(payload.get("meta"))?.is_some() {
                tracing::warn!("dropping _meta on audio content (unsupported by rmcp 1.x)");
            }
            RawContent::Audio(rmcp::model::RawAudioContent {
                data: required_string(payload, "data")?,
                mime_type: required_string(payload, "mime-type")?,
            })
        }
        "resource-link" => {
            let payload = as_object(payload, "resource-link")?;
            RawContent::ResourceLink(rmcp::model::RawResource {
                uri: required_string(payload, "uri")?,
                name: optional_string(payload, "name")?.unwrap_or_default(),
                title: None,
                description: optional_string(payload, "description")?,
                mime_type: optional_string(payload, "mime-type")?,
                size: None,
                icons: None,
                meta: meta(payload.get("meta"))?,
            })
        }
        "resource" => {
            let payload = as_object(payload, "embedded-resource")?;
            RawContent::Resource(RawEmbeddedResource {
                resource: resource_contents(payload.get("resource-data"))?,
                meta: meta(payload.get("meta"))?,
            })
        }
        other => return Err(format!("unknown content-block case '{other}'")),
    };

    let annotations = optional_field(
        as_object(payload, "content-block payload")?,
        "annotations",
        annotations,
    )?;
    Ok(raw.optional_annotate(annotations))
}

fn resource_contents(value: Option<&Value>) -> Result<ResourceContents, String> {
    let value = value.ok_or_else(|| "embedded-resource needs \"resource-data\"".to_string())?;
    let object = as_object(value, "resource-contents")?;
    let case = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "resource-contents needs a \"type\" naming its case".to_string())?;
    let payload = object
        .get("value")
        .ok_or_else(|| format!("resource-contents case '{case}' needs a \"value\""))?;
    let payload = as_object(payload, "resource-contents payload")?;

    match case {
        "text" => Ok(ResourceContents::TextResourceContents {
            uri: required_string(payload, "uri")?,
            mime_type: optional_string(payload, "mime-type")?,
            text: required_string(payload, "text")?,
            meta: meta(payload.get("meta"))?,
        }),
        "blob" => Ok(ResourceContents::BlobResourceContents {
            uri: required_string(payload, "uri")?,
            mime_type: optional_string(payload, "mime-type")?,
            blob: required_string(payload, "blob")?,
            meta: meta(payload.get("meta"))?,
        }),
        other => Err(format!("unknown resource-contents case '{other}'")),
    }
}

fn annotations(value: &Value) -> Result<Annotations, String> {
    let object = as_object(value, "annotations")?;
    let audience = match object.get("audience") {
        Some(Value::Array(items)) => Some(items.iter().map(role).collect::<Result<Vec<_>, _>>()?),
        Some(Value::Null) | None => None,
        Some(other) => return Err(format!("audience must be a list, got {}", type_of(other))),
    };
    let last_modified = match optional_string(object, "last-modified")? {
        Some(text) => Some(
            text.parse()
                .map_err(|e| format!("'last-modified' is not an ISO 8601 timestamp: {e}"))?,
        ),
        None => None,
    };
    let priority = match object.get("priority").and_then(Value::as_f64) {
        Some(priority) if (0.0..=1.0).contains(&priority) => Some(priority as f32),
        Some(priority) => {
            return Err(format!(
                "'priority' must be between 0 and 1, got {priority}"
            ));
        }
        None => None,
    };

    let mut annotations = Annotations::default();
    annotations.audience = audience;
    annotations.priority = priority;
    annotations.last_modified = last_modified;
    Ok(annotations)
}

fn role(value: &Value) -> Result<Role, String> {
    match value.as_str() {
        Some("user") => Ok(Role::User),
        Some("assistant") => Ok(Role::Assistant),
        Some(other) => Err(format!("unknown role '{other}'")),
        None => Err(format!("role must be a string, got {}", type_of(value))),
    }
}

fn tool_annotations(value: &Value) -> Result<ToolAnnotations, String> {
    let object = as_object(value, "tool-annotations")?;
    let mut annotations = ToolAnnotations::default();
    annotations.title = optional_string(object, "title")?;
    annotations.read_only_hint = object.get("read-only-hint").and_then(Value::as_bool);
    annotations.destructive_hint = object.get("destructive-hint").and_then(Value::as_bool);
    annotations.idempotent_hint = object.get("idempotent-hint").and_then(Value::as_bool);
    annotations.open_world_hint = object.get("open-world-hint").and_then(Value::as_bool);
    Ok(annotations)
}

// A `list<meta-entry>` is a list of two-element arrays. Both elements are
// (currently) strings in the WIT, while a `_meta` value is any JSON.
fn meta(value: Option<&Value>) -> Result<Option<Meta>, String> {
    let items = match value {
        Some(Value::Array(items)) => items,
        Some(Value::Null) | None => return Ok(None),
        Some(other) => return Err(format!("meta must be a list, got {}", type_of(other))),
    };
    if items.is_empty() {
        return Ok(None);
    }
    let mut map = serde_json::Map::new();
    for item in items {
        let pair = item
            .as_array()
            .ok_or_else(|| format!("meta entry must be a pair, got {}", type_of(item)))?;
        let [key, value] = pair.as_slice() else {
            return Err(format!(
                "meta entry must hold exactly two elements, got {}",
                pair.len()
            ));
        };
        let key = key
            .as_str()
            .ok_or_else(|| format!("meta key must be a string, got {}", type_of(key)))?;
        let value = value
            .as_str()
            .ok_or_else(|| format!("meta value must be a string, got {}", type_of(value)))?;
        map.insert(key.to_string(), Value::String(value.to_string()));
    }
    Ok(Some(Meta(map)))
}

// A field value as a JSON Schema.
fn schema(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<serde_json::Map<String, Value>, String> {
    let text = object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("'{field}' must be a JSON string"))?;
    let parsed: Value =
        serde_json::from_str(text).map_err(|e| format!("'{field}' is not valid JSON: {e}"))?;
    match parsed {
        Value::Object(map) => Ok(map),
        other => Err(format!(
            "'{field}' must be a JSON Schema object, got {}",
            type_of(&other)
        )),
    }
}

fn input_schema(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<serde_json::Map<String, Value>, String> {
    let schema = schema(object, field)?;
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => Ok(schema),
        Some(other) => Err(format!("'{field}' must be of type object, got {other}")),
        None => Err(format!("'{field}' must declare type object")),
    }
}

fn optional_schema(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<serde_json::Map<String, Value>>, String> {
    match object.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(_) => schema(object, field).map(Some),
    }
}

fn optional_json(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<Value>, String> {
    match object.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::String(text)) => serde_json::from_str(text)
            .map(Some)
            .map_err(|e| format!("'{field}' is not valid JSON: {e}")),
        Some(other) => Err(format!(
            "'{field}' must be a JSON string, got {}",
            type_of(other)
        )),
    }
}

fn optional_field<T>(
    object: &serde_json::Map<String, Value>,
    field: &str,
    read: impl Fn(&Value) -> Result<T, String>,
) -> Result<Option<T>, String> {
    match object.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(value) => read(value).map(Some),
    }
}

fn required_bool(object: &serde_json::Map<String, Value>, field: &str) -> Result<bool, String> {
    object
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("'{field}' must be a boolean"))
}

fn required_string(object: &serde_json::Map<String, Value>, field: &str) -> Result<String, String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("'{field}' must be a string"))
}

fn optional_string(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<String>, String> {
    match object.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!(
            "'{field}' must be a string, got {}",
            type_of(other)
        )),
    }
}

fn as_object<'a>(
    value: &'a Value,
    what: &str,
) -> Result<&'a serde_json::Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{what} must be an object, got {}", type_of(value)))
}

fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // What `tool.metadata()` on the tool-adapter example returns.
    fn greeter_metadata() -> Value {
        serde_json::json!({
            "name": "greeter.greet",
            "title": null,
            "description": "Greets someone by name",
            "input-schema": "{\"type\":\"object\",\"properties\":{\"name\":{\"type\":\"string\"}},\"required\":[\"name\"]}",
            "output-schema": "{\"type\":\"string\"}",
            "annotations": null,
            "meta": []
        })
    }

    // What `tool.call()` on the tool-adapter example returns.
    fn greeter_result() -> Value {
        serde_json::json!({
            "content": [{
                "type": "text",
                "value": { "text": "\"Hello World!\"", "annotations": null, "meta": [] }
            }],
            "is-error": false,
            "structured-content": "\"Hello World!\"",
            "meta": []
        })
    }

    #[test]
    fn metadata_maps_onto_a_tool() {
        let tool = tool_from_metadata(&greeter_metadata(), "greet").unwrap();
        assert_eq!(tool.description.as_deref(), Some("Greets someone by name"));
        assert_eq!(tool.input_schema.get("type").unwrap(), "object");
        assert!(tool.input_schema.get("properties").is_some());
        assert_eq!(
            tool.output_schema.unwrap().get("type").unwrap(),
            &Value::String("string".to_string())
        );
        assert!(tool.title.is_none());
        assert!(tool.annotations.is_none());
        assert!(tool.meta.is_none());
    }

    #[test]
    fn only_the_name_is_overridden() {
        // The component's metadata reports `greeter.greet`, but the hosted
        // component definition name is used.
        let tool = tool_from_metadata(&greeter_metadata(), "greet").unwrap();
        assert_eq!(tool.name, "greet");
    }

    #[test]
    fn a_schema_must_be_a_json_object() {
        let mut value = greeter_metadata();
        value["input-schema"] = Value::String("\"just a string\"".to_string());
        let err = tool_from_metadata(&value, "greet").unwrap_err();
        assert!(
            err.contains("must be a JSON Schema object"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn an_input_schema_must_describe_an_object() {
        let mut value = greeter_metadata();
        value["input-schema"] = Value::String("{\"type\":\"string\"}".to_string());
        let err = tool_from_metadata(&value, "greet").unwrap_err();
        assert!(
            err.contains("must be of type object"),
            "unexpected error: {err}"
        );

        // A schema with no root type does not describe an object either.
        value["input-schema"] = Value::String("{\"properties\":{}}".to_string());
        let err = tool_from_metadata(&value, "greet").unwrap_err();
        assert!(
            err.contains("must declare type object"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn an_output_schema_need_not_describe_an_object() {
        // 2026-07-28 admits any JSON Schema as an output schema.
        let tool = tool_from_metadata(&greeter_metadata(), "greet").unwrap();
        assert_eq!(
            tool.output_schema.unwrap().get("type").unwrap(),
            &Value::String("string".to_string())
        );
    }

    #[test]
    fn an_absent_output_schema_stays_absent() {
        let mut value = greeter_metadata();
        value["output-schema"] = Value::Null;
        let tool = tool_from_metadata(&value, "greet").unwrap();
        assert!(tool.output_schema.is_none());
    }

    #[test]
    fn annotations_and_meta_pass_through() {
        let mut value = greeter_metadata();
        value["title"] = serde_json::json!("Greeter");
        value["annotations"] = serde_json::json!({
            "title": "Greet",
            "read-only-hint": true,
            "destructive-hint": null,
            "idempotent-hint": true,
            "open-world-hint": false
        });
        value["meta"] = serde_json::json!([["com.example/owner", "search"]]);
        let tool = tool_from_metadata(&value, "greet").unwrap();
        assert_eq!(tool.title.as_deref(), Some("Greeter"));
        let annotations = tool.annotations.unwrap();
        assert_eq!(annotations.title.as_deref(), Some("Greet"));
        assert_eq!(annotations.read_only_hint, Some(true));
        assert_eq!(annotations.destructive_hint, None);
        assert_eq!(annotations.open_world_hint, Some(false));
        assert_eq!(
            tool.meta.unwrap().0.get("com.example/owner"),
            Some(&Value::String("search".to_string()))
        );
    }

    #[test]
    fn a_result_passes_through() {
        let result = call_tool_result(&greeter_result()).unwrap();
        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            result.content[0].as_text().unwrap().text,
            "\"Hello World!\""
        );
        // `structured-content` holds JSON text, so it arrives parsed.
        assert_eq!(
            result.structured_content,
            Some(Value::String("Hello World!".to_string()))
        );
        assert!(result.meta.is_none());
    }

    #[test]
    fn an_in_band_error_passes_through() {
        let mut value = greeter_result();
        value["is-error"] = Value::Bool(true);
        value["structured-content"] = Value::Null;
        let result = call_tool_result(&value).unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(result.structured_content.is_none());
    }

    #[test]
    fn result_meta_passes_through() {
        let mut value = greeter_result();
        value["meta"] = serde_json::json!([["x-count", "3"]]);
        let result = call_tool_result(&value).unwrap();
        assert_eq!(
            result.meta.unwrap().0.get("x-count"),
            Some(&Value::String("3".to_string()))
        );
    }

    #[test]
    fn every_content_case_is_read() {
        let value = serde_json::json!({
            "content": [
                { "type": "text", "value": { "text": "t", "annotations": null, "meta": [] } },
                { "type": "image", "value": { "data": "aGk=", "mime-type": "image/png", "annotations": null, "meta": [] } },
                { "type": "audio", "value": { "data": "aGk=", "mime-type": "audio/wav", "annotations": null, "meta": [] } },
                { "type": "resource-link", "value": { "uri": "file:///a", "name": "a", "description": null, "mime-type": null, "annotations": null, "meta": [] } },
                { "type": "resource", "value": {
                    "resource-data": { "type": "text", "value": { "uri": "file:///b", "mime-type": null, "text": "b", "meta": [] } },
                    "annotations": null,
                    "meta": []
                } }
            ],
            "is-error": false,
            "structured-content": null,
            "meta": []
        });
        let result = call_tool_result(&value).unwrap();
        assert_eq!(result.content.len(), 5);
        assert_eq!(result.content[0].as_text().unwrap().text, "t");
        assert_eq!(result.content[1].as_image().unwrap().mime_type, "image/png");
        let RawContent::Audio(audio) = &result.content[2].raw else {
            panic!("expected audio content");
        };
        assert_eq!(audio.mime_type, "audio/wav");
        assert_eq!(
            result.content[3].as_resource_link().unwrap().uri,
            "file:///a"
        );
        assert!(result.content[4].as_resource().is_some());
    }

    #[test]
    fn content_annotations_are_read() {
        let value = serde_json::json!({
            "content": [{
                "type": "text",
                "value": {
                    "text": "t",
                    "annotations": {
                        "audience": ["user", "assistant"],
                        "priority": 0.5,
                        "last-modified": "2026-09-16T00:00:00Z"
                    },
                    "meta": []
                }
            }],
            "is-error": false,
            "structured-content": null,
            "meta": []
        });
        let result = call_tool_result(&value).unwrap();
        let annotations = result.content[0].annotations.as_ref().unwrap();
        assert_eq!(
            annotations.audience.as_deref(),
            Some([Role::User, Role::Assistant].as_slice())
        );
        assert_eq!(annotations.priority, Some(0.5));
        assert!(annotations.last_modified.is_some());
    }

    #[test]
    fn a_priority_out_of_range_is_an_error() {
        let with_priority = |priority: f64| {
            serde_json::json!({
                "content": [{
                    "type": "text",
                    "value": {
                        "text": "t",
                        "annotations": { "audience": null, "priority": priority, "last-modified": null },
                        "meta": []
                    }
                }],
                "is-error": false,
                "structured-content": null,
                "meta": []
            })
        };
        // The bounds are inclusive.
        assert!(call_tool_result(&with_priority(0.0)).is_ok());
        assert!(call_tool_result(&with_priority(1.0)).is_ok());

        let err = call_tool_result(&with_priority(1.5)).unwrap_err();
        assert!(err.contains("between 0 and 1"), "unexpected error: {err}");
        assert!(call_tool_result(&with_priority(-0.1)).is_err());
    }

    #[test]
    fn meta_entries_are_pairs() {
        let value = meta_entries([("traceparent".to_string(), "00-abc".to_string())]);
        assert_eq!(value, serde_json::json!([["traceparent", "00-abc"]]));
    }

    #[test]
    fn no_meta_entries_is_an_empty_list() {
        assert_eq!(meta_entries([]), serde_json::json!([]));
    }

    #[test]
    fn exports_matching_includes_the_version_separator() {
        // `composable:tools/toolset` also starts with `composable:tools/tool`.
        assert!("composable:tools/tool@0.2.0".starts_with(TOOL_EXPORT));
        assert!(!"composable:tools/toolset@0.2.0".starts_with(TOOL_EXPORT));
    }
}
