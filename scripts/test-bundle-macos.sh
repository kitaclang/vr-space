#!/usr/bin/env bash
# Signing guard regressions; does not build, install or touch the real app/config.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEMP="$(mktemp -d)"
trap 'rm -rf "$TEMP"' EXIT
export HOME="$TEMP"
unset VR_SPACE_SIGN_IDENTITY
for mode in --install --run; do
  if "$ROOT/scripts/bundle-macos.sh" "$mode" > "$TEMP/output" 2>&1; then
    echo "Unexpected success without a signing certificate" >&2
    exit 1
  fi
  grep -q 'No ad-hoc fallback' "$TEMP/output"
done
mkdir -p "$HOME/Library/Application Support/vr-space"
printf '%s\n' '-' > "$HOME/Library/Application Support/vr-space/signing-identity"
if "$ROOT/scripts/bundle-macos.sh" --run > "$TEMP/output" 2>&1; then
  echo "Unexpected success with a saved ad-hoc identity" >&2
  exit 1
fi
grep -q 'No ad-hoc fallback' "$TEMP/output"
"$ROOT/scripts/bundle-macos.sh" --help | grep -q -- '--run'
echo 'Bundle signing guards passed.'
