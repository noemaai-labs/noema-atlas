#!/usr/bin/env bash
# Build a macOS .app bundle (and optionally a drag-to-install .dmg) for the
# Noema Atlas desktop GUI.
#
# Usage:
#   scripts/bundle-macos.sh [output-dir]              # host-arch build
#   scripts/bundle-macos.sh --universal [output-dir]  # fat arm64 + x86_64 build
#   scripts/bundle-macos.sh --dmg [output-dir]        # also build a .dmg
#
# Env toggles (equivalent to the flags):
#   UNIVERSAL=1   build a universal2 (arm64 + x86_64) binary
#   DMG=1         also emit <output-dir>/Noema-Atlas-macos[-universal].dmg
#   ZIP=1         also emit <output-dir>/Noema-Atlas-macos[-universal].zip
#
# Code signing & notarization (all optional — degrades to ad-hoc if unset):
#   SIGN_IDENTITY   a "Developer ID Application: … (TEAMID)" identity. When set,
#                   the app is signed with the hardened runtime + a secure
#                   timestamp (the prerequisite for notarization). When unset,
#                   the app is ad-hoc signed so it still launches locally.
#   NOTARY_KEYCHAIN_PROFILE   a `notarytool store-credentials` profile name; OR
#   NOTARY_APPLE_ID + NOTARY_PASSWORD + NOTARY_TEAM_ID   an app-specific password.
#                   When a .dmg is built and notary creds are present, the DMG is
#                   submitted to Apple and the ticket stapled, so Gatekeeper lets
#                   it open with no right-click dance.
#
# Without a Developer ID + notarization the app is ad-hoc signed; on first launch
# users may need to right-click → Open (or run
# `xattr -dr com.apple.quarantine "Noema Atlas.app"`).
set -euo pipefail

cd "$(dirname "$0")/.."

UNIVERSAL="${UNIVERSAL:-0}"
ZIP="${ZIP:-0}"
DMG="${DMG:-0}"
POSITIONAL=()
for arg in "$@"; do
  case "$arg" in
    --universal) UNIVERSAL=1 ;;
    --zip) ZIP=1 ;;
    --dmg) DMG=1 ;;
    *) POSITIONAL+=("$arg") ;;
  esac
done
OUT_DIR="${POSITIONAL[0]:-dist}"
APP_NAME="Noema Atlas"
BIN="noema-desktop"
IDENT="com.noema.atlas"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"
SIGN_IDENTITY="${SIGN_IDENTITY:-}"

if [[ "$UNIVERSAL" == "1" ]]; then
  echo "==> building universal release $BIN (v$VERSION)"
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null 2>&1 || true
  cargo build --release -p "$BIN" --target aarch64-apple-darwin
  cargo build --release -p "$BIN" --target x86_64-apple-darwin
  BIN_PATH="$(mktemp -d)/$BIN"
  lipo -create -output "$BIN_PATH" \
    "target/aarch64-apple-darwin/release/$BIN" \
    "target/x86_64-apple-darwin/release/$BIN"
  SUFFIX="-universal"
else
  echo "==> building release $BIN (v$VERSION)"
  cargo build --release -p "$BIN"
  BIN_PATH="target/release/$BIN"
  SUFFIX=""
fi

APP="$OUT_DIR/$APP_NAME.app"
echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN_PATH" "$APP/Contents/MacOS/$BIN"
chmod +x "$APP/Contents/MacOS/$BIN"

# Build a proper .icns from the logo (multi-resolution) if tools are present.
ICON_KEY=""
if [[ -f assets/logo.png ]] && command -v sips >/dev/null && command -v iconutil >/dev/null; then
  echo "==> generating app icon from assets/logo.png"
  ICONSET="$(mktemp -d)/Noema.iconset"
  mkdir -p "$ICONSET"
  for s in 16 32 128 256 512; do
    sips -z $s $s assets/logo.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
    sips -z $((s*2)) $((s*2)) assets/logo.png --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
  done
  iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Noema.icns"
  ICON_KEY="  <key>CFBundleIconFile</key><string>Noema</string>"
