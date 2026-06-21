# Releasing Noema Atlas

This repo uses a split release flow:

- GitHub Actions builds and uploads Linux and Windows artifacts for Atlas and Atlas Studio.
- macOS artifacts are built, signed, and notarized locally, then uploaded to the same GitHub release.
- No GitHub web UI step is required unless you want to edit release notes, title, or prerelease status.

## Normal push checks

Regular pushes to `main` or `master`, and pull requests, run the lightweight CI workflow:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

There is no CodeQL workflow in this repo.

## Release sequence

Use a version tag that starts with `v`, such as `v1.2.3`.

1. Make sure the release commit is pushed:

   ```sh
   git push origin main
   ```

2. Build, sign, and notarize the macOS artifacts locally.

   Atlas:

   ```sh
   export SIGN_IDENTITY="Developer ID Application: Noema, Inc. (TEAMID)"
   export NOTARY_KEYCHAIN_PROFILE="noema"
   scripts/bundle-macos.sh --universal --dmg --zip dist
   ```

   This produces:

   ```text
   dist/Noema-Atlas-macos-universal.dmg
   dist/Noema-Atlas-macos-universal.zip
   ```

   Atlas Studio:

   ```sh
   export APPLE_SIGNING_IDENTITY="Developer ID Application: Noema, Inc. (TEAMID)"
   export APPLE_ID="you@example.com"
   export APPLE_PASSWORD="app-specific-password"
   export APPLE_TEAM_ID="TEAMID"
   scripts/bundle-studio.sh
   ```

   Studio macOS bundles are written under:

   ```text
   crates/studio/target/release/bundle/
   ```

3. Push the release tag:

   ```sh
   git tag v1.2.3
   git push origin v1.2.3
   ```

   The tag push starts both release workflows:

   - `.github/workflows/release.yml` builds Atlas for Linux and Windows.
   - `.github/workflows/release-studio.yml` builds Atlas Studio for Linux and Windows.

4. Wait for both GitHub Actions workflows to finish.

   The GitHub release is created or updated as individual jobs finish. This means the release page may temporarily show only some Linux/Windows assets while other jobs are still running.

5. Upload the locally notarized macOS artifacts to the same tag release:

   ```sh
   gh release upload v1.2.3 \
     dist/Noema-Atlas-macos-universal.dmg \
     dist/Noema-Atlas-macos-universal.zip
   ```

   Upload the Atlas Studio macOS artifact from `crates/studio/target/release/bundle/` as well. Use the exact file path produced by Tauri, for example:

   ```sh
   gh release upload v1.2.3 crates/studio/target/release/bundle/dmg/*.dmg
   ```

## GitHub-built assets

The `release.yml` workflow uploads these Atlas assets:

| OS | Assets |
| --- | --- |
| Linux | `Noema-Atlas-x86_64.AppImage`, `noema-atlas-linux-x86_64.tar.gz` |
| Windows | `Noema-Atlas-Setup.exe`, `Noema-Atlas-windows-x86_64.zip` |

The `release-studio.yml` workflow uploads Atlas Studio Linux and Windows installer assets collected from Tauri's bundle output, such as `.AppImage`, `.deb`, `.msi`, or `.exe` files.

macOS is intentionally not built by GitHub Actions. Keep Mac signing and notarization local so the final Mac binaries are produced under your Apple Developer credentials.

## Manual workflow runs

Both release workflows also support `workflow_dispatch`. Manual runs are useful for test packaging, but tag pushes are the normal release path because tag runs upload to the GitHub release automatically.

On non-tag manual runs, artifacts are uploaded as workflow artifacts instead of release assets.
