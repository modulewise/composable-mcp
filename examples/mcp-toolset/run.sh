#!/bin/bash

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

if ! command -v tools &>/dev/null; then
  echo "Error: tools CLI not found (cargo install --path crates/cli, from the repository root)" >&2
  exit 1
fi

# The mcp-toolset exposes an MCP server (localhost:3001/mcp) as one toolset.
if ! curl -s -o /dev/null http://localhost:3001/mcp; then
  echo "Error: no MCP server at localhost:3001 (start ../calculator/run.sh)" >&2
  exit 1
fi

# The toolset's tools are listed and called prefixed by its name.
NAME="${1:-multiply}"
DEFAULT_ARGS='{"a":6,"b":7}'
ARGS="${2:-$DEFAULT_ARGS}"

echo "Listing tools..."
tools list config.toml

echo
echo "Calling mcp-toolset_${NAME} with arguments: ${ARGS}"
tools call config.toml -- "mcp-toolset_${NAME}" "$ARGS" | jq .
