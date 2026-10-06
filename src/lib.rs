//! Tools and toolsets as components, for any host: an agent's runtime, an MCP
//! server, or a CLI. `ToolService` contributes the `[tool]` and `[toolset]`
//! config categories, which expand into the components that implement them.

mod config;
mod service;

pub use service::ToolService;
