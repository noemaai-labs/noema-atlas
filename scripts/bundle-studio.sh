#!/usr/bin/env bash
# Build Noema Studio's installer(s) — the opt-in Tauri + Svelte desktop app.
#
# Studio is a SEPARATE app from the lean egui `noema-desktop`; this produces its
# OWN installer (a second GitHub release asset), per the project's two-app model.
# Tauri's bundler emits the right artifact for the host OS:
#
#   macOS   -> .app + .dmg          (WKWebView; no extra runtime to install)
#   Linux   -> .deb + .AppImage     (needs webkit2gtk at runtime)
#   Windows -> .msi + .exe (NSIS)   (WebView2; bootstrapped if missing)
#
# Requirements: Node + npm (front-end build) and rustc >= 1.88 (Tauri 2.x's MSRV,
# above the lean crates' floor — Studio is its own workspace for exactly this).
#
# Optional macOS signing/notarization — Tauri reads these directly:
#   APPLE_SIGNING_IDENTITY="Developer ID Application: … (TEAMID)"
#   APPLE_ID + APPLE_PASSWORD + APPLE_TEAM_ID   (for notarization)
#
# Use a specific toolchain with STUDIO_TOOLCHAIN=1.88.0 (-> `cargo +1.88.0 …`).
set -euo pipefail
cd "$(dirname "$0")/../crates/studio"

CARGO="cargo"
if [[ -n "${STUDIO_TOOLCHAIN:-}" ]]; then CARGO="cargo +${STUDIO_TOOLCHAIN}"; fi

# rustc >= 1.88 guard (Tauri's dependency tree refuses to build below it).
ver="$($CARGO --version | sed -E 's/cargo ([0-9]+\.[0-9]+).*/\1/')"
major="${ver%%.*}"; minor="${ver#*.}"
if (( major < 1 || (major == 1 && minor < 88) )); then
  echo "error: Noema Studio needs rustc >= 1.88 (have $ver)." >&2
  echo "  rustup toolchain install 1.88.0   # then re-run as:" >&2
  echo "  STUDIO_TOOLCHAIN=1.88.0 $0 $*" >&2
  exit 1
fi

echo "==> installing front-end deps"
npm --prefix ui ci 2>/dev/null || npm --prefix ui install

echo "==> ensuring tauri-cli (v2)"
$CARGO tauri --version >/dev/null 2>&1 || $CARGO install tauri-cli --version "^2" --locked

# Auto-update signing. With `bundle.createUpdaterArtifacts: true` set in
# tauri.conf.json, Tauri only emits the detached `.sig` files (and the macOS
# `.app.tar.gz` that the updater consumes instead of the .dmg) when a signing key is
# present. Provide it via the standard Tauri env var, or point STUDIO_UPDATER_KEY at
# the private key file (default: ~/noema-studio-updater.key). The matching PUBLIC key
# is pinned in tauri.conf.json `plugins.updater.pubkey`.
if [[ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]]; then
  KEY_FILE="${STUDIO_UPDATER_KEY:-$HOME/noema-studio-updater.key}"
  if [[ -f "$KEY_FILE" ]]; then
    export TAURI_SIGNING_PRIVATE_KEY="$(cat "$KEY_FILE")"
    export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}"
    echo "==> updater signing: using key file $KEY_FILE"
  else
    echo "WARNING: no updater signing key found (set TAURI_SIGNING_PRIVATE_KEY or place" >&2
    echo "         the key at $KEY_FILE). Bundles will build but WITHOUT the .sig files" >&2
    echo "         the auto-updater requires — Studio auto-update would be inert." >&2
  fi
else
  echo "==> updater signing: using TAURI_SIGNING_PRIVATE_KEY from the environment"
fi

echo "==> building Studio bundles (vite build + Tauri bundler)"
$CARGO tauri build "$@"

echo "==> done. bundles under:"
ls -1d target/release/bundle/*/ 2>/dev/null || true

echo "==> updater artifacts (.sig signatures + macOS .app.tar.gz):"
find target/release/bundle -type f \( -name '*.sig' -o -name '*.app.tar.gz' \) -print 2>/dev/null \
  || echo "   (none — was a signing key set?)"
