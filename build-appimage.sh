#!/usr/bin/env bash
# Builds RustArc-x86_64.AppImage.
# Needs: rustup/cargo, a C++ compiler (g++), curl.
# TIP: build on an older distro (e.g. Ubuntu 22.04) so the AppImage runs on more systems.
set -euo pipefail
cd "$(dirname "$0")"

ARCH="${ARCH:-x86_64}"
export ARCH

VERSION="$(awk -F'"' '/^version =/ { print $2; exit }' Cargo.toml)"

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
cp packaging/.DirIcon "$APPDIR/.DirIcon"
chmod +x "$APPDIR/AppRun"

# Bundle Noto Sans (the UI font) if it is installed here; the app falls back to egui's
# built-in font when it can't find it.
mkdir -p "$APPDIR/usr/share/fonts"
FONT="$(fc-match -f '%{family}|%{file}' 'Noto Sans' 2>/dev/null || true)"
FAMILY="${FONT%%|*}"; FAMILY="${FAMILY%%,*}"
if [ "$FAMILY" = "Noto Sans" ] && [ -f "${FONT#*|}" ]; then
  cp "${FONT#*|}" "$APPDIR/usr/share/fonts/NotoSans-Regular.ttf"
else
  echo "warning: Noto Sans not found on this system, not bundling it (install fonts-noto-core / noto-fonts)"
fi

TOOL=build/appimagetool
if [ ! -x "$TOOL" ]; then
  mkdir -p build
  curl -L -o "$TOOL" "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-${ARCH}.AppImage"
  chmod +x "$TOOL"
fi

# --appimage-extract-and-run lets this work on machines/CI without FUSE.
"$TOOL" --appimage-extract-and-run "$APPDIR" "RustArc-${VERSION}-linux-${ARCH}.AppImage"
echo "Done: RustArc-${VERSION}-linux-${ARCH}.AppImage"
