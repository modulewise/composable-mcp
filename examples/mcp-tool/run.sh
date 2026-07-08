#!/bin/bash

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

if ! command -v composable &>/dev/null; then
  echo "Error: composable CLI not found (cargo install composable-runtime)"
  exit 1
fi

# The mcp-tool is configured to call calculator.multiply on an MCP server at
# localhost:3001/mcp. The args object is passed to the `tool.call` function.
DEFAULT_ARGS='{"a":6,"b":7}'
ARGS="${1:-$DEFAULT_ARGS}"

echo "Calling mcp-tool with arguments: ${ARGS}"

if command -v jq &>/dev/null; then
  composable invoke config.toml -- mcp-tool.tool.call "$ARGS" | jq 'fromjson'
else
  composable invoke config.toml -- mcp-tool.tool.call "$ARGS"
fi
