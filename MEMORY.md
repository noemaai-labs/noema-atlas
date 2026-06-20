# Noema Atlas Memory

This file keeps project decisions that should survive source-comment cleanup.

## Workspace And Packaging

- The default workspace intentionally excludes `crates/studio`. Studio is a separate Tauri workspace because Tauri 2.x requires Rust 1.88+, while the lean CLI, core, registry, desktop, and mobile crates keep the lower workspace MSRV.
- Build Studio from `crates/studio`; standalone Studio builds need the `custom-protocol` feature so Tauri serves the embedded frontend instead of expecting the dev server.
- The lean desktop app is the primary Noema Atlas app. Studio is an optional Tauri/Svelte frontend over the same engine root.

## Trust And Verification

- Sources provide bytes; manifests, signatures, and content digests provide truth. The same blob fetched from Hugging Face, HTTPS, IPFS, local files, or Iroh deduplicates by content hash.
- Share-link display metadata is advisory. Receivers should treat title, family, quant, description, origin, and license as sender-supplied; only content ids verify bytes.
- Verification is cheapest-first: manifest policy/signature checks, streaming chunk checks when available, then full-file BLAKE3/SHA-256 before CAS commit. Bad bytes are quarantined instead of replacing good cache entries.

## Worldwide Peer Tracking

- The tracker keys live providers by BLAKE3 and keeps a SHA-256 to BLAKE3 alias for clients that only know Hub hashes.
- Provider records are keyed by stable NodeId, not transient tickets, so re-announces with changed addresses do not inflate peer counts.
- Provider metadata is per provider record. Public visibility, group visibility, labels, and peer counts are derived from the currently live providers, not one mutable global row.
- The provider TTL is 15 minutes. Clients should re-announce more often than that, and explicit withdraws are used for share-off, delete, shutdown, and close flows so peer-visible state changes immediately.
- Peer queries exclude the caller's own stable NodeId. A user's own seeding device should not show up as a downloadable peer.

## Iroh Transport

- Iroh uses BLAKE3 content addressing and a disk-backed blob store, so multi-GB models can be seeded by reference without copying into RAM.
- The node key is persisted under the engine store so the NodeId remains stable across launches and tracker records can be withdrawn or excluded later.
- Per-file share-off uses a connection registry around the blob accept handler to close only the peers pulling that blob.
- Connect, stall, address-resolution, shutdown, and upload-slot watchdogs exist to keep dead peers or unresponsive endpoints from pinning the UI or stale upload counters.

## Desktop And Studio Runtime

- Desktop and Studio share the same engine root and model cache but use separate UI settings files.
- Studio and desktop should withdraw tracker announces and stop seeders on quit/close so closed apps do not linger as peers until TTL expiry.
- Mirror, proxy, tracker, and seeding startup settings are read when the engine opens; UI changes to those settings may require restart unless explicitly wired live.

## Mobile FFI

- Mobile `download` calls block on an embedded Tokio runtime. Swift and Kotlin callers should invoke them off the platform main thread.
