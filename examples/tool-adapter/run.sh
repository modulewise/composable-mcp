#!/bin/bash

set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

composable invoke config.toml -- greeter-tool.tool.call '{"name":"World"}' | jq 'fromjson'
