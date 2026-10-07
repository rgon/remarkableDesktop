#!/usr/bin/env bash
# Builds the host client into dist/remdesktop-<version>-x86_64.AppImage.
#   scripts/build-appimage.sh [--skip-build]
# Needs the libxkbcommon dev package (and its runtime libs) to build, and
# downloads linuxdeploy to bundle the shared libraries.
set -euo pipefail
cd "$(dirname "$0")/.."

[ "${1:-}" = --skip-build ] || cargo build --release -p remdesktop-host

version=$(sed -n 's/^version = "\(.*\)".*/\1/p' host/Cargo.toml | head -1)
arch=x86_64
tools=target/appimage-tools
appdir=target/appimage/AppDir
rm -rf "$appdir"
mkdir -p "$tools" dist

linuxdeploy=$tools/linuxdeploy-$arch.AppImage
if [ ! -x "$linuxdeploy" ]; then
    curl -fsSL -o "$linuxdeploy" \
        "https://github.com/linuxdeploy/linuxdeploy/releases/download/1-alpha-20250213-2/linuxdeploy-$arch.AppImage"
    chmod +x "$linuxdeploy"
fi

# Extract-and-run so the build needs no FUSE.
APPIMAGE_EXTRACT_AND_RUN=1 \
OUTPUT="dist/remdesktop-$version-$arch.AppImage" \
LINUXDEPLOY_OUTPUT_VERSION="$version" \
"$linuxdeploy" \
    --appdir "$appdir" \
    --executable target/release/remdesktop-host \
    --desktop-file packaging/es.rgon.remdesktop.desktop \
    --icon-file packaging/remdesktop.svg --icon-filename remdesktop \
    --custom-apprun packaging/AppRun \
    --output appimage

echo "built dist/remdesktop-$version-$arch.AppImage"
