#!/bin/bash

set -e

DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

mkdir -p lib

cargo build --release --target wasm32-unknown-unknown -p tool-adapter

wasm-tools component new \
  target/wasm32-unknown-unknown/release/tool_adapter.wasm \
  -o lib/tool-adapter.wasm
