use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

use composable_otel::OtelService;
use composable_runtime::Runtime;
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
async fn main() -> Result<()> {
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
        Command::List { .. } => toolset.list().await.map(Value::Array),
        Command::Call { meta, call, .. } => match request(&call, meta) {
            Ok(request) => toolset.call(request).await,
            Err(e) => Err(e),
        },
    };
    runtime.shutdown().await;

    println!("{}", serde_json::to_string_pretty(&output?)?);
    Ok(())
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

fn parse_key_value(s: &str) -> Result<(String, String), String> {
    let (key, value) = s
        .split_once('=')
        .ok_or_else(|| format!("expected KEY=VALUE, got '{s}'"))?;
    Ok((key.to_string(), value.to_string()))
}
