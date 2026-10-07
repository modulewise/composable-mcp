#!/bin/bash

set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

if ! command -v tools &>/dev/null; then
  echo "Error: tools CLI not found (cargo install --path crates/cli, from the repository root)" >&2
  exit 1
fi

tools call config.toml -- greeter '{"name":"World"}'
