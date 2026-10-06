use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

use composable_mcp_server::McpService;
use composable_otel::OtelService;
use composable_runtime::Runtime;
use composable_tools::ToolService;

#[derive(Parser)]
#[command(name = "mcp-server")]
#[command(about = "An MCP server for Wasm components")]
struct Cli {
    /// Component definition files (.toml) and standalone .wasm files
    #[arg(help = "Component definition files (.toml) and standalone .wasm files")]
    definitions: Vec<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let runtime = Runtime::builder()
        .from_paths(&cli.definitions)
        .with_service::<OtelService>()
        .with_service::<ToolService>()
        .with_service::<McpService>()
        .build()
        .await?;

    runtime.run().await
}
