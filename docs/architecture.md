# Architecture

Noema Atlas separates five concerns that other tools tend to conflate:
**identity, policy, caching, discovery, and transport.** No single protocol
(Hugging Face, Iroh, HTTPS, BitTorrent) is a complete answer for
distributing local LLM weights, so none of them is made the namespace. Instead:

* **Identity** = the artifact's content digest (BLAKE3 primary, SHA-256 for
  ecosystem interop). This is what the cache keys on and what dedups files.
* **Truth/policy** = a signed manifest (Ed25519). It binds identity, provenance,
  license, gating, and the list of *permitted* sources.
* **Caching** = a filesystem content-addressed store (CAS) + a SQLite index.
* **Transport** = pluggable adapters that only have to *yield bytes*.
* **Planning** = a platform- and health-aware scorer that orders sources.

```
          ┌─────────────────────────────────────────────┐
          │                 manifest / policy            │  identity, license, gating
          ├─────────────────────────────────────────────┤
          │                   verify                      │  signatures, chunk + full-file
          ├─────────────────────────────────────────────┤
          │              cache (CAS + SQLite)            │  durable, deduplicated
          ├─────────────────────────────────────────────┤
   local │ https │ huggingface │ iroh │ bittorrent       │  transport adapters
          ├─────────────────────────────────────────────┤
          │              planner / engine                │  orchestration + public API
          └─────────────────────────────────────────────┘
```

## Module map (`crates/core`)

| Module        | Responsibility |
|---------------|----------------|
| `error`       | One `Error` type; transport errors classified for retry/ban decisions. |
| `hash`        | Dual BLAKE3+SHA-256 hashing; explicit BLAKE3 Merkle tree over fixed leaves. |
| `manifest`    | Manifest schema, canonical-JSON, validation, source descriptors. |
| `sign`        | Ed25519 key pairs, manifest signing, signature verification + trust. |
| `cas`         | Filesystem CAS: commit/dedup/quarantine, reflink/hardlink install views. |
| `db`          | SQLite index of everything (manifests, sources, cache, health, …). |
| `secret`      | OS keystore abstraction + read-only env fallback. |
| `verify`      | Streaming verifier, file-safety classification, GGUF/Safetensors headers. |
| `policy`      | License/gated policy classes, redistribution + unsafe-type rules. |
| `platform`    | Transport enablement + priority for desktop. |
| `planner`     | Source eligibility + scoring + ordering. |
| `transport`   | The `TransportAdapter` trait + local/https/hf/iroh/bittorrent adapters. |
| `engine`      | The public API; the end-to-end download flow. |

## The download flow

```
User selects model
  └─ import manifest ──> verify signature ──> evaluate policy
        (deny → explain)                          │ allow
                                                  ▼
                      for each artifact:  CAS hit? ── yes ─> dedup, done
                                                  │ no
                                                  ▼
                          plan sources (policy + platform + health)
                                                  │
                                                  ▼
              try best source ─ stream ─ hash incrementally ─ write temp
                 │  drop/incomplete → keep temp, resume on next source
                 │  size/hash wrong → quarantine + ban source + failover
                 └─ complete → full-file dual-hash (+ size) → commit into CAS
                                                  │
                                                  ▼
                              materialize install view (reflink/hardlink)
```

Key properties:

* **Resume across sources.** A partial transfer is written to a temp file keyed
  by `(manifest_id, artifact)`. If source A drops at 60%, source B resumes from
  60% via an HTTP Range request. On resume the verifier is rebuilt by re-reading
  the existing bytes, so integrity state is never lost.
* **Integrity.** A full-file BLAKE3+SHA-256 (and size) check always runs before
  a blob is committed: corrupt or poisoned bytes are quarantined and never enter
  the cache, and the source is banned for the session. *(Per-leaf streaming
  rejection — catching a bad 1 MiB leaf against the signed Merkle root the moment
  it lands, instead of at end-of-file — is designed for but not yet wired: the
  manifest carries the leaf size + Merkle root, but not the per-leaf hash list a
  download would check against. See the roadmap.)*
* **Atomic commit.** Bytes are verified in a temp file, then `rename`d into the
  CAS (same filesystem). A blob is immutable once committed; install views are
  links to it.
* **Quarantine, not overwrite.** Any mismatch moves the bad bytes to
  `quarantine/` and records a row; a good cache entry is never clobbered.
* **Pause vs Stop.** A user can interrupt an in-flight transfer two ways. **Pause**
  (`Engine::request_pause`, surfaced as `Error::Cancelled`) keeps the `.part` temp
  and marks the download row `paused`, so re-downloading resumes from the kept
  bytes. **Stop** (`Engine::request_stop`, surfaced as `Error::Stopped`) discards
  progress: the engine deletes the `.part` temp and the download row, and for the
  P2P (Iroh) source — whose partial is an incomplete blob in the node's own store,
  not the engine's `.part` — the adapter also drops that store entry on the
  discard-cancel, so the next attempt starts clean.

## Content-addressed store layout

```
<root>/
  cas/blake3/aa/bb/<blake3>.blob          immutable content
  cas/blake3/aa/bb/<blake3>.meta.json     {blake3, sha256, size, committed_at}
  chunks/blake3/<blake3>/leaves.merkle    leaf-hash sidecar (post-download re-verification)
  manifests/<manifest_id>.json
  installs/<model-slug>/current/<path>    reflink/hardlink to a blob
  quarantine/<download-id>-<ts>/          rejected bytes (forensics)
  tmp/                                    in-flight downloads
  db/index.sqlite
  auth/                                   (no plaintext secrets — see secret.rs)
```

## Why this hashing scheme

* **BLAKE3** is the primary identity: fast, and natively content-addressed by the
  same kind of root hash Iroh uses for blobs.
* **SHA-256** is carried alongside for interop — the community publishes
  `sha256sums` and BitTorrent v2 keys pieces on SHA-256.
* An **explicit application-level Merkle tree** over fixed 1 MiB leaves is
  designed to give range/streaming verification independent of BLAKE3's internal
  chunking. The signed manifest commits only to the small Merkle *root*; the
  intent is that the (larger) per-leaf hash list can be fetched from any
  untrusted source and re-validated against the signed root before it is trusted
  for per-leaf checks. **Today** the root + leaf size are recorded and a sidecar
  is computed after download for re-verification, but downloads are not yet gated
  leaf-by-leaf (the per-leaf list isn't shipped/fetched) — the full-file
  dual-hash before commit is the active integrity gate. See the roadmap.

## Source planning

Identity priority is constant (manifest digest wins; CAS hit short-circuits).
*Transport* priority is computed per source as a blend of:

* class base priority (Iroh peers are favored over HTTP/HF),
* observed success ratio (neutral prior when unseen),
* latency bonus, native content-addressing bonus,
* integrity penalty (a source that ever served corrupt bytes is **banned**),
* metered/battery deprioritization of heavy peer transports.

Ineligible sources (disallowed class, disabled on platform, banned) are excluded
with a recorded reason that the UI/`noema plan` surfaces.

## Concurrency & safety notes

* The engine is `async` (Tokio). The SQLite connection is wrapped in a `Mutex`
  so `Db` is `Send + Sync` and shareable across tasks.
* Hashing during streaming runs on the async task interleaved with network
  awaits; for very large files this can be moved to `spawn_blocking` (noted as a
  future optimization). Resume re-hashing already uses bounded buffered reads.
* All artifact install paths are validated against traversal (`..`, absolute,
  drive letters, NUL) before any filesystem write.