fi

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleIdentifier</key><string>$IDENT</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleExecutable</key><string>$BIN</string>
$ICON_KEY
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>10.15</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
</dict>
</plist>
PLIST

# Code sign. With a Developer ID identity, sign for distribution (hardened
# runtime + secure timestamp); otherwise ad-hoc so it at least runs locally.
if command -v codesign >/dev/null; then
  if [[ -n "$SIGN_IDENTITY" ]]; then
    echo "==> signing with Developer ID: $SIGN_IDENTITY (hardened runtime)"
    # Sign the nested binary first, then the bundle, deep.
    codesign --force --options runtime --timestamp \
      --sign "$SIGN_IDENTITY" "$APP/Contents/MacOS/$BIN"
    codesign --force --options runtime --timestamp --deep \
      --sign "$SIGN_IDENTITY" "$APP"
    codesign --verify --strict --verbose=2 "$APP" || true
  else
    echo "==> ad-hoc code signing (no SIGN_IDENTITY set)"
    codesign --force --deep --sign - "$APP" \
      || echo "   (codesign failed; app still usable after clearing quarantine)"
  fi
fi

echo "==> done: $APP"

notarize_and_staple() {
  # $1 = path to notarize (dmg or zip). Submits + waits + staples if creds exist.
  local target="$1"
  if ! command -v xcrun >/dev/null; then return 0; fi
  if [[ -n "${NOTARY_KEYCHAIN_PROFILE:-}" ]]; then
    echo "==> notarizing $target (keychain profile)"
    xcrun notarytool submit "$target" --keychain-profile "$NOTARY_KEYCHAIN_PROFILE" --wait
    xcrun stapler staple "$target"
  elif [[ -n "${NOTARY_APPLE_ID:-}" && -n "${NOTARY_PASSWORD:-}" && -n "${NOTARY_TEAM_ID:-}" ]]; then
    echo "==> notarizing $target (apple-id)"
    xcrun notarytool submit "$target" \
      --apple-id "$NOTARY_APPLE_ID" --password "$NOTARY_PASSWORD" --team-id "$NOTARY_TEAM_ID" --wait
    xcrun stapler staple "$target"
  else
    echo "   (skipping notarization — no NOTARY_* credentials set)"
  fi
}

if [[ "$DMG" == "1" ]]; then
  DMG_PATH="$OUT_DIR/${APP_NAME// /-}-macos${SUFFIX}.dmg"
  echo "==> building dmg -> $DMG_PATH"
  rm -f "$DMG_PATH"
  STAGING="$(mktemp -d)"
  cp -R "$APP" "$STAGING/"
  ln -s /Applications "$STAGING/Applications"   # drag-to-install affordance
  hdiutil create -volname "$APP_NAME" -srcfolder "$STAGING" -ov -format UDZO "$DMG_PATH" >/dev/null
  if [[ -n "$SIGN_IDENTITY" ]] && command -v codesign >/dev/null; then
    codesign --force --sign "$SIGN_IDENTITY" "$DMG_PATH" || true
  fi
  notarize_and_staple "$DMG_PATH"
  echo "==> dmg ready: $DMG_PATH"
fi

if [[ "$ZIP" == "1" ]]; then
  ZIP_PATH="$OUT_DIR/${APP_NAME// /-}-macos${SUFFIX}.zip"
  echo "==> zipping -> $ZIP_PATH"
  rm -f "$ZIP_PATH"
  # ditto preserves the bundle structure, symlinks, and extended attributes.
  ditto -c -k --keepParent "$APP" "$ZIP_PATH"
  notarize_and_staple "$ZIP_PATH"
  echo "==> zip ready: $ZIP_PATH"
fi
