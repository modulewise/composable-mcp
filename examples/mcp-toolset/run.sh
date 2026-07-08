#!/bin/bash

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

if ! command -v composable &>/dev/null; then
  echo "Error: composable CLI not found (cargo install composable-runtime)"
  exit 1
fi

# The mcp-toolset exposes an MCP server (localhost:3001/mcp) as one toolset.
# `list` returns metadata for every tool and `call` dispatches by tool name.
NAME="${1:-calculator.multiply}"
DEFAULT_ARGS='{"a":6,"b":7}'
ARGS="${2:-$DEFAULT_ARGS}"

echo "Invoking toolset.list..."
composable invoke config.toml -- mcp-toolset.toolset.list

echo
echo "Invoking toolset.call: ${NAME} ${ARGS}"
if command -v jq &>/dev/null; then
  composable invoke config.toml -- mcp-toolset.toolset.call "$NAME" "$ARGS" | jq 'fromjson'
else
  composable invoke config.toml -- mcp-toolset.toolset.call "$NAME" "$ARGS"
fi
