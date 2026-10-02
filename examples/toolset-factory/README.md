# Toolset Example

A toolset generated over a local tool and a remote toolset, with one `list` across both and one
`call` that routes to either.

## Prerequisites

- [`composable`](https://github.com/modulewise/composable-runtime)
  (`cargo install --git https://github.com/modulewise/composable-runtime --branch main --locked composable-runtime`)
- `jq`
- The components pre-built: `../../components/build.sh`

## The Toolset

```toml
[component.toolset]
uri = "factory:toolset-factory"
imports = ["greeter", "calculator"]

[component.toolset-factory]
uri = "../../components/lib/toolset-factory.wasm"
config.tools = ["greeter"]
config.toolsets = ["calculator"]
```

The toolset-factory generates a component that imports each specified `tool` and `toolset`.
It exports `composable:tools/toolset` and acts as an aggregator/router.

- `greeter` is a local tool: a function over the hello component, adapted to a tool, as in
  [tool-adapter](../tool-adapter).
- `calculator` is a remote toolset: [mcp-toolset](../mcp-toolset) over an MCP server, here the
  [calculator](../calculator) example, which `run.sh` starts on port 3001.

Each import is satisfied by the component with its name.

## Names

`list` returns metadata for every aggregated tool:

- a tool by its import name: `greeter`
- a toolset's tools, prefixed by its import name and `_`: e.g. `calculator_add`

`call` routes on those names:

- `greeter` to the tool
- `calculator_multiply` to the nested toolset as `multiply`

## Running the Example

```bash
./run.sh
```

It builds and starts the calculator server, lists the toolset, then calls one local and one
remote tool:

```
==> toolset.list
[
  "greeter",
  "calculator_add",
  "calculator_divide",
  "calculator_multiply",
  "calculator_subtract"
]

==> toolset.call greeter (local)
Hello World!

==> toolset.call calculator_multiply (remote)
42
```
