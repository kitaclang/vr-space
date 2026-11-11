#!/usr/bin/env bash
# Build vr-space.app: a release build of headset-visualizer wrapped in a macOS app bundle.
#
#   scripts/bundle-macos.sh            # build target/release/vr-space.app
#   scripts/bundle-macos.sh --install  # also copy it to ~/Applications
#
# The bundle is signed so macOS can give Screen Recording permission to vr-space itself.
# By default it's an ad-hoc signature, which changes with every build, so macOS forgets the
# permission after a rebuild. To keep it, sign with a certificate from your keychain:
#
#   VR_SPACE_SIGN_IDENTITY="Apple Development: Your Name (TEAMID)" scripts/bundle-macos.sh
#
# (`security find-identity -v -p codesigning` lists the ones you have.)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CRATE="$ROOT/apps/headset-visualizer"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
[[ "$TARGET_DIR" = /* ]] || TARGET_DIR="$ROOT/$TARGET_DIR"
APP="$TARGET_DIR/release/vr-space.app"
IDENTITY="${VR_SPACE_SIGN_IDENTITY:--}"
BUNDLE_ID="io.github.kitaclang.vr-space"

install=false
for arg in "$@"; do
  case "$arg" in
    --install) install=true ;;
    -h|--help) sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "Unknown option: $arg (try --help)" >&2; exit 2 ;;
  esac
done

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "vr-space.app can only be built on macOS." >&2
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
  mkdir -p "$HOME/Applications"
  rm -rf "$HOME/Applications/vr-space.app"
  ditto "$APP" "$HOME/Applications/vr-space.app"
  echo "Installed $HOME/Applications/vr-space.app"
fi

echo "Done: $APP"
