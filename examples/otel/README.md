# OpenTelemetry Example

Demonstrates server-level tracing with OTLP span export.

Spans are emitted for `tools/list` and `tools/call` requests. The `initialize` request and lifecycle notifications do not emit spans.

## Prerequisites

- Docker (for Jaeger)
- `wasm-tools` (`cargo install wasm-tools`) to compile WAT to WASM

## Run

1. Start Jaeger:

```sh
docker compose up -d
```

2. Compile the example component:

```sh
wasm-tools parse add-two.wat -o add-two.wasm
```

3. Start the MCP server:

```sh
cargo run -p composable-mcp-server -- config.toml add-two.wasm
```

4. Initialize a session, list tools, and call a tool using the curl scripts:

```sh
../curl/initialize.sh
../curl/list_tools.sh
../curl/call_tool.sh add-two.add-two x=5
```

5. View traces at http://localhost:16686 with service `mcp`.

## With trace propagation

Per the MCP spec, trace context propagates via `_meta` in the request params, not via HTTP headers. Include `traceparent` (and optionally `tracestate`) inside `params._meta`:

```sh
curl -X POST http://localhost:3001/mcp \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -H "MCP-Session-Id: $SESSION_ID" \
  -d '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"add-two.add-two","arguments":{"x":5},"_meta":{"traceparent":"00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"}}}'
```

The `tools/call add-two.add-two` span should appear as a child of trace `0af7651916cd43dd8448eb211c80319c`.
