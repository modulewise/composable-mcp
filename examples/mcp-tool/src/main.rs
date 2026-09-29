use anyhow::Result;
use composable_otel::OtelService;
use composable_runtime::{Runtime, Val};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    let runtime = Runtime::builder()
        .from_path(std::path::PathBuf::from("config-otel.toml"))
        .with_service::<OtelService>()
        .build()
        .await?;

    runtime.start()?;

    let args = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r#"{"a":6,"b":7}"#.to_string());

    let result = runtime
        .host()
        .invoke(
            "mcp-tool",
            "tool.call",
            vec![Val::Json(serde_json::json!(args)), Val::Json(serde_json::json!([]))],
            None,
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("tool.call returned nothing"))?;

    println!("{}", result.into_json()?);

    // Flush the OtelService batch span processor before exiting.
    runtime.shutdown().await;

    Ok(())
}
