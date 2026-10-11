#!/bin/bash

# End-to-end suite for mcp-client, mcp-tool, and mcp-toolset against the MCP
# "Everything" reference server (modelcontextprotocol/servers).
#
# Starts the server with the Streamable HTTP transport on port 3002, then lists
# and calls its tools through the `tools` CLI. `config.toml` composes the
# components built in `components/lib` (run `components/build.sh` first): one
# mcp-toolset for the whole server, and one mcp-tool for each of its tools.
# Every tool is called through both, with the same assertion.
#
# What this covers: the session lifecycle, SSE responses, every content-block
# case (including a blob resource), annotations, structured content, `_meta` on
# a request, and a server's `isError` results passing through unchanged.
#
# Not yet covered: server notifications and auth. The client sends no progress
# token, so `trigger-long-running-operation` sends no progress, and the toggles
# only schedule notifications for a session that has closed by then. Will add
# auth testing once the mcp-client is updated to support it.

set -euo pipefail

SUITE_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SUITE_DIR"

SERVER_PACKAGE="@modelcontextprotocol/server-everything@1.0.0"
PORT=3002
URL="http://localhost:$PORT/mcp"
CONFIG=config.toml

# The server's tools, each called by an mcp-tool component with the same name.
TOOLS=(
  echo get-annotated-message get-env get-resource-links get-resource-reference
  get-structured-content get-sum get-tiny-image gzip-file-as-resource
  simulate-research-query toggle-simulated-logging toggle-subscriber-updates
  trigger-long-running-operation
)

for cmd in tools npx jq curl; do
  if ! command -v "$cmd" &>/dev/null; then
    echo "Error: $cmd not found" >&2
    exit 1
  fi
done

for component in mcp-client mcp-tool mcp-toolset; do
  if [[ ! -f "../../../lib/${component}.wasm" ]]; then
    echo "Error: components/lib/${component}.wasm not found (run components/build.sh)" >&2
    exit 1
  fi
done

if curl -s -o /dev/null "$URL"; then
  echo "Error: something is already listening on port $PORT" >&2
  exit 1
fi

TMP="$(mktemp -d)"

echo "==> Starting $SERVER_PACKAGE on port $PORT..."
PORT=$PORT npx -y "$SERVER_PACKAGE" streamableHttp >"$TMP/server.log" 2>&1 &
SERVER=$!
# npx runs the server as a child process, so stop both.
trap 'pkill -P $SERVER 2>/dev/null; kill $SERVER 2>/dev/null; rm -rf "$TMP"' EXIT
for _ in $(seq 150); do
  curl -s -o /dev/null "$URL" && break
  kill -0 $SERVER 2>/dev/null || break
  sleep 0.2
done
if ! curl -s -o /dev/null "$URL"; then
  echo "Error: the server did not start on port $PORT" >&2
  cat "$TMP/server.log" >&2
  exit 1
fi

PASS=0; FAIL=0

# Call a tool, keeping its output in OUT and its stderr in $TMP/stderr.
call() { # name arguments [--meta KEY=VALUE]...
  local name="$1" arguments="$2"
  shift 2
  if OUT="$(tools call "$CONFIG" "$@" -- "$name" "$arguments" 2>"$TMP/stderr")"; then
    STATUS=0
  else
    STATUS=$?
  fi
}

# Assert that a jq expression holds for OUT.
check() { # description jq-expression
  local description="$1" expression="$2"
  if [[ $STATUS -eq 0 ]] && jq -e "$expression" <<<"$OUT" >/dev/null 2>&1; then
    echo "  PASS  $description"
    PASS=$((PASS + 1))
  else
    echo "  FAIL  $description"
    [[ $STATUS -ne 0 ]] && tail -3 "$TMP/stderr" | sed 's/^/        /'
    FAIL=$((FAIL + 1))
  fi
}

# Call a server tool through its mcp-tool and through the mcp-toolset, asserting
# the same jq expression for both.
both() { # tool arguments description jq-expression [--meta KEY=VALUE]...
  local tool="$1" arguments="$2" description="$3" expression="$4"
  shift 4
  call "$tool" "$arguments" "$@"
  check "mcp-tool:    $description" "$expression"
  call "everything_$tool" "$arguments" "$@"
  check "mcp-toolset: $description" "$expression"
}

