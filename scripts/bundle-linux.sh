#!/usr/bin/env bash
# Build a self-contained Linux AppImage for the Noema Atlas desktop GUI.
# Double-click to run; no install, no root, works across distros.
#
# Usage:
#   scripts/bundle-linux.sh [output-dir]      # default output-dir: dist
#
# Strategy:
#   * If `linuxdeploy` is on PATH (or LINUXDEPLOY points at it) it is used to
#     bundle the shared libraries the egui/glow app needs (libxkbcommon, GL, …),
#     which is the most portable result.
#   * Otherwise, if `appimagetool` is available, the AppDir is packaged as-is
#     (relies on the host's system libraries — fine for same-distro use).
#   * The release `.tar.gz` of the raw binaries is produced by the release CI
#     workflow alongside this AppImage as a fallback.
set -euo pipefail

cd "$(dirname "$0")/.."

OUT_DIR="${1:-dist}"
APP_NAME="Noema Atlas"
BIN="noema-desktop"
ARCH="$(uname -m)"   # x86_64 / aarch64
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"

echo "==> building release $BIN (v$VERSION) for $ARCH"
cargo build --release -p "$BIN"

APPDIR="$(mktemp -d)/Noema_Atlas.AppDir"
echo "==> assembling AppDir"
mkdir -p "$APPDIR/usr/bin" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/256x256/apps"

cp "target/release/$BIN" "$APPDIR/usr/bin/$BIN"
chmod +x "$APPDIR/usr/bin/$BIN"

# Desktop entry (top-level copy is required by appimagetool; the usr/share copy
# is what desktop environments index after extraction).
DESKTOP_FILE="$APPDIR/noema-atlas.desktop"
cat > "$DESKTOP_FILE" <<DESKTOP
[Desktop Entry]
Type=Application
Name=$APP_NAME
Comment=Verified multi-source downloader for local LLM weights
Exec=$BIN
Icon=noema-atlas
Categories=Development;Utility;Network;
Terminal=false
DESKTOP
cp "$DESKTOP_FILE" "$APPDIR/usr/share/applications/noema-atlas.desktop"

# Icon (top-level + hicolor). Fall back gracefully if the logo is missing.
if [[ -f assets/logo.png ]]; then
  cp assets/logo.png "$APPDIR/noema-atlas.png"
  cp assets/logo.png "$APPDIR/usr/share/icons/hicolor/256x256/apps/noema-atlas.png"
fi

# AppRun launcher.
cat > "$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
export PATH="$HERE/usr/bin:$PATH"
export LD_LIBRARY_PATH="$HERE/usr/lib:${LD_LIBRARY_PATH:-}"
exec "$HERE/usr/bin/noema-desktop" "$@"
APPRUN
chmod +x "$APPDIR/AppRun"

mkdir -p "$OUT_DIR"
OUT_PATH="$OUT_DIR/Noema-Atlas-${ARCH}.AppImage"
rm -f "$OUT_PATH"

LINUXDEPLOY="${LINUXDEPLOY:-$(command -v linuxdeploy || true)}"
if [[ -n "$LINUXDEPLOY" ]]; then
  echo "==> packaging with linuxdeploy (bundling libraries)"
  OUTPUT="$OUT_PATH" "$LINUXDEPLOY" \
    --appdir "$APPDIR" \
    --executable "$APPDIR/usr/bin/$BIN" \
    --desktop-file "$DESKTOP_FILE" \
    ${ICON:+--icon-file "$APPDIR/noema-atlas.png"} \
    --output appimage
  # linuxdeploy names the file from the desktop entry; normalize it.
  if [[ ! -f "$OUT_PATH" ]]; then
    mv "$(ls -t ./*.AppImage | head -1)" "$OUT_PATH"
  fi
elif command -v appimagetool >/dev/null; then
  echo "==> packaging with appimagetool (relies on host libraries)"
  ARCH="$ARCH" appimagetool "$APPDIR" "$OUT_PATH"
else
  echo "ERROR: neither 'linuxdeploy' nor 'appimagetool' found on PATH." >&2
  echo "Install one (e.g. download linuxdeploy-x86_64.AppImage) and re-run," >&2
  echo "or set LINUXDEPLOY=/path/to/linuxdeploy.AppImage." >&2
  exit 1
fi

chmod +x "$OUT_PATH"
echo "==> AppImage ready: $OUT_PATH"
