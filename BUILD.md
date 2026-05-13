# Build Guide

This document covers prerequisites, platform-specific build steps, sidecar binaries, and release notes for Aria2 Download Manager.

## Shared Notes

- App name: Aria2 Download Manager
- Identifier: com.aria2.manager
- Rust binary: aria2-manager
- Tauri config: src-tauri/tauri.conf.json
- macOS config merge: src-tauri/tauri.macos.conf.json
- Linux config merge: src-tauri/tauri.linux.conf.json
- Sidecar binaries live under src-tauri/bin and use Tauri externalBin naming.

## Sidecar Binaries (aria2)

Required sidecar names by target:

```text
src-tauri/bin/aria2c-x86_64-pc-windows-msvc.exe
src-tauri/bin/aria2c-x86_64-apple-darwin
src-tauri/bin/aria2c-aarch64-apple-darwin
src-tauri/bin/aria2c-x86_64-unknown-linux-gnu
src-tauri/bin/aria2c-aarch64-unknown-linux-gnu
```

- Windows: this repo includes src-tauri/bin/aria2c-x86_64-pc-windows-msvc.exe.
- macOS/Linux: provide the target sidecar in src-tauri/bin. The build scripts can copy a system-installed aria2c into place.

## Prerequisites (All Platforms)

- Rust: https://rustup.rs
- Node.js LTS: https://nodejs.org
The Tauri CLI is installed through npm from the project lockfile.

## Windows

### Build

```powershell
npm install
npm run tauri -- build
```

The Windows helper script regenerates icons, removes stale build artifacts, builds Tauri bundles, and verifies the Windows icon:

```powershell
npm run build:windows
```

Artifacts:

```text
src-tauri/target/release/bundle/nsis/*.exe
src-tauri/target/release/bundle/msi/*.msi
```

## macOS

### Prerequisites

```bash
xcode-select --install
brew install node aria2 imagemagick
```

Universal build setup:

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
```

### Build

```bash
npm install
./scripts/build-macos.sh
```

Artifacts:

```text
src-tauri/target/universal-apple-darwin/release/bundle/macos/Aria2 Download Manager.app
src-tauri/target/universal-apple-darwin/release/bundle/dmg/*.dmg
```

## Linux

### Prerequisites (Ubuntu example)

```bash
sudo apt update
sudo apt install -y \
  build-essential curl wget file libssl-dev \
  libgtk-3-dev libwebkit2gtk-4.1-dev \
  libayatana-appindicator3-dev librsvg2-dev \
  pkg-config libxdo-dev patchelf libfuse2 rpm aria2
```

### Build

```bash
npm install
./scripts/build-linux.sh
```

Artifacts:

```text
src-tauri/target/release/bundle/appimage/*.AppImage
src-tauri/target/release/bundle/deb/*.deb
src-tauri/target/release/bundle/rpm/*.rpm
```

## Release Steps

1. Update versions in package.json, src-tauri/Cargo.toml, and src-tauri/tauri.conf.json.
2. Build locally for the target platform.
3. Tag a release (vX.Y.Z) to trigger the GitHub Actions release workflow.

## Troubleshooting

- Missing aria2 sidecar: place the correct binary in src-tauri/bin for your target.
- macOS icon generation fails: ensure ImageMagick (magick) is installed.
- Linux rpm build fails: install rpmbuild or set TAURI_BUNDLES=appimage,deb.
- WebView2 not found on Windows: install the WebView2 runtime from Microsoft.
