# Toolset Example

A toolset that includes a local tool and a nested remote toolset.

## Prerequisites

- `tools` (from the repository root: `cargo install --path crates/cli`)
- `jq`
- The pre-built components: `../../components/build.sh`

## The Toolset

```toml
[toolset.toolset]
tools = ["greeter"]
toolsets = ["calculator"]
```

A `[toolset]` generates a component that imports each listed tool and toolset. It exports
`composable:tools/toolset` and acts as an aggregator/router.

- `greeter` is a local tool for a function of the hello component, as in [tool](../tool).
- `calculator` is a remote toolset with the tools of an MCP server, as in [mcp-toolset](../mcp-toolset).
  `run.sh` starts the server from the [calculator](../calculator) example on port 3001.

Each import is satisfied by the component with its name.

## Names

The toolset names its tools:

- a tool by its import name: `greeter`
- a toolset's tools, prefixed by its import name and `_`: e.g. `calculator_add`

`tools` lists and calls every top-level tool (those that are not yet members of a toolset).
In this config that is only "toolset".

`call` routes by the full names, including any toolset names:

- `toolset_greeter` routes through the toolset to the `greeter` tool
- `toolset_calculator_multiply` routes through the toolset to the nested `calculator` toolset's
  `multiply` tool

## Running the Example

```bash
./run.sh
```

It builds and starts the calculator server, lists the tools, then calls one local and one remote
tool:

```
==> tools list
[
  "toolset_greeter",
  "toolset_calculator_multiply",
  "toolset_calculator_subtract",
  "toolset_calculator_divide",
  "toolset_calculator_add"
]

==> tools call toolset_greeter (local)
Hello World!

==> tools call toolset_calculator_multiply (remote)
42
```
