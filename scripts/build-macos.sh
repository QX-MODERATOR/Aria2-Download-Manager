#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "macOS bundles must be built on macOS." >&2
  exit 1
fi

TARGET="${TAURI_TARGET:-universal-apple-darwin}"
BUNDLES="${TAURI_BUNDLES:-app,dmg}"
BIN_DIR="$ROOT_DIR/src-tauri/bin"
ICONSET_DIR="$ROOT_DIR/src-tauri/icons/icon.iconset"
ICNS_OUT="$ROOT_DIR/src-tauri/icons/icon.icns"

mkdir -p "$BIN_DIR"

ensure_tool() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Missing required tool: $1" >&2
    exit 1
  fi
}

ensure_tool cargo
ensure_tool npm
ensure_tool rustup
ensure_tool iconutil
ensure_tool sips
ensure_tool magick

copy_system_aria2_for_target() {
  local triple="$1"
  local arch="$2"
  local dest="$BIN_DIR/aria2c-$triple"

  if [[ -f "$dest" ]]; then
    chmod +x "$dest"
    return
  fi

  if command -v aria2c >/dev/null 2>&1; then
    local source
    source="$(command -v aria2c)"
    if lipo -info "$source" 2>/dev/null | grep -Eq "($arch|Non-fat file: .* is architecture: $arch)"; then
      cp "$source" "$dest"
      chmod +x "$dest"
      return
    fi
  fi

  echo "Missing native aria2 sidecar: src-tauri/bin/aria2c-$triple" >&2
  echo "Install aria2 for $arch and copy the executable to that path, or set TAURI_TARGET to a target you have." >&2
  exit 1
}

case "$TARGET" in
  universal-apple-darwin)
    rustup target add aarch64-apple-darwin x86_64-apple-darwin
    copy_system_aria2_for_target "aarch64-apple-darwin" "arm64"
    copy_system_aria2_for_target "x86_64-apple-darwin" "x86_64"
    ;;
  aarch64-apple-darwin)
    rustup target add aarch64-apple-darwin
    copy_system_aria2_for_target "aarch64-apple-darwin" "arm64"
    ;;
  x86_64-apple-darwin)
    rustup target add x86_64-apple-darwin
    copy_system_aria2_for_target "x86_64-apple-darwin" "x86_64"
    ;;
  *)
    echo "Unsupported macOS target: $TARGET" >&2
    exit 1
    ;;
esac

rm -rf "$ICONSET_DIR"
mkdir -p "$ICONSET_DIR"
cp "$ROOT_DIR/src-tauri/icons/16x16.png" "$ICONSET_DIR/icon_16x16.png"
cp "$ROOT_DIR/src-tauri/icons/32x32.png" "$ICONSET_DIR/icon_16x16@2x.png"
cp "$ROOT_DIR/src-tauri/icons/32x32.png" "$ICONSET_DIR/icon_32x32.png"
cp "$ROOT_DIR/src-tauri/icons/64x64.png" "$ICONSET_DIR/icon_32x32@2x.png"
cp "$ROOT_DIR/src-tauri/icons/128x128.png" "$ICONSET_DIR/icon_128x128.png"
cp "$ROOT_DIR/src-tauri/icons/128x128@2x.png" "$ICONSET_DIR/icon_128x128@2x.png"
magick -background none "$ROOT_DIR/src-tauri/icon-master.svg" -resize 256x256 -depth 8 -define png:color-type=6 "$ICONSET_DIR/icon_256x256.png"
magick -background none "$ROOT_DIR/src-tauri/icon-master.svg" -resize 512x512 -depth 8 -define png:color-type=6 "$ICONSET_DIR/icon_256x256@2x.png"
cp "$ICONSET_DIR/icon_256x256@2x.png" "$ICONSET_DIR/icon_512x512.png"
magick -background none "$ROOT_DIR/src-tauri/icon-master.svg" -resize 1024x1024 -depth 8 -define png:color-type=6 "$ICONSET_DIR/icon_512x512@2x.png"
iconutil -c icns "$ICONSET_DIR" -o "$ICNS_OUT"
rm -rf "$ICONSET_DIR"

if [[ "$TARGET" == "universal-apple-darwin" ]]; then
  cargo build --release --manifest-path "$ROOT_DIR/src-tauri/Cargo.toml" --target aarch64-apple-darwin
  cargo build --release --manifest-path "$ROOT_DIR/src-tauri/Cargo.toml" --target x86_64-apple-darwin
else
  cargo build --release --manifest-path "$ROOT_DIR/src-tauri/Cargo.toml" --target "$TARGET"
fi
npm run tauri -- build --target "$TARGET" --bundles "$BUNDLES"

echo "macOS bundles:"
echo "  src-tauri/target/$TARGET/release/bundle/macos/Aria2 Download Manager.app"
echo "  src-tauri/target/$TARGET/release/bundle/dmg/*.dmg"
