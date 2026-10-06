#!/bin/bash

set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

if ! command -v composable &>/dev/null; then
  echo "Error: composable CLI not found (cargo install composable-runtime)"
  exit 1
fi

for component in toolset-factory mcp-toolset mcp-client; do
  if [[ ! -f "../../components/lib/${component}.wasm" ]]; then
    echo "Error: ../../components/lib/${component}.wasm not found (run components/build.sh)"
    exit 1
  fi
done

# The remote side: an MCP server with the calculator tools, on port 3001.
echo "==> Starting the calculator MCP server..."
(cd ../.. && cargo build --release -p composable-mcp-server >/dev/null)
(cd ../calculator && exec ../../target/release/mcp-server config.toml calculator.wasm) >/dev/null 2>&1 &
SERVER=$!
trap 'kill $SERVER 2>/dev/null' EXIT
for _ in $(seq 50); do
  curl -s -o /dev/null http://localhost:3001/mcp && break
  sleep 0.2
done
if ! curl -s -o /dev/null http://localhost:3001/mcp; then
  echo "Error: the calculator MCP server did not start on port 3001"
  exit 1
fi

# A tool call request: the tool's name, its arguments as JSON text, no meta.
request() {
  printf '{"name":"%s","arguments":%s,"meta":[]}' "$1" "$(printf '%s' "$2" | jq -Rs .)"
}

echo
echo "==> toolset.list"
composable invoke config.toml -- toolset.toolset.list | jq 'map(.name)'

echo
echo "==> toolset.call greeter (local)"
composable invoke config.toml -- toolset.toolset.call "$(request greeter '{"name":"World"}')" \
  | jq -r '.content[0].value.text | fromjson'

echo
echo "==> toolset.call calculator_multiply (remote)"
composable invoke config.toml -- toolset.toolset.call "$(request calculator_multiply '{"a":6,"b":7}')" \
  | jq -r '.content[0].value.text'
