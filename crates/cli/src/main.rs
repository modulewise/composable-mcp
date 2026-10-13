use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::ExitCode;
use tracing_subscriber::EnvFilter;

use composable_otel::OtelService;
use composable_runtime::{ComponentError, Runtime, Val};
use composable_tools::{HostToolset, ToolService, meta_entries};

#[derive(Parser)]
#[command(name = "tools")]
#[command(about = "List and call tools")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the metadata of every tool
    List {
        /// Component definition files (.toml) and standalone .wasm files
        #[arg(required = true)]
        definitions: Vec<PathBuf>,
    },
    /// Call a tool
    Call {
        /// Component definition files (.toml) and standalone .wasm files
        #[arg(required = true)]
        definitions: Vec<PathBuf>,

        /// Meta entries passed with the call (KEY=VALUE)
        #[arg(long = "meta", value_parser = parse_key_value)]
        meta: Vec<(String, String)>,

        /// The tool name, then its arguments as a JSON object (default {}), passed after --
        #[arg(last = true, required = true, num_args = 1..=2, value_names = ["NAME", "ARGUMENTS"])]
        call: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let (Command::List { definitions } | Command::Call { definitions, .. }) = &cli.command;

    let runtime = Runtime::builder()
        .from_paths(definitions)
        .with_service::<OtelService>()
        .with_service::<ToolService>()
        .build()
        .await?;
    runtime.start()?;

    let toolset = HostToolset::new(runtime.host(), None);
    let output = match cli.command {
        Command::List { .. } => toolset.list().await.map(|tools| {
            let fields = ["input-schema", "output-schema"];
            Value::Array(
                tools
                    .into_iter()
                    .map(|tool| decoded(tool, &fields))
                    .collect(),
            )
        }),
        Command::Call { meta, call, .. } => match request(&call, meta) {
            Ok(request) => toolset
                .call(request)
                .await
                .map(|result| decoded(result, &["structured-content"])),
            Err(e) => Err(e),
        },
    };
    runtime.shutdown().await;

    // A tool-error is printed as JSON on stdout like a result and summarized
    // on stderr. Any other error is a host failure, reported on stderr only.
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            let Some(tool_error) = tool_error(&error) else {
                return Err(error);
            };
            println!("{}", serde_json::to_string_pretty(&tool_error)?);
            eprintln!("Error: {}", summary(&tool_error));
            return Ok(ExitCode::FAILURE);
        }
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(ExitCode::SUCCESS)
}

// A call-tool-request from the values after `--` and the meta entries.
fn request(call: &[String], meta: Vec<(String, String)>) -> Result<Value> {
    let name = &call[0];
    let arguments = call.get(1).map_or("{}", String::as_str);
    match serde_json::from_str(arguments) {
        Ok(Value::Object(_)) => {}
        Ok(_) => bail!("ARGUMENTS must be a JSON object"),
        Err(e) => bail!("ARGUMENTS is not valid JSON: {e}"),
    }
    Ok(json!({
        "name": name,
        "arguments": arguments,
        "meta": meta_entries(meta),
    }))
}

// `value` with each of `fields` that holds a JSON-encoded string (a
// `composable:tools` `json`) replaced by the JSON it encodes. A field that is
// absent, null or not valid JSON remains as is.
fn decoded(mut value: Value, fields: &[&str]) -> Value {
    for field in fields {
        let parsed = match value.get(*field) {
            Some(Value::String(text)) => serde_json::from_str::<Value>(text).ok(),
            _ => None,
        };
        if let Some(json) = parsed {
            value[*field] = json;
        }
    }
    value
}

// The `tool-error` that `error` carries, with its `data` decoded, or `None`
// if `error` is not a tool-error (an integer `code` and a string `message`).
fn tool_error(error: &anyhow::Error) -> Option<Value> {
    let ComponentError {
        value: Some(Val::Json(value)),
    } = error.downcast_ref::<ComponentError>()?
    else {
        return None;
    };
    (value["code"].is_i64() && value["message"].is_string())
        .then(|| decoded(value.clone(), &["data"]))
}

// A tool-error's message and code, on one line.
fn summary(tool_error: &Value) -> String {
    let message = tool_error["message"].as_str().unwrap_or_default();
    format!("{message} (code {})", tool_error["code"])
}

fn parse_key_value(s: &str) -> Result<(String, String), String> {
    let (key, value) = s
        .split_once('=')
        .ok_or_else(|| format!("expected KEY=VALUE, got '{s}'"))?;
    Ok((key.to_string(), value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component_error(value: Value) -> anyhow::Error {
        anyhow::Error::new(ComponentError {
            value: Some(Val::Json(value)),
        })
    }

    #[test]
    fn a_listing_shows_schemas_as_json() {
        let tool = json!({
            "name": "echo",
            "input-schema": r#"{"type":"object"}"#,
            "output-schema": null,
            "meta": [["io.example/raw", r#"{"a":1}"#]],
        });
        let tool = decoded(tool, &["input-schema", "output-schema"]);
        assert_eq!(tool["input-schema"], json!({ "type": "object" }));
        assert_eq!(tool["output-schema"], Value::Null);
        // A meta entry's value is text.
        assert_eq!(tool["meta"], json!([["io.example/raw", r#"{"a":1}"#]]));
    }

    #[test]
    fn a_schema_that_is_not_json_remains_a_string() {
        let tool = decoded(json!({ "input-schema": "{not json" }), &["input-schema"]);
        assert_eq!(tool["input-schema"], json!("{not json"));
    }

    #[test]
    fn a_call_result_shows_structured_content_as_json() {
        let fields = ["structured-content"];
        let result = decoded(
            json!({ "is-error": false, "structured-content": r#"{"temperature":21}"# }),
            &fields,
        );
        assert_eq!(result["structured-content"], json!({ "temperature": 21 }));

        let result = decoded(json!({ "structured-content": null }), &fields);
        assert_eq!(result["structured-content"], Value::Null);
    }

    #[test]
    fn a_tool_error_includes_its_message_code_and_data() {
        let error = component_error(json!({
            "code": -32602,
            "message": "Unknown tool: x",
            "data": r#"{"known":["echo"]}"#,
        }));
        let tool_error = tool_error(&error).unwrap();
        assert_eq!(
            tool_error,
            json!({
                "code": -32602,
                "message": "Unknown tool: x",
                "data": { "known": ["echo"] },
            })
        );
        assert_eq!(summary(&tool_error), "Unknown tool: x (code -32602)");
    }

    #[test]
    fn a_tool_error_without_data_includes_its_message_and_code() {
        let error =
            component_error(json!({ "code": -32602, "message": "Unknown tool: x", "data": null }));
        let tool_error = tool_error(&error).unwrap();
        assert_eq!(tool_error["data"], Value::Null);
        assert_eq!(summary(&tool_error), "Unknown tool: x (code -32602)");
    }

    #[test]
    fn other_errors_are_not_tool_errors() {
        assert!(tool_error(&anyhow::anyhow!("component 'x' not found")).is_none());

        // A toolset's `list` fails with a string, not a tool-error.
        assert!(tool_error(&component_error(json!("alerts: down"))).is_none());

        let without_code = component_error(json!({ "message": "Unknown tool: x" }));
        assert!(tool_error(&without_code).is_none());
    }
}
