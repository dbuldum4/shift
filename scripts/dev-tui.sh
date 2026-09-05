#!/bin/sh
# Build the CLI engine without the native GPUI app and run the OpenTUI client.
set -eu

root="$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
engine="$root/target/debug/shift-cli"

if ! command -v cargo >/dev/null 2>&1; then
  echo "dev-tui: cargo is not on PATH" >&2
  exit 1
fi
if ! command -v bun >/dev/null 2>&1; then
  echo "dev-tui: bun is not on PATH (need 1.3.14)" >&2
  exit 1
fi

if [ ! -d "$root/tui/node_modules" ]; then
  if ! command -v npm >/dev/null 2>&1; then
    echo "dev-tui: tui/node_modules is missing and npm is not on PATH" >&2
    exit 1
  fi
  (cd "$root/tui" && npm ci --ignore-scripts)
fi

cargo build --manifest-path "$root/Cargo.toml" --bin shift-cli --no-default-features

if [ ! -x "$engine" ]; then
  echo "dev-tui: expected engine at $engine" >&2
  exit 1
fi

export SHIFT_CLI_ENGINE="$engine"
exec bun run --cwd "$root/tui" dev "$@"