echo
echo "==> list"
if OUT="$(tools list "$CONFIG" 2>"$TMP/stderr")"; then STATUS=0; else STATUS=$?; fi
TOOL_NAMES="$(printf '%s\n' "${TOOLS[@]}" | jq -R . | jq -sc .)"
check "every mcp-tool is listed by its component name" \
  "[.[].name] | contains($TOOL_NAMES)"
check "every server tool is listed by the mcp-toolset, prefixed" \
  "[.[].name | select(startswith(\"everything_\")) | ltrimstr(\"everything_\")] | sort == ($TOOL_NAMES | sort)"
for tool in "${TOOLS[@]}"; do
  check "mcp-tool and mcp-toolset report the same metadata for $tool" \
    "(.[] | select(.name == \"$tool\") | del(.name)) == (.[] | select(.name == \"everything_$tool\") | del(.name))"
done
check "an output schema is passed through" \
  '.[] | select(.name == "get-structured-content") | ."output-schema" != null'

echo
echo "==> call"
both echo '{"message":"hi"}' "echo returns its text" \
  '.content[0].value.text == "Echo: hi"'
both get-annotated-message '{"messageType":"error","includeImage":true}' \
  "content annotations are passed through" \
  '[.content[].type] == ["text", "image"] and .content[0].value.annotations.audience == ["user", "assistant"] and .content[1].value.annotations.priority == 0.5'
both get-env '{}' "the server environment is returned" \
  '."is-error" == false and (.content[0].value.text | length) > 0'
both get-resource-links '{"count":2}' "resource links are returned" \
  '[.content[].type | select(. == "resource-link")] | length == 2'
both get-resource-reference '{"resourceType":"Text","resourceId":1}' \
  "an embedded text resource is returned" \
  '[.content[].type] == ["text", "resource", "text"] and .content[1].value."resource-data".type == "text"'
both get-structured-content '{"location":"New York"}' "structured content is returned" \
  '."structured-content" | fromjson | has("temperature")'
both get-sum '{"a":2,"b":3}' "get-sum returns the sum" \
  '.content[0].value.text == "The sum of 2 and 3 is 5."'
both get-tiny-image '{}' "an image is returned" \
  '[.content[] | select(.type == "image") | .value."mime-type"] == ["image/png"]'
both gzip-file-as-resource \
  '{"data":"data:text/plain;base64,aGVsbG8=","name":"hello.gz","outputType":"resource"}' \
  "an embedded blob resource is returned" \
  '.content[0].type == "resource" and .content[0].value."resource-data".type == "blob" and .content[0].value."resource-data".value."mime-type" == "application/gzip"'
both gzip-file-as-resource \
  '{"data":"data:text/plain;base64,aGVsbG8=","name":"hello.gz","outputType":"resourceLink"}' \
  "a resource link to the gzipped file is returned" \
  '.content[0].type == "resource-link" and .content[0].value.uri == "demo://resource/session/hello.gz"'
both simulate-research-query '{"topic":"wasm"}' \
  "the server's isError result is a task-required tool" \
  '."is-error" == true and (.content[0].value.text | contains("requires task augmentation"))'
both toggle-simulated-logging '{}' "simulated logging starts" \
  '."is-error" == false and (.content[0].value.text | startswith("Started simulated, random-leveled logging"))'
both toggle-subscriber-updates '{}' "subscriber updates start" \
  '."is-error" == false and (.content[0].value.text | startswith("Started simulated resource updated notifications"))'
both trigger-long-running-operation '{"duration":1,"steps":2}' \
  "a long-running operation completes" \
  '.content[0].value.text == "Long running operation completed. Duration: 1 seconds, Steps: 2."'

echo
echo "==> meta and errors"
both echo '{"message":"hi"}' "a call with meta succeeds" '."is-error" == false' \
  --meta traceparent=00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01
both echo '{}' "the server's isError result is invalid arguments" \
  '."is-error" == true and (.content[0].value.text | contains("Input validation error"))'
call everything_missing '{}'
check "mcp-toolset: the server's isError result is unknown tool" \
  '."is-error" == true and (.content[0].value.text | contains("-32602"))'

call missing '{}'
if [[ $STATUS -ne 0 ]] && grep -qF '"code":-32602' "$TMP/stderr"; then
  echo "  PASS  an unknown name is a tool-error"
  PASS=$((PASS + 1))
else
  echo "  FAIL  an unknown name is a tool-error"
  FAIL=$((FAIL + 1))
fi

echo
echo "$PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
