#!/usr/bin/env bash
# Build vr-space.app: a release build of headset-visualizer wrapped in a macOS app bundle.
#
#   scripts/bundle-macos.sh            # build target/release/vr-space.app
#   scripts/bundle-macos.sh --install  # also copy it to ~/Applications
#   scripts/bundle-macos.sh --run      # install and launch the installed copy
#
# The bundle is signed so macOS can give Screen Recording permission to vr-space itself.
# Install/run requires a certificate, never an ad-hoc fallback. Set it once:
#   VR_SPACE_SIGN_IDENTITY="Apple Development: Your Name (TEAMID)" scripts/bundle-macos.sh --run
# Successful installs remember it in ~/Library/Application Support/vr-space/signing-identity.
# Build-only bundles may use ad-hoc signing; do not launch them for normal testing.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CRATE="$ROOT/apps/headset-visualizer"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
[[ "$TARGET_DIR" = /* ]] || TARGET_DIR="$ROOT/$TARGET_DIR"
APP="$TARGET_DIR/release/vr-space.app"
IDENTITY_FILE="$HOME/Library/Application Support/vr-space/signing-identity"
IDENTITY="${VR_SPACE_SIGN_IDENTITY:-}"
if [[ -z "$IDENTITY" && -f "$IDENTITY_FILE" ]]; then
  IDENTITY="$(cat "$IDENTITY_FILE")"
fi
IDENTITY="${IDENTITY:--}"
INSTALLED="$HOME/Applications/vr-space.app"
BUNDLE_ID="io.github.kitaclang.vr-space"

install=false
run=false
for arg in "$@"; do
  case "$arg" in
    --install) install=true ;;
    --run) install=true; run=true ;;
    -h|--help) sed -n '2,/^set -euo pipefail/{ /^set /d; p; }' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "Unknown option: $arg (try --help)" >&2; exit 2 ;;
  esac
done

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "vr-space.app can only be built on macOS." >&2
  exit 1
fi

if $install && [[ "$IDENTITY" == "-" ]]; then
  echo "Install requires VR_SPACE_SIGN_IDENTITY (a code-signing certificate). No ad-hoc fallback." >&2
  echo "List certificates with: security find-identity -v -p codesigning" >&2
  exit 1
fi

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$CRATE/Cargo.toml" | head -n 1)"
if [[ -z "$VERSION" ]]; then
  echo "Couldn't read the version from $CRATE/Cargo.toml" >&2
  exit 1
fi

echo "Building headset-visualizer $VERSION (release)…"
"$ROOT/scripts/cargo.sh" build --release -p headset-visualizer

echo "Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$TARGET_DIR/release/headset-visualizer" "$APP/Contents/MacOS/vr-space"
sed "s/@VERSION@/$VERSION/g" "$CRATE/macos/Info.plist" > "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"

# App icon: drawn by the app itself (no image asset), turned into an .icns by iconutil.
ICONSET="$(mktemp -d)/AppIcon.iconset"
"$APP/Contents/MacOS/vr-space" --write-iconset "$ICONSET" > /dev/null
iconutil --convert icns --output "$APP/Contents/Resources/AppIcon.icns" "$ICONSET"
rm -rf "$(dirname "$ICONSET")"
plutil -lint -s "$APP/Contents/Info.plist"

echo "Signing (${IDENTITY/#-/ad-hoc})…"
codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$APP"
codesign --verify --strict "$APP"

if $install; then
  # Quit the old process before replacing its bundle, keeping login and Dock paths stable.
  osascript -e 'if application "'"$INSTALLED"'" is running then' \
    -e 'tell application "'"$INSTALLED"'" to quit' -e 'end if'
  mkdir -p "$HOME/Applications"
  rm -rf "$INSTALLED"
  ditto "$APP" "$INSTALLED"
  codesign --verify --strict "$INSTALLED"
  mkdir -p "$(dirname "$IDENTITY_FILE")"
  printf '%s\n' "$IDENTITY" > "$IDENTITY_FILE"
  echo "Installed $INSTALLED"
fi

if $run; then
  open "$INSTALLED"
else
  echo "Built $APP (build output only; use --run to install and launch)."
fi
