# Releasing — one-click installers for macOS, Windows, Linux

Tagging a release builds and attaches a one-click installer for every OS:

| OS | Artifact | How users run it |
|----|----------|------------------|
| macOS | `Noema-Atlas-macos.dmg` (universal arm64 + x86_64) | Open the dmg, drag **Noema Atlas** to Applications. Developer ID-signed + notarized → opens with no warning. |
| Windows | `Noema-Atlas-Setup.exe` (+ `Noema-Atlas-windows-x86_64.zip` portable) | Run the installer (Start-menu + desktop shortcuts, uninstaller). |
| Linux | `Noema-Atlas-x86_64.AppImage` (+ `noema-atlas-linux-x86_64.tar.gz`) | `chmod +x` and double-click / run. |

```sh
git tag v0.1.0 && git push --tags     # → .github/workflows/release.yml
```

Without the signing secrets below, the workflow still succeeds: macOS falls back
to an **ad-hoc** signature (Gatekeeper warns; right-click → Open), Windows ships
unsigned, and Linux is unaffected (AppImages aren't signed).

## macOS signing + notarization (Noema developer account)

Set these as repository **Actions secrets**. They map onto `scripts/bundle-macos.sh`.

| Secret | What it is |
|--------|-----------|
| `MACOS_CERT_P12` | Base64 of your **Developer ID Application** certificate exported as `.p12`. Create with `base64 -i cert.p12 \| pbcopy`. |
| `MACOS_CERT_PASSWORD` | The password you set when exporting the `.p12`. |
| `MACOS_SIGN_IDENTITY` | The identity string, e.g. `Developer ID Application: Noema, Inc. (TEAMID)`. Run `security find-identity -v -p codesigning` to see it. |
| `MACOS_NOTARY_APPLE_ID` | The Apple ID email of the developer account. |
| `MACOS_NOTARY_PASSWORD` | An **app-specific password** (appleid.apple.com → Sign-In & Security → App-Specific Passwords), *not* your Apple ID password. |
| `MACOS_NOTARY_TEAM_ID` | Your 10-char Apple Team ID. |

How it works in CI:
1. The cert is imported into a temporary keychain.
2. `bundle-macos.sh --universal --dmg` signs the app with the hardened runtime +
   a secure timestamp (the prerequisites for notarization) and builds the dmg.
3. The dmg is submitted to Apple (`notarytool ... --wait`) and the ticket is
   **stapled** so the app opens offline with no Gatekeeper prompt.

To sign/notarize locally instead of in CI:

```sh
export SIGN_IDENTITY="Developer ID Application: Noema, Inc. (TEAMID)"
# Either store creds once: xcrun notarytool store-credentials noema --apple-id … --team-id … --password …
export NOTARY_KEYCHAIN_PROFILE="noema"
# …or pass them inline:
export NOTARY_APPLE_ID="you@example.com" NOTARY_PASSWORD="app-specific-pw" NOTARY_TEAM_ID="TEAMID"
scripts/bundle-macos.sh --universal --dmg dist
```

## Windows signing (optional)

| Secret | What it is |
|--------|-----------|
| `WINDOWS_CERT_PFX_PATH` | Path to an Authenticode `.pfx` on the runner (or wire a base64 import step like macOS). |
| `WINDOWS_CERT_PASSWORD` | The `.pfx` password. |

`scripts/bundle-windows.ps1` signs each binary and the installer with `signtool`
when these are set; otherwise the installer is unsigned (SmartScreen may warn
until the certificate builds reputation).

## Linux

No secrets needed. The AppImage is built with `linuxdeploy` so it bundles the
shared libraries the egui app needs and runs across distros.
