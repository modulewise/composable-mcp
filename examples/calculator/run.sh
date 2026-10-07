#!/bin/bash

set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

if ! command -v mcp-server &>/dev/null; then
  echo "Error: mcp-server not found (cargo install --path crates/mcp-server, from the repository root)" >&2
  exit 1
fi

if ! command -v wasm-tools &>/dev/null; then
  echo "Error: wasm-tools not found (cargo install wasm-tools)" >&2
  exit 1
fi

wasm-tools parse calculator.wat -o calculator.wasm

# Serves the calculator tools at http://localhost:3001/mcp.
exec mcp-server config.toml calculator.wasm
