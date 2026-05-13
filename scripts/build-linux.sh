#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "Linux bundles must be built on Linux." >&2
  exit 1
fi

BUNDLES="${TAURI_BUNDLES:-appimage,deb,rpm}"
BIN_DIR="$ROOT_DIR/src-tauri/bin"
HOST_TRIPLE="$(rustc -Vv | awk '/host:/ { print $2 }')"
SIDECAR="$BIN_DIR/aria2c-$HOST_TRIPLE"

mkdir -p "$BIN_DIR"

for tool in cargo npm rustc; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "Missing required tool: $tool" >&2
    exit 1
  fi
done

if [[ ! -f "$SIDECAR" ]]; then
  if command -v aria2c >/dev/null 2>&1; then
    cp "$(command -v aria2c)" "$SIDECAR"
  else
    echo "Missing aria2 sidecar: $SIDECAR" >&2
    echo "Install aria2, then rerun this script so it can copy the native executable." >&2
    exit 1
  fi
fi
chmod +x "$SIDECAR"

if [[ "$BUNDLES" == *rpm* ]] && ! command -v rpmbuild >/dev/null 2>&1; then
  echo "rpm packaging requested but rpmbuild is missing. Install rpm/rpmbuild or set TAURI_BUNDLES=appimage,deb." >&2
  exit 1
fi

cargo build --release
npm run tauri -- build --bundles "$BUNDLES"

echo "Linux bundles:"
echo "  src-tauri/target/release/bundle/appimage/*.AppImage"
echo "  src-tauri/target/release/bundle/deb/*.deb"
echo "  src-tauri/target/release/bundle/rpm/*.rpm"
