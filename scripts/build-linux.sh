#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "Linux bundles must be built on Linux." >&2
  exit 1
fi

BUNDLES="${TAURI_BUNDLES:-appimage,deb,rpm}"
HOST_TRIPLE="$(rustc -Vv | awk '/host:/ { print $2 }')"

echo "Building Linux bundles: $BUNDLES"
echo "Rust host triple: $HOST_TRIPLE"

for tool in cargo npm rustc; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "Missing required tool: $tool" >&2
    exit 1
  fi
done

if ! command -v aria2c >/dev/null 2>&1; then
  echo "Missing system aria2c. Install aria2 before building Linux packages." >&2
  exit 1
fi
aria2c --version | head -n 1

if [[ "$BUNDLES" == *rpm* ]] && ! command -v rpmbuild >/dev/null 2>&1; then
  echo "rpm packaging requested but rpmbuild is missing. Install rpm/rpmbuild or set TAURI_BUNDLES=appimage,deb." >&2
  exit 1
fi

echo "Compiling Rust release binary..."
cargo build --release --manifest-path "$ROOT_DIR/src-tauri/Cargo.toml"

echo "Building Tauri Linux bundles..."
npm run tauri -- build --bundles "$BUNDLES"

if [[ -f "$ROOT_DIR/src-tauri/target/release/aria2-manager" ]]; then
  timeout 10s "$ROOT_DIR/src-tauri/target/release/aria2-manager" --version || true
fi

if find "$ROOT_DIR/src-tauri/target/release/bundle" -type f -name 'aria2c*' | grep -q .; then
  echo "Linux bundle unexpectedly contains an aria2c sidecar." >&2
  find "$ROOT_DIR/src-tauri/target/release/bundle" -type f -name 'aria2c*' >&2
  exit 1
fi

echo "Linux bundles:"
echo "  src-tauri/target/release/bundle/appimage/*.AppImage"
echo "  src-tauri/target/release/bundle/deb/*.deb"
echo "  src-tauri/target/release/bundle/rpm/*.rpm"
