#!/bin/bash

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

if ! command -v composable &>/dev/null; then
  echo "Error: composable CLI not found (cargo install composable-runtime)"
  exit 1
fi

# The mcp-tool is configured to call multiply on a server at localhost:3001/mcp.
DEFAULT_ARGS='{"a":6,"b":7}'
ARGS="${1:-$DEFAULT_ARGS}"
DEFAULT_META='[]'
META="${2:-$DEFAULT_META}"

echo "Calling mcp-tool with arguments: ${ARGS}"

composable invoke config.toml -- mcp-tool.tool.call "$ARGS" "$META" | jq .
