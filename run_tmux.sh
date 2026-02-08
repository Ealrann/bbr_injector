#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT_DIR"

SESSION="bbr_injector"
CONFIG_PATH="${1:-config.toml}"
WAIT_SECS="${BBR_INJECTOR_TMUX_WAIT_SECS:-5}"

if ! command -v tmux >/dev/null 2>&1; then
  echo "error: tmux not found in PATH" >&2
  exit 1
fi

if [[ ! -f "$CONFIG_PATH" ]]; then
  echo "error: config not found: $CONFIG_PATH" >&2
  echo "hint: cp config.example.toml config.toml (then edit it)" >&2
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found in PATH" >&2
  exit 1
fi

if ! command -v pnpm >/dev/null 2>&1; then
  echo "error: pnpm not found in PATH" >&2
  exit 1
fi

stop_existing_session() {
  if ! tmux has-session -t "$SESSION" 2>/dev/null; then
    return 0
  fi

  echo "injector: existing tmux session found: $SESSION; sending Ctrl+C" >&2
  tmux send-keys -t "$SESSION":0 C-c >/dev/null 2>&1 || true

  local waited_quarters=0
  local max_quarters=$((WAIT_SECS * 4))
  while tmux has-session -t "$SESSION" 2>/dev/null; do
    if (( waited_quarters >= max_quarters )); then
      break
    fi
    sleep 0.25
    waited_quarters=$((waited_quarters + 1))
  done

  if tmux has-session -t "$SESSION" 2>/dev/null; then
    echo "injector: timeout waiting for shutdown; killing tmux session: $SESSION" >&2
    tmux kill-session -t "$SESSION" >/dev/null 2>&1 || true
  fi
}

echo "injector: building UI (pnpm)" >&2
(
  cd ui
  pnpm install
  pnpm build
)

echo "injector: building binary (cargo --release)" >&2
cargo build --release

TARGET_DIR="${CARGO_TARGET_DIR:-target}"
BIN="$TARGET_DIR/release/bbr-injector"
if [[ ! -x "$BIN" ]]; then
  echo "error: build succeeded but binary not found: $BIN" >&2
  exit 1
fi

stop_existing_session

RUST_LOG_VALUE="${RUST_LOG:-info}"
printf -v RUST_LOG_Q '%q' "$RUST_LOG_VALUE"

CMD=("$BIN" --config "$CONFIG_PATH")
printf -v CMD_STR '%q ' "${CMD[@]}"

tmux new-session -d -s "$SESSION" -c "$ROOT_DIR" bash -lc "export RUST_LOG=$RUST_LOG_Q; exec $CMD_STR"

echo "started tmux session: $SESSION" >&2
echo "attach with: tmux attach -t $SESSION" >&2
