# Calculator Example

A simple calculator component exposing add, subtract, multiply, and divide as MCP tools.

## Prerequisites

- `mcp-server` (from the repository root: `cargo install --path crates/mcp-server`)
- `wasm-tools` (`cargo install wasm-tools`)

## Run

1. Start the MCP server on port 3001:

```sh
./run.sh
```

That script also compiles the component via `wasm-tools parse calculator.wat -o calculator.wasm`.

2. Test with the example curl scripts:

```sh
../curl/initialize.sh
../curl/list_tools.sh
../curl/call_tool.sh add a=4 b=3
../curl/call_tool.sh divide a=99 b=11
```

(or use [MCP Inspector](https://github.com/modelcontextprotocol/inspector))
