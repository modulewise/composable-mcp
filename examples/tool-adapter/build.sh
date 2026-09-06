#!/bin/bash

set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

if [[ ! -f lib/hello.wasm ]]; then
  wkg oci pull -o lib/greeter.wasm ghcr.io/modulewise/demo/hello:0.2.0
fi

if [[ ! -f "lib/json-mapper.wasm" ]]; then
  wkg oci pull -o lib/json-mapper.wasm ghcr.io/modulewise/component/json-mapper:0.1.0
fi

cargo run -p tool-adapter -- ./lib/greeter.wasm ./lib/greeter-tool.wasm greeter.greet "Greet someone by name"
