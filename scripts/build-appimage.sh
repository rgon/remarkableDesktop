#!/usr/bin/env bash
# Builds the host client into dist/remdesktop-<version>-x86_64.AppImage.
#   scripts/build-appimage.sh [--skip-build]
# Bundles the tablet binaries (TABLET_DIR holding remdesktop-armv7 and
# remdesktop-aarch64, else target/<triple>/release/remdesktop) for `install`.
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

share=$appdir/usr/share/remdesktop
mkdir -p "$share/tablet"
install -m755 packaging/install.sh "$share/install.sh"
for t in armv7:armv7-unknown-linux-gnueabihf aarch64:aarch64-unknown-linux-gnu; do
    name=remdesktop-${t%%:*}
    for src in "${TABLET_DIR:-/nonexistent}/$name" "target/${t#*:}/release/remdesktop"; do
        [ -f "$src" ] && { install -m755 "$src" "$share/tablet/$name"; continue 2; }
    done
    [ -z "${REQUIRE_TABLET:-}" ] || { echo "missing tablet binary $name" >&2; exit 1; }
    echo "warning: no tablet binary for ${t%%:*}; \`install\` will not support it" >&2
done

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
