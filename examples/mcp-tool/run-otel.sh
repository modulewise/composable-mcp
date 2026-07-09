#!/bin/bash

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

# Runs a host that registers OtelService and invokes the mcp-tool component via
# the mcp-client-otel-interceptor. Expects an OTLP collector at localhost:4317
# to receive the spans.
DEFAULT_ARGS='{"a":6,"b":7}'
ARGS="${1:-$DEFAULT_ARGS}"

echo "Calling mcp-tool via otel interceptor with arguments: ${ARGS}"

if command -v jq &>/dev/null; then
  cargo run --quiet -- "$ARGS" | jq 'fromjson'
else
  cargo run --quiet -- "$ARGS"
fi
