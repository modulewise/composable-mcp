//! Tools and toolsets as components, for any host: an agent's runtime, an MCP
//! server, or a CLI. `ToolService` contributes the `[tool]` and `[toolset]`
//! config categories, which expand into the components that implement them.

mod config;
mod service;
mod tools;

pub use service::ToolService;
pub use tools::{
    HostToolset, TOOL_CALL, TOOL_EXPORT, TOOL_METADATA, TOOLSET_CALL, TOOLSET_EXPORT, TOOLSET_LIST,
    exports_tool, exports_toolset, meta_entries,
};
