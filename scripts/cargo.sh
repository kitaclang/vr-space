#!/usr/bin/env bash
# Cargo with the repository's private, locally applied ar-drivers patch.
# Usage: scripts/cargo.sh build --workspace (or test, run, etc.; test-driver for driver tests)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PATCH="$ROOT/patches/ar-drivers-fallible-update.patch"
REV="$(sed -n 's/^ar-drivers = .*rev = "\([^"]*\)".*/\1/p' "$ROOT/apps/headset-visualizer/Cargo.toml")"
if [[ -z "$REV" ]]; then
  echo "Couldn't read the pinned ar-drivers revision." >&2
  exit 1
fi
HASH="$(shasum -a 256 "$PATCH" | cut -d ' ' -f 1)"
CACHE="$ROOT/target/patched-drivers"
DRIVER="$CACHE/$REV-$HASH"

if [[ ! -d "$DRIVER" ]]; then
  mkdir -p "$CACHE"
  TEMP="$(mktemp -d "$CACHE/.prepare.XXXXXX")"
  trap 'rm -rf "$TEMP"' EXIT
  echo "Preparing patched ar-drivers ($REV)…" >&2
  git init -q "$TEMP"
  git -C "$TEMP" fetch -q --depth 1 https://github.com/3rl-io/headset-utils.git "$REV"
  git -C "$TEMP" checkout -q --detach FETCH_HEAD
  git -C "$TEMP" apply "$PATCH"
  # Never expose an incomplete checkout to a later invocation.
  mv "$TEMP" "$DRIVER"
fi

# Fail rather than silently building an unpatched or mismatched cached driver.
[[ "$(git -C "$DRIVER" rev-parse HEAD)" == "$REV" ]]
git -C "$DRIVER" apply --reverse --check "$PATCH"

cd "$ROOT"
# The dependency is not a workspace member, so test its patched fusion separately.
if [[ "${1:-}" == "test-driver" ]]; then
  shift
  export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
  exec cargo test --manifest-path "$DRIVER/Cargo.toml" --lib "$@"
fi
# Escape the absolute path for Cargo's TOML --config value.
DRIVER="${DRIVER//\\/\\\\}"
DRIVER="${DRIVER//\"/\\\"}"
exec cargo --config "patch.\"https://github.com/3rl-io/headset-utils\".ar-drivers.path=\"$DRIVER\"" "$@"
