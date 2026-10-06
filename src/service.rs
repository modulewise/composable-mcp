use composable_runtime::{ConfigHandler, Service};

use crate::config::ToolConfigHandler;

/// Contributes the `[tool]` and `[toolset]` config handlers to any host:
/// an agent's runtime, an MCP server, or a CLI.
#[derive(Default)]
pub struct ToolService;

impl Service for ToolService {
    fn config_handler(&self) -> Option<Box<dyn ConfigHandler>> {
        Some(Box::new(ToolConfigHandler::default()))
    }
}
