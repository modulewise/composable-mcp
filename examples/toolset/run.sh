#!/bin/bash

set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

if ! command -v tools &>/dev/null; then
  echo "Error: tools CLI not found (cargo install --path crates/cli, from the repository root)" >&2
  exit 1
fi

for component in mcp-toolset mcp-client; do
  if [[ ! -f "../../components/lib/${component}.wasm" ]]; then
    echo "Error: ../../components/lib/${component}.wasm not found (run components/build.sh)" >&2
    exit 1
  fi
done

# The remote side: an MCP server with the calculator tools, on port 3001.
echo "==> Starting the calculator MCP server..."
../calculator/run.sh >/dev/null &
SERVER=$!
trap 'kill $SERVER 2>/dev/null' EXIT
for _ in $(seq 50); do
  curl -s -o /dev/null http://localhost:3001/mcp && break
  kill -0 $SERVER 2>/dev/null || break
  sleep 0.2
done
if ! curl -s -o /dev/null http://localhost:3001/mcp; then
  echo "Error: the calculator MCP server did not start on port 3001" >&2
  exit 1
fi

echo
echo "==> tools list"
tools list config.toml | jq 'map(.name)'

echo
echo "==> tools call toolset_greeter (local)"
tools call config.toml -- toolset_greeter '{"name":"World"}' \
  | jq -r '.content[0].value.text | fromjson'

echo
echo "==> tools call toolset_calculator_multiply (remote)"
tools call config.toml -- toolset_calculator_multiply '{"a":6,"b":7}' \
  | jq -r '.content[0].value.text'
