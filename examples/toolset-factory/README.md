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

The toolset-factory generates a component that imports each member under its name, a `tool` for
each of `tools` and a `toolset` for each of `toolsets`, and exports `composable:tools/toolset`:

- `greeter` is a local tool: a function over the hello component, adapted to a tool, as in
  [tool-adapter](../tool-adapter).
- `calculator` is a remote toolset: [mcp-toolset](../mcp-toolset) over an MCP server, here the
  [calculator](../calculator) example, which `run.sh` starts on port 3001.

Each import is satisfied by the component with its name.

## Names

`list` returns every member's tools:

- a tool under its import name, `greeter`, whatever name the tool itself reports
- a toolset's tools under its import name and `_`, `calculator_add` and so on

`call` routes on those names: `greeter` to the tool, and `calculator_multiply` to the toolset as
`multiply`. `_` cannot occur in an import name, so a toolset's prefix always ends at the first
`_`. An unknown name returns MCP's unknown tool error, `-32602`.

If any member fails to list its tools, `list` fails with that member's error.

## Running the Example

```bash
./run.sh
```

It builds and starts the calculator server, then lists the toolset and calls one local and one
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
