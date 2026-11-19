#!/usr/bin/env bash
# Builds RustArc-x86_64.AppImage.
# Needs: rustup/cargo, a C++ compiler (g++), curl.
# TIP: build on an older distro (e.g. Ubuntu 22.04) so the AppImage runs on more systems.
set -euo pipefail
cd "$(dirname "$0")"

ARCH="${ARCH:-x86_64}"
export ARCH

cargo build --release

APPDIR=build/RustArc.AppDir
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/scalable/apps"

cp target/release/rustarc "$APPDIR/usr/bin/"
cp packaging/AppRun "$APPDIR/AppRun"
cp packaging/rustarc.desktop "$APPDIR/rustarc.desktop"
cp packaging/rustarc.desktop "$APPDIR/usr/share/applications/"
cp packaging/rustarc.svg "$APPDIR/rustarc.svg"
cp packaging/rustarc.svg "$APPDIR/usr/share/icons/hicolor/scalable/apps/"
chmod +x "$APPDIR/AppRun"

TOOL=build/appimagetool
if [ ! -x "$TOOL" ]; then
  mkdir -p build
  curl -L -o "$TOOL" "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-${ARCH}.AppImage"
  chmod +x "$TOOL"
fi

# --appimage-extract-and-run lets this work on machines/CI without FUSE.
"$TOOL" --appimage-extract-and-run "$APPDIR" "RustArc-${ARCH}.AppImage"
echo "Done: RustArc-${ARCH}.AppImage"
