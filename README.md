# Modulewise Toolbelt

A [Model Context Protocol](https://modelcontextprotocol.io/) (MCP) Server that exposes [Wasm Components](https://component-model.bytecodealliance.org) as Tools.

**This is currently an early-stage non-production prototype.**

## Build

Prerequisite: a current [rust toolchain](https://www.rust-lang.org/tools/install)

Clone the [composable-mcp](https://github.com/modulewise/composable-mcp) project if you have not already.

Then from within the `composable-mcp` directory:

```
cargo install --path .
```

That will build the binary with the `release` profile and add
it to your cargo bin directory which should be on your PATH.

## Run Simple Components

Provide the path to one or more `.wasm` files as command line arguments:

```sh
toolbelt hello.wasm calculator.wasm
```

Or you can specify OCI URIs for published Wasm Components, such as these:

```sh
toolbelt oci://ghcr.io/modulewise/demo/hello:0.2.0 \
         oci://ghcr.io/modulewise/demo/calculator:0.2.0
```

> [!TIP]
>
> If you'd like to build the Wasm Components locally, clone the
> [modulewise/demos](https://github.com/modulewise/demos) project and follow the build instructions in
> [components/README.md](https://github.com/modulewise/demos/blob/main/components/README.md)

## Run Components with Dependencies

By default, components operate in a least-privilege capability mode.
If your component requires capabilities from the host runtime, you can
specify those capabilities in a `.toml` file:

```toml
[capability.http]
type = "wasi:http"
```

And then define the tool component that imports one or more capabilities:

```toml
[component.flights]
uri = "file:///path/to/flight-search.wasm"
imports = ["http"]
```

Pass the definition file to the server instead of direct `.wasm` files:

```sh
toolbelt flights.toml
```

Wasm Components can also import other components which may have their own dependencies:

`components.toml`
```toml
[component.incrementor]
uri = "../demos/components/lib/incrementor.wasm"
imports = ["keyvalue"]

[component.incrementor.config]
bucket = "increments"

[component.keyvalue]
uri = "../demos/components/lib/valkey-client.wasm"
imports = ["wasip2"]
```

And responsibilities can be separated across multiple files:

`capabilities.toml`
```toml
[capability.wasip2]
type = "wasi:p2"
```

Now these files can be passed to the server:

```sh
toolbelt components.toml capabilities.toml
```

This allows for various combinations of host capabilities and guest components.
It also promotes responsibility-driven separation of concerns between
supporting infrastructure and domain-centric tools.

## Configure the MCP Server

When no `[server]` with `type = "mcp"` is configured, toolbelt starts a default
MCP server on `127.0.0.1:3001` that auto-discovers top-level components (those
not imported by other components).

For explicit control, add a `[server]` section with `type = "mcp"` to your
definition file.

### Discover components to expose as tools with a selector

Use `component-selector` to match components by metadata:

```toml
[server.mcp]
type = "mcp"
port = 3001
component-selector = "!dependents"
```

This exposes all top-level components as tools. Other selector expressions
are supported, for example `labels.domain = "shopping"` or `name = "greeter"`.

### Define tools explicitly

Use `[server.mcp.tool.*]` entries to map specific component functions to
named tools:

```toml
[server.mcp]
type = "mcp"
port = 3001

[server.mcp.tool.greeter]
component = "greeter"
function = "greet"
description = "Greet someone by name"

[server.mcp.tool.adder]
component = "calculator"
function = "operations.add"
```

The tool name comes from the TOML key (`greeter` and `adder` above).
The function should be qualified by interface if the specified function
is exported via interface, or directly if exported at the WIT world level.
The `description` field is optional but plays an important role in providing
instructions to a calling agent.

### Combine both

Selectors and explicit tools can be used together. Explicit tool definitions
take precedence on name collisions:

```toml
[server.mcp]
type = "mcp"
port = 3001
component-selector = "!dependents"

[server.mcp.tool.greeter]
component = "greeter"
function = " greet"
description = "Custom description for the greet tool"
```

### Channel-backed tools

A tool can publish to a messaging channel instead of invoking a component
directly. The request body is published as a Message, and whatever component
consumes from the channel (via a `[subscription.*]`) handles the request and
may publish a reply.

```toml
[server.mcp.tool.get-exchange-rate]
channel = "exchange-rate-requests"
description = "Get the current exchange rate between two currencies"
input-schema = { type = "object", properties = { from = { type = "string" }, to = { type = "string" } }, required = ["from", "to"] }
```

Channel-backed tools require an explicit `input-schema` since no WIT signature
is directly available at the tool boundary. An `output-schema` is optional.

Component-backed tools may also supply an explicit `input-schema` or
`output-schema`. When present, each must structurally align with the schema
derived from WIT and the mapping blocks. The explicit schema can layer
additional constraints or metadata on top of the derived shape.

### Mapping pipeline

A tool ultimately invokes a WIT function. Bridging between a Message
(JSON body + headers) and WIT (typed args, typed result) involves a Message
Mapper driven by four optional config blocks. Where they are defined depends on
which type of tool target:

- **Component-backed tool** (`component` + `function` on the tool definition):
  the blocks are defined on the tool definition itself.
- **Channel-backed tool** (`channel` on the tool definition): the blocks are
  defined on the downstream `[subscription.*]` that registers a consumer on the
  channel.

**Inbound** (Message -> WIT call):

1. `param-mapping`: per-arg templates that build WIT args by reading paths
   into the inbound Message (`{body.<path>}`, `{headers.<path>}`). Without an
   entry for a given WIT param, the param name is looked up as a top-level
   field on the Message body, and that field's value becomes the arg.
2. `param-encoding`: for any WIT arg typed as a byte array (`list<u8>`),
   the associated value is encoded based on a content-type, either provided as
   a literal value or via path-match against the body or headers.

**Outbound** (WIT result -> reply Message):

3. `result-decoding`: for any byte-array field on the WIT result, the value
   is decoded based on a content-type, either provided as a literal value or
   via path-match against the result. The decoded value replaces the bytes
   before `result-mapping` runs.
4. `result-mapping`: structural `body` / `headers` slots that shape the
   reply Message: `body` becomes the tool reply's `structuredContent`, and
   mapped headers are available for outbound `_meta` propagation (see
   below).

With no blocks declared, direct name-matching drives the inbound side and the
WIT result becomes the reply body verbatim.

The template path grammar (`body.foo`, `headers["x"]`, `body.items[3]`), the
two content-type forms (literal or `{path}` resolution), and the supported
content-types (`application/json`, `text/plain`) are identical across both
component and channel targets as well as the HTTP server's mapping blocks. For
a complete description, see the
[composable HTTP server README](https://github.com/modulewise/composable-runtime/blob/main/crates/http-server/README.md).

In the example below, `result-mapping.body` lifts the `forecast` field of the
WIT result to become the tool reply body. The `headers` slot lifts
`quota.remaining` to a Message header named `x-ratelimit-remaining`. Headers on
the reply Message are available for outbound `_meta` propagation. The next
section shows how `propagate-result-meta` emits a selected header as an MCP
`_meta` entry on the `CallToolResult`.

```toml
[server.mcp.tool.weather]
component = "weather-client"
function = "get-forecast"

[server.mcp.tool.weather.param-mapping]
url = "https://api.example.com/forecast?city={body.city}"

[server.mcp.tool.weather.result-mapping]
body = "{forecast}"
headers = { x-ratelimit-remaining = "{quota.remaining}" }
```

The same config can be applied on a subscription:

```toml
[subscription.forecast]
channel = "forecast-requests"
component = "weather-client"
function = "get-forecast"

[subscription.forecast.param-mapping]
url = "https://api.example.com/forecast?city={body.city}"

[subscription.forecast.result-mapping]
body = "{forecast}"
headers = { x-ratelimit-remaining = "{quota.remaining}" }
```

### Propagate MCP `_meta`

Tool definitions can lift MCP `_meta` entries from the request into inbound
Message headers, and emit Message headers as `_meta` entries on the tool result:

```toml
[server.mcp.tool.tagger]
component = "tagger"
function = "tag"
propagate-request-meta = [
    "com.example.tools/tag",
    "com.example.tools/correlation-id as correlation-id",
]
propagate-result-meta = [
    "x-ratelimit-remaining as com.example.tools/ratelimit-remaining",
]
```

Each entry has either the `_meta` key itself or can be renamed using the
"[source] as [target]" option. For `propagate-request-meta`, the source side is
the MCP `_meta` key and the target is a Message header name. For
`propagate-result-meta`, the source is a Message header name and the target is
the MCP `_meta` key.

Validation rejects entries whose MCP-side key does not conform to the MCP
`_meta` format (prefix labels + name), per the
[MCP spec](https://modelcontextprotocol.io/specification/2025-11-25/basic#_meta).

A `propagate-result-meta` target that collides with a `result-mapping.headers`
target from a different source is rejected at startup.

### Origin validation

Toolbelt validates the `Origin` header if present on requests per the MCP spec.
When binding to a loopback address, localhost origins are allowed by default.
Otherwise, all origins are denied unless explicitly configured:

```toml
[server.mcp]
type = "mcp"
port = 3001
host = "0.0.0.0"
allowed-origins = ["app.example.com", "localhost"]
```

### OpenTelemetry tracing

Add `otlp-endpoint` to export spans via OTLP:

```toml
[server.mcp]
type = "mcp"
port = 3001
otlp-endpoint = "http://localhost:4317"
otlp-protocol = "grpc"  # or "http/protobuf"; defaults to "grpc"
```

Spans are emitted for `tools/list` and `tools/call` with attributes following the [gen_ai MCP semantic conventions](https://opentelemetry.io/docs/specs/semconv/gen-ai/mcp/). Trace context propagates from client to server via `_meta` in request params and from server into Wasm component invocations via `traceparent` in the propagation context.

See [examples/otel](examples/otel) for a complete example with Jaeger.

## Test with MCP Inspector

1. Run the server as described above.

2. Start the [MCP Inspector](https://github.com/modelcontextprotocol/inspector?tab=readme-ov-file#quick-start-ui-mode).

3. Ensure the `Streamable HTTP` Transport Type is selected.

4. Ensure the specified URL is `http://127.0.0.1:3001/mcp` (replace host or port if not using defaults).

5. Click `Connect` and then `List Tools`.

6. Select a Tool, provide parameter values, and click `Run Tool`.

## License

Copyright (c) 2026 Modulewise Inc and the Modulewise Composable MCP contributors.

Apache License v2.0: see [LICENSE](./LICENSE) for details.
