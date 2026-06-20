# Threat model

Noema Atlas distributes large, valuable, sometimes politically- or
commercially-sensitive files from a mix of trusted and untrusted sources. If it
becomes useful, it will attract abuse. The design separates **transport trust**
from **publisher trust**: an untrusted mirror is fine if the manifest is trusted
and the bytes match; an authentic-looking torrent is unacceptable if its contents
don't match a signed manifest.

## Assets

* Integrity of downloaded model weights (no silent tampering).
* Authenticity/provenance (the file is what the publisher signed).
* User access tokens (Hugging Face, private mirrors).
* The local cache (not corrupted by a bad download).
* Respect for gating/licensing (don't leak gated models publicly).

## Trust boundaries

| Component | Trusted? |
|-----------|----------|
| Signed manifest from a key in the user's trust store | **Trusted** |
| Manifest signature math (embedded key) | Verifiable, not inherently trusted |
| Any transport source (HF, mirror, IPFS, peer) | **Untrusted** for content |
| OS keystore | Trusted for secret storage |
| Local CAS blobs (post-commit) | Trusted (immutable, verified) |

## Threats & mitigations

| # | Threat | Mitigation |
|---|--------|-----------|
| 1 | **Corrupted mirror** serves wrong bytes | Full-file BLAKE3+SHA-256+size verification before commit (per-leaf streaming rejection is roadmap, see below); quarantine; failover to another source; resume preserves the verified prefix. (`corrupt_mirror_is_rejected_and_failover_succeeds`) |
| 2 | **Malicious peer / poisoned file** with correct name but wrong hash | Same content verification; the source is **banned** for the session after one integrity failure; nothing enters the CAS. (`poisoned_single_source_…`) |
| 3 | **Replayed / stale manifest** (downgrade) | `model.revision` pins a commit; signatures cover the whole manifest; registry stores by `manifest_id`; clients can pin trusted keys. |
| 4 | **Forged manifest** | Ed25519 signature over canonical bytes; gated/required-signature models demand a *trusted* signer, not just valid math. (`tampered_signature_is_not_trusted`, `unsigned_gated_manifest_is_denied`) |
| 5 | **Unsafe serialized formats** (pickle RCE) | `.pkl/.pickle/.pt/.pth/.ckpt` and executables/scripts are **blocked by default**; GGUF/Safetensors allowed and header-validated; override is explicit (`--allow-unsafe`). |
| 6 | **Path traversal** via artifact paths | `validate_artifact_path` rejects `..`, absolute, drive-letter, NUL on import and before every write. (`path_traversal_artifact_is_rejected_on_import`) |
| 7 | **Stolen tokens** | Tokens live only in the OS keystore (Keychain / Credential Manager / kernel keyutils) or read-only env; never written to plaintext files in the cache. |
| 8 | **Redistribution of gated/licensed models** | License *enforcement* is **out of scope by design** — Atlas is a content-addressed P2P service that verifies *bytes*, not licenses. But the *default* is conservative: with worldwide sharing on (the default), only **openly-licensed** public models are auto-seeded. **Gated/token-walled and restrictively-licensed** content, plus **privately-imported** files, are NOT auto-shared unless the operator opts in — globally ("also share gated/licensed models" in Settings) or per-model — so a license-walled or personal file isn't broadcast by surprise. Once opted in, Atlas doesn't police it; **redistribution-licence compliance is the operator's responsibility** (a disclosure is shown on first run). (`blob_shareable_default_public`) |
| 9 | **Untrusted IPFS gateway / DHT** | Gateways are treated as untrusted transports; bytes are verified client-side, so a lying gateway only causes a retry/fail, never acceptance. (`ipfs_gateway_fetch_matches_manifest`) |
| 10 | **Cache eviction races / interrupted writes / power loss** | Downloads write to `tmp/`; commit is an atomic `rename`; blobs are immutable & made read-only; partial files never become blobs. |
| 11 | **Over-long / hostile responses** | Streaming guards reject a source that sends more than the declared size; safetensors header parsing is size-capped. |
| 12 | **DoS via huge manifests / leaf lists** | Manifests are bounded JSON; fetched leaf lists are validated against the signed Merkle root before trust. |

## Defense-in-depth: verification layers

1. **Manifest signature** — before any network I/O.
2. **Full-file dual hash** — BLAKE3 + SHA-256 + size, before CAS commit. This is
   the active integrity gate: poisoned/corrupt bytes never enter the cache.
3. **Format header** — GGUF magic/version, Safetensors header JSON (advisory
   once the content already matches the signed digest).
4. **Quarantine** — mismatches are moved aside and logged, never committed.

*Roadmap:* a **streaming per-leaf** layer — each 1 MiB leaf checked against the
signed Merkle root as it arrives, rejecting a poisoned chunk mid-download instead
of at end-of-file — is designed for but not yet wired (the manifest carries the
leaf size + root, but downloads don't yet fetch/check the per-leaf hash list).

## Hardening recommendations for operators

* For high-stakes curated manifests, require **two independent publisher
  signatures** (the schema supports multiple signatures; raise the trust bar in
  policy).
* **Share my models worldwide** (and `noema p2p-share`) auto-seed your
  **openly-licensed** public downloads by default. Gated/licensed and
  privately-imported models stay local until you opt them in — globally ("also
  share gated/licensed models") or per-model. Atlas verifies content, not
  licenses, so enable gated sharing only for content you're permitted to
  redistribute; **compliance is the operator's responsibility** (threat-model #8).
  A per-model opt-out and a global off switch are in Settings.
* Keep `--allow-unsafe` off; prefer GGUF/Safetensors.
* Pin trusted keys (`--trusted`) and use `--require-trusted` for sensitive setups.

## Residual risks / non-goals

* Noema Atlas does not scan model *contents* for backdoored weights — it
  guarantees the file matches what a trusted publisher signed, not that the
  publisher is benign. (A ClamAV/YARA hook for sidecar files is a planned
  optional integration.)
* Public-peer **availability** is an economic problem (seeders/pinning), not a
  correctness one; the system layers HF/mirrors for baseline availability and
  treats peer networks as opportunistic acceleration.
