#!/bin/bash

set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

if [[ ! -f lib/greeter.wasm ]]; then
  wkg oci pull -o lib/greeter.wasm ghcr.io/modulewise/demo/hello:0.2.0
fi

if [[ ! -f lib/function-factory.wasm ]]; then
  wkg oci pull -o lib/function-factory.wasm ghcr.io/modulewise/component/function-factory:0.4.0
fi

if [[ ! -f "lib/filesystem-loader.wasm" ]]; then
  wkg oci pull -o lib/filesystem-loader.wasm ghcr.io/modulewise/component/filesystem-loader:0.4.0
fi

if [[ ! -f "lib/json-mapper.wasm" ]]; then
  wkg oci pull -o lib/json-mapper.wasm ghcr.io/modulewise/component/json-mapper:0.4.0
fi

echo "==> Building the tool-adapter..."
(cd ../../components/tools/tool-adapter && ./build.sh >/dev/null)
cp ../../components/tools/tool-adapter/lib/tool-adapter.wasm lib/
