#!/bin/sh

if [ ! -f lib/http-client.wasm ]; then
  echo "Fetching http-client component..."
  wkg oci pull ghcr.io/modulewise/component/http-client:0.2.0 -o lib/http-client.wasm
fi
if [ ! -f lib/wasi-logging-to-stdout.wasm ]; then
  echo "Fetching WASI logging component..."
  wkg oci pull ghcr.io/componentized/logging/to-stdout:v0.2.1 -o lib/wasi-logging-to-stdout.wasm
fi
if [ ! -f lib/stdout-to-stderr.wasm ]; then
  echo "Fetching stdout-to-stderr adapter..."
  wkg oci pull ghcr.io/componentized/cli/stdout-to-stderr:v0.1.1 -o lib/stdout-to-stderr.wasm
fi

PROJECTS=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[].name')

for project in $PROJECTS; do
  echo "Building $project..."

  target=wasm32-unknown-unknown
  cargo build -p "$project" --target $target --release

  cargo_name=$(echo "$project" | tr '-' '_')
  core_wasm="target/${target}/release/${cargo_name}.wasm"
  wasm-tools component new "$core_wasm" -o "lib/${project}.wasm"
  echo "  -> lib/${project}.wasm"
done
