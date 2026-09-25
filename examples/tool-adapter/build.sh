#!/bin/bash

set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

echo "==> Building the tool-adapter..."
(cd ../../components/tools/tool-adapter && ./build.sh >/dev/null)
cp ../../components/tools/tool-adapter/lib/tool-adapter.wasm lib/
