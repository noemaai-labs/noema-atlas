# Roadmap

The shipping order is deliberate: deliver value early (a better downloader),
then add the heavier or more controversial peer features once the cache,
manifest, and policy layers are mature.

## Milestone status

| Milestone | Scope | Status |
|-----------|-------|--------|
| **A** | Manifest spec, signing, CAS, verifier, HTTPS mirrors | ✅ done |
| **B** | Hugging Face adapter + revision pinning + token auth | ✅ done |
| **C** | Source health scoring, reputation + integrity banning | ✅ done (a LAN `serve`/mDNS peer transfer was built then **removed** — superseded by worldwide tracker + Iroh) |
| **D** | IPFS trustless gateway adapter | ✅ done (gateway phase) |
| **E** | Iroh peer adapter (QUIC) | ✅ done (`--features iroh`) |
| **E2** | BitTorrent v2 adapter | ❌ removed — the swarm/DHT + vendored-C TLS stack wasn't worth it for a route that never fired in the search-first HF flow (only hand-authored manifests carried magnets). The `bittorrent_v2` source variant is retained deserialize-only for back-compat. |
| **F** | Mobile wrappers (iOS/Android via UniFFI) | 🔜 core is ready; bindings pending |
| **G** | Beta hardening, threat-model review, load tests | 🔄 in progress |

### Done (A–D)

* Ed25519 signed manifests, canonical JSON, validation.
* Content-addressed cache (BLAKE3 + SHA-256), atomic commit, reflink/hardlink
  installs, quarantine, SQLite index.
* Full-file BLAKE3+SHA-256 verification before commit, quarantine on mismatch
  (streaming *per-leaf* rejection is still future — see "Next").
* Adapters: local file, HTTPS mirror (range/resume), Hugging Face, IPFS gateway,
  Iroh peers. Planner with platform/health scoring and failover.
* Policy engine (license classes, gated rules, unsafe-type blocking).
* `noema` CLI + `noema-registry` + the native `noema-desktop` app.
* Unit + integration + security tests; CI matrix on macOS/Windows/Linux.

### Next

* ✅ **Iroh blobs adapter** (`--features iroh`): blob-hash fetch by ticket over
  QUIC, BLAKE3-verified, with n0 discovery + relay for NAT traversal. Exposed via
  `iroh-serve` / `iroh-fetch` and wired into the engine as an `iroh` source.
  Verified with a live two-process transfer (byte-identical, hash-checked).
* ✅ **Multi-peer swarm aggregation (Iroh)**: when the tracker returns several
  peers for a large blob, the leecher splits the blob's chunk space into pieces
  and pulls them from *all* peers at once over per-peer connections (work-stealing,
  so fast peers do more and a dead peer's piece is re-served), making throughput
  the sum of the peers rather than the speed of one. Each piece is bao-verified
  against the file hash as it streams; the full-file dual-hash gate still runs
  before commit. **Crucially leecher-side only**: no change to content identity,
  the manifest, seeding, or the wire protocol — every peer already serving a whole
  blob answers ranged requests, so the existing swarm aggregates with no re-seed.
  One peer / small files fall back to the plain single-peer download.
* ❌ **mDNS LAN discovery** (`serve --mdns` + `discover`): built then **removed**.
  Worldwide tracker + Iroh discovery (NAT-traversing) superseded LAN-only peering.
* ✅ **Mobile FFI**: `crates/mobile-ffi` (UniFFI) compiles; remaining is the
  Swift/Kotlin binding generation + app shells (needs Xcode / Android Studio).
* ✅ **Packaging**: one-click installers for macOS (`.dmg`, Developer ID-signed +
  notarized when secrets are set), Windows (`.exe` installer), and Linux
  (`.AppImage`), built by a tagged-release CI workflow.
* **Streaming per-leaf verification**: ship the per-leaf BLAKE3 hash list in the
  manifest (or fetch it from an untrusted source and validate it against the
  signed Merkle root) so a poisoned chunk is rejected *mid-download*, before the
  whole file arrives. Today the manifest carries only the leaf size + Merkle
  root, so the full-file dual-hash check before commit is the active gate.
* **Trustless IPFS CAR/raw block verification** (beyond gateway fetch).
* **Desktop GUI**: a **native `egui` app** (`noema-desktop`) ships today — pure
  Rust, no webview/HTML, low RAM. A loopback web dashboard (`noema ui`) is also
  available for headless/remote boxes.
* ✅ **Multi-connection downloads**: a single large HTTP(S)/Hugging Face source
  is split into parallel range requests (aria2-style) and verified whole-file
  before commit. Multi-*peer* striping across Iroh peers also ships (see above).
  **Next:** fetch disjoint ranges from *several different HTTP sources* at once,
  and mix HTTP + peer stripes in one transfer (the planner and verifier already
  support the pieces).
* **Optional ClamAV/YARA** hook for sidecar files.
* **Availability/preservation**: source-health dashboards, curated preservation
  nodes for long-tail/huge checkpoints, registry availability hints.

## Known limitations

These are deliberate trade-offs surfaced by the adversarial security/correctness
review, not undiscovered bugs:

* **Single writer per store.** The resume temp path is deterministic per
  `(manifest, artifact)` so a download can resume across process restarts. Two
  *concurrent* processes pointed at the same store downloading the same artifact
  could interleave writes. Run one engine instance per store (the GUI/CLI assume
  this); multi-process locking is a future addition.
* **Partial files are retained on failure (by design).** When every source fails
  with a resumable error, the `.part` file is kept so a later `noema download`
  resumes instead of restarting. It is bounded (one per artifact) and reused on
  the next attempt. It is discarded in three cases: an integrity failure
  (quarantined), or a user **Stop** (the engine deletes the `.part` and the
  download row so the next attempt starts clean — distinct from a user **Pause**,
  which keeps the partial like a failure does). For the P2P (Iroh) source the
  partial lives as an incomplete blob in the node's own store, so Stop also drops
  that store entry; Pause leaves it for resume.

## Definition of done (beta)

A user can: import a signed manifest; download an artifact from any allowed
source combination; resume after interruption; verify end-to-end; see clear
provenance and policy; reuse identical files without re-downloading; and safely
avoid redistributing gated/restricted artifacts. **All of these work today** via
both the native desktop app and the CLI; remaining beta work is streaming
per-leaf verification, mobile shells, and hardening.
