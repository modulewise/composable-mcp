//! Presents a `composable:runtime/function` as a tool.
//!
//! The function provides name, description, and both schemas. Config adds
//! a title, annotations, and optional overrides for the name and description.

wit_bindgen::generate!({
    path: "wit",
    world: "tool-adapter",
    generate_all,
});

use std::collections::BTreeMap;

use function_invoker::{Error, HeaderMapping, Request};

use composable::tools::types::{
    CallToolResult, ContentBlock, TextContent, ToolAnnotations, ToolError, ToolMetadata,
};
use exports::composable::tools::tool::Guest;

struct ToolAdapter;

/// A config value, or `None` when unset.
fn config(key: &str) -> Option<String> {
    wasi::config::store::get(key).ok().flatten()
}

/// A config value parsed as a bool, for the annotation hints.
fn config_flag(key: &str) -> Option<bool> {
    config(key).and_then(|v| v.parse().ok())
}

/// The `dest = source` pairs under a config prefix.
fn mapping_config(prefix: &str) -> BTreeMap<String, String> {
    let Ok(all) = wasi::config::store::get_all() else {
        return BTreeMap::new();
    };
    all.into_iter()
        .filter_map(|(key, value)| {
            let dest = key.strip_prefix(prefix)?;
            Some((dest.to_string(), value))
        })
        .collect()
}

fn header_mapping() -> HeaderMapping {
    HeaderMapping {
        request_headers: mapping_config("request-meta."),
        response_headers: mapping_config("response-meta."),
    }
}

/// A schema without the named properties. Only an object has any.
fn without_properties(schema: String, names: &[&String]) -> String {
    if names.is_empty() {
        return schema;
    }
    let Ok(serde_json::Value::Object(mut base)) = serde_json::from_str(&schema) else {
        return schema;
    };
    if let Some(serde_json::Value::Object(properties)) = base.get_mut("properties") {
        properties.retain(|name, _| !names.contains(&name));
    }
    if let Some(serde_json::Value::Array(required)) = base.get_mut("required") {
        required.retain(|name| !name.as_str().is_some_and(|n| names.iter().any(|k| *k == n)));
    }
    serde_json::Value::Object(base).to_string()
}

/// The function's input schema, excluding any params supplied from `_meta`.
fn filtered_input_schema(schema: String, mapping: &HeaderMapping) -> String {
    without_properties(schema, &mapping.request_headers.keys().collect::<Vec<_>>())
}

/// The function's output schema, excluding any fields extracted into `_meta`.
fn filtered_output_schema(schema: String, mapping: &HeaderMapping) -> String {
    without_properties(
        schema,
        &mapping.response_headers.values().collect::<Vec<_>>(),
    )
}

const DEFAULT_SPEC_VERSION: &str = "2025-06-18";

/// The first spec version whose `structuredContent` admits any JSON value.
const ANY_STRUCTURED_CONTENT: &str = "2026-07-28";

/// Whether this tool may report a non-object result as structured content.
fn structures_any_json() -> bool {
    config("spec-version")
        .as_deref()
        .unwrap_or(DEFAULT_SPEC_VERSION)
        >= ANY_STRUCTURED_CONTENT
}

/// The function's output schema, if this tool may report it. A function always
/// declares one, but for a tool it depends on the spec. Before the 2026-07-28
/// version, output schema was only to be included for a result value having
/// `type: "object"`. For recent spec versions, it will always be included.
fn reportable_output_schema(schema: String) -> Option<String> {
    if structures_any_json() {
        return Some(schema);
    }
    let root: serde_json::Value = serde_json::from_str(&schema).ok()?;
    (root.get("type").and_then(|t| t.as_str()) == Some("object")).then_some(schema)
}

/// The annotations, or `None` when config sets none of them.
fn annotations() -> Option<ToolAnnotations> {
    let annotations = ToolAnnotations {
        title: config("title"),
        read_only_hint: config_flag("read-only-hint"),
        destructive_hint: config_flag("destructive-hint"),
        idempotent_hint: config_flag("idempotent-hint"),
        open_world_hint: config_flag("open-world-hint"),
    };
    let empty = annotations.title.is_none()
        && annotations.read_only_hint.is_none()
        && annotations.destructive_hint.is_none()
        && annotations.idempotent_hint.is_none()
        && annotations.open_world_hint.is_none();
    (!empty).then_some(annotations)
}

impl Guest for ToolAdapter {
    async fn metadata() -> Result<ToolMetadata, String> {
        let function = composable::runtime::function::metadata().await?;
        let mapping = header_mapping();
        Ok(ToolMetadata {
            name: config("name").unwrap_or(function.name),
            title: config("title"),
            description: config("description").or(function.description),
            input_schema: filtered_input_schema(function.input_schema, &mapping),
            output_schema: reportable_output_schema(filtered_output_schema(
                function.output_schema,
                &mapping,
            )),
            annotations: annotations(),
            meta: Vec::new(),
        })
    }

    async fn call(
        arguments: String,
        meta: Vec<(String, String)>,
    ) -> Result<CallToolResult, ToolError> {
        // Only structured when output schema may be reported.
        let structured = composable::runtime::function::metadata()
            .await
            .ok()
            .and_then(|function| reportable_output_schema(function.output_schema))
            .is_some();

        let invoked = function_invoker::invoke(
            Request {
                body: Some(arguments),
                headers: meta,
            },
            &header_mapping(),
            |input| composable::runtime::function::call(input),
        )
        .await;

        match invoked {
            Ok(response) => Ok(CallToolResult {
                // The serialized JSON is always provided as text content.
                content: vec![text(response.body.clone())],
                is_error: false,
                structured_content: structured.then_some(response.body),
                meta: response.headers,
            }),
            // Function call failures are reported in-band.
            Err(error @ (Error::FailedCall(_) | Error::InvalidResponse(_))) => Ok(CallToolResult {
                content: vec![text(error.to_string())],
                is_error: true,
                structured_content: None,
                meta: Vec::new(),
            }),
            // Other errors are reported on this call's `Result`.
            Err(error) => Err(ToolError {
                code: -32000,
                message: error.to_string(),
                data: None,
            }),
        }
    }
}

fn text(text: String) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text,
        annotations: None,
        meta: Vec::new(),
    })
}

export!(ToolAdapter);
