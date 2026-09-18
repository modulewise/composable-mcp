# Hosted Tool Example

A component exporting `composable:tools/tool` hosted over MCP. The server calls
the component's `metadata()` and uses that information to surface an MCP Tool.
The component here uses the same composition as the `tool-adapter` example.

## Run

1. Fetch the components and build the tool-adapter:

```sh
./build.sh
```

2. Start the MCP Servers and call their tools:

```sh
./run.sh
```

When that runs, two servers start. The first, on port 3001, hosts a component
that is referenced by name within a `[server.named.tool.greet]` block. That
component is called via the scripts in the `curl` example directory:

```sh
../curl/initialize.sh http://127.0.0.1:3001/mcp
../curl/list_tools.sh
../curl/call_tool.sh greet name=World
```

The second server, on port 3002, matches the component by label. That component
is called with the same commands as above, but initializing with port 3002.
