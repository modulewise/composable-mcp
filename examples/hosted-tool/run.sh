#!/bin/bash

set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

NAME="${1:-World}"

if [[ ! -f lib/tool-adapter.wasm ]]; then
  echo "Components are missing. Run ./build.sh first."
  exit 1
fi

cargo run --quiet --manifest-path ../../Cargo.toml -- config.toml &
SERVER_PID=$!
trap 'kill "$SERVER_PID" 2>/dev/null || true' EXIT

# The tool's metadata() is called at startup, so the server is not listening
# until that has finished.
for _ in $(seq 30); do
  if curl -s -o /dev/null "http://127.0.0.1:3001/mcp"; then
    break
  fi
  sleep 1
done

echo "==> Explicit, on 3001"
../curl/initialize.sh http://127.0.0.1:3001/mcp >/dev/null
../curl/list_tools.sh | jq -c '.result.tools[] | {name, inputSchema, outputSchema}'
../curl/call_tool.sh greet "name=$NAME"

echo
echo "==> Discovered by selector, on 3002"
../curl/initialize.sh http://127.0.0.1:3002/mcp >/dev/null
../curl/list_tools.sh | jq -c '.result.tools[] | {name, inputSchema, outputSchema}'
../curl/call_tool.sh greeter-tool "name=$NAME"
