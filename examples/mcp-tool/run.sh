#!/bin/bash

# Usage: ./run.sh [--otel] [ARGUMENTS] [KEY=VALUE...]
#
# --otel uses config-otel.toml, which calls through the
# mcp-client-otel-interceptor and exports spans to an OTLP collector at
# localhost:4317. Each KEY=VALUE is passed with the call as a meta entry.

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

if ! command -v tools &>/dev/null; then
  echo "Error: tools CLI not found (cargo install --path crates/cli, from the repository root)" >&2
  exit 1
fi

CONFIG=config.toml
if [[ "${1:-}" == "--otel" ]]; then
  CONFIG=config-otel.toml
  shift
fi

# The mcp-tool is configured to call multiply on a server at localhost:3001/mcp.
if ! curl -s -o /dev/null http://localhost:3001/mcp; then
  echo "Error: no MCP server at localhost:3001 (start ../calculator/run.sh)" >&2
  exit 1
fi

DEFAULT_ARGS='{"a":6,"b":7}'
ARGS="${1:-$DEFAULT_ARGS}"
[[ $# -gt 0 ]] && shift

META=()
for entry in "$@"; do
  META+=(--meta "$entry")
done

echo "Calling mcp-tool with arguments: ${ARGS} (${CONFIG})"

tools call "$CONFIG" "${META[@]}" -- mcp-tool "$ARGS" | jq .
