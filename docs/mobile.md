# Mobile (iOS / Android) plan

The whole point of putting the engine in a pure-Rust `noema-core` crate is that
the *same* verified-download logic runs on desktop, iOS, and Android. The core
is deliberately free of desktop-only assumptions:

* `noema-core` builds as `lib`, `staticlib`, and `cdylib` (see its
  `[lib] crate-type`), which are exactly the artifact kinds Swift and Kotlin
  consume.
* TLS is rustls (no system OpenSSL), SQLite is bundled (no system SQLite), and
  secrets go through a `SecretStore` trait with platform backends.

## Binding strategy: UniFFI

This ships today as **`crates/mobile-ffi`** (`noema-mobile-ffi`), a
[UniFFI](https://mozilla.github.io/uniffi-rs/) wrapper over `Engine`. It builds
as `cdylib`/`staticlib` and exposes a `NoemaEngine` object:

```rust
#[uniffi::export]
impl NoemaEngine {
    #[uniffi::constructor]
    pub fn new(root: String) -> Result<Arc<Self>, FfiError>;
    pub fn import_manifest(&self, json: Vec<u8>) -> Result<ImportInfo, FfiError>;
    pub fn list_manifests(&self) -> Result<Vec<ManifestInfo>, FfiError>;
    pub fn download(&self, manifest_id: String) -> Result<DownloadInfo, FfiError>; // blocking
    pub fn materialize(&self, manifest_id: String, target_dir: String) -> Result<u32, FfiError>;
    pub fn import_file(&self, manifest_id: String, artifact_path: String, file_path: String) -> Result<ArtifactInfo, FfiError>;
    pub fn list_cache(&self) -> Result<Vec<CacheInfo>, FfiError>;
}
```

`download` blocks on an embedded Tokio runtime, so call it off the platform main
thread. Generate the Swift/Kotlin bindings with the `uniffi-bindgen` CLI against
the built library.

Build targets:

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios \
                  aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
# iOS:     cargo build -p mobile-ffi --target aarch64-apple-ios --release  → .a + XCFramework
# Android: cargo-ndk to produce per-ABI .so, packaged into the AAR
```

## Platform-aware behavior (already in the core)

`PlatformProfile` encodes the rollout matrix, and the planner honors it:

| Platform | Leads with | Held back (v1) |
|----------|-----------|----------------|
| Desktop  | local CAS → LAN → Iroh → IPFS → HF/HTTPS | — |
| iOS      | local CAS → **HF/HTTPS (background URLSession)** → IPFS gateway → LAN/Iroh (foreground) | public seeding, indefinite relay |
| Android  | local CAS → HF/HTTPS → LAN/Iroh (foreground) | always-on public seeding |

`allow_public_seeding` and `background_p2p` default to **false** on mobile.

## iOS specifics

* Use **background `URLSession`** for the HTTP/HF transports so large downloads
  survive app suspension. The Rust core exposes range/resume semantics; the
  Swift wrapper can drive the actual transfers and hand verified bytes to the
  core, or the core can run foreground transfers directly.
* Trigger the **local-network permission** prompt before LAN discovery/transfer;
  keep peer transfers foreground-only in v1.
* Mark re-creatable cache files with `isExcludedFromBackup` (Apple provides a
  backup-exclusion key for cache/app-support data).
* Do **not** ship public seeding in v1 — App Review scrutinizes apps that
  facilitate illegal file sharing. Frame the app as an *authorized model
  acquisition and verification tool*.

## Android specifics

* Use a **`dataSync` foreground service** (or `WorkManager`) for large transfers;
  respect Android 15's time limits on background foreground-service types.
* LAN/Iroh in the foreground first; optionally Wi-Fi Direct for local transfer.
* Public seeding disabled by default; keep background swarms bounded.
* Secrets via the Android Keystore (the `keyring` crate's backend, or a Kotlin
  shim through the FFI).

## Secret storage on mobile

`secret::SecretStore` already abstracts this. On desktop it uses the `keyring`
crate (Keychain / Credential Manager / kernel keyutils). On mobile the FFI layer
can either reuse `keyring`'s Apple/Android backends or delegate to a
Swift/Kotlin-implemented store passed into the core.

## Status

The core is mobile-ready (pure Rust, right crate-types, platform profiles,
abstracted secrets) **and the `crates/mobile-ffi` UniFFI surface compiles today**.
What remains is purely platform-native scaffolding that needs Xcode / Android
Studio to build: running `uniffi-bindgen` to emit the Swift/Kotlin glue, the
XCFramework / AAR packaging, and the SwiftUI / Jetpack Compose app shells (UI +
background-transfer + permission plumbing). No new engine logic is required.
