#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT_DIR"

CONFIG_PATH="${1:-config.toml}"

if [[ ! -f "$CONFIG_PATH" ]]; then
  echo "error: config not found: $CONFIG_PATH" >&2
  echo "hint: cp config.example.toml config.toml (then edit it)" >&2
  exit 1
fi

if ! command -v pnpm >/dev/null 2>&1; then
  echo "error: pnpm not found in PATH" >&2
  exit 1
fi

(
  cd ui
  pnpm install
  pnpm build
)

export RUST_LOG="${RUST_LOG:-info}"

exec cargo run -- --config "$CONFIG_PATH"

