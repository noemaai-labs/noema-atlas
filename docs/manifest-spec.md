# Manifest specification (v1.0)

A manifest is the single source of truth for a model's identity, provenance,
license, gating, and the sources it may be fetched from. It is canonical JSON,
signed with Ed25519.

## Canonicalization & signing

* The bytes that are signed/verified are the manifest **with the `signatures`
  array removed**, serialized as compact JSON with **recursively sorted object
  keys** (`Manifest::canonical_bytes`). serde_json's default `Map` is a
  `BTreeMap`, so key order is deterministic — the `preserve_order` feature must
  **not** be enabled.
* The signer's public key is embedded in `publisher.public_keys` *before*
  canonicalization, so the manifest is fully self-verifying: anyone can check the
  signature math without external lookups. Whether the signing key is *trusted*
  is a separate decision made by the verifier's trust store.
* `key_id` convention: `ed25519:<hex-of-32-byte-public-key>`.
* Signatures are base64 of the raw 64-byte Ed25519 signature.

## Top-level shape

```json
{
  "schema_version": "1.0",
  "manifest_id": "mdl_b3_6a4f...d2",
  "publisher": {
    "id": "hf:Qwen/Qwen3-8B-Instruct-GGUF",
    "display_name": "Qwen",
    "public_keys": [
      { "key_id": "ed25519:abc...", "algorithm": "ed25519",
        "public_key": "abc...", "purpose": ["manifest-signing"] }
    ]
  },
  "model": {
    "name": "Qwen3 8B Instruct GGUF",
    "family": "Qwen3", "architecture": "transformer",
    "revision": "hf:commit:0123456789abcdef",
    "format": "gguf", "quantization": "Q4_K_M"
  },
  "license": {
    "spdx": "apache-2.0",
    "license_url": null,
    "redistribution": "public_p2p_allowed"
  },
  "access": {
    "gated": false,
    "require_signed_manifest": true,
    "allowed_source_classes": ["huggingface","https_mirror","ipfs","iroh","local_file"]
  },
  "artifacts": [
    {
      "path": "qwen3-8b-instruct-q4_k_m.gguf",
      "role": "weights",
      "size_bytes": 4920000000,
      "hashes": { "blake3": "6a4f...", "sha256": "c2de..." },
      "chunking": { "leaf_size": 1048576, "leaf_b3_merkle_root": "ab90..." },
      "format": "gguf",
      "sources": [
        { "type": "huggingface", "repo_id": "Qwen/Qwen3-8B-Instruct-GGUF",
          "revision": "0123456789abcdef", "path": "qwen3-8b-instruct-q4_k_m.gguf", "auth": "none" },
        { "type": "https_mirror", "url": "https://mirror.example/model.gguf", "auth": "none" },
        { "type": "ipfs", "cid": "bafy...", "retrieval": ["gateway"], "auth": "none" },
        { "type": "iroh", "blob_hash": "6a4f...", "tickets": [], "auth": "none" },
        { "type": "local_file", "path": "/data/model.gguf" }
      ]
    }
  ],
  "provenance": {
    "origin": "huggingface", "model_card_ref": "hf:model-card",
    "malware_badges_observed": true, "generated_at": "2026-06-16T00:00:00Z"
  },
  "signatures": [
    { "key_id": "ed25519:abc...", "algorithm": "ed25519", "signature": "base64..." }
  ]
}
```

## Field reference

### `license.redistribution` / policy classes

| Value | Meaning |
|-------|---------|
| `public_p2p_allowed` | Fetch from and reseed to any allowed public source. |
| `public_download_only` | Download, but do not reseed onto public peer networks. |
| `gated_no_redistribution` | Signed manifest + authenticated acquisition; never public. |
| `enterprise_private` | Local cache + authorized enterprise sources only. |

### `access`

* `gated` (bool): if true, **forces** a signed manifest, requires a *trusted*
  signature to download, and forbids public redistribution.
* `require_signed_manifest` (bool): a valid signature is required to download.
* `allowed_source_classes`: the publisher's allow-list. When non-empty it is
  authoritative — a source whose class isn't listed is refused.

### `artifacts[].hashes`

Both `blake3` and `sha256` are mandatory, lowercase, 64 hex chars. BLAKE3 is the
cache key and primary identity; SHA-256 is for ecosystem interop.

### `artifacts[].chunking` (optional)

* `leaf_size`: fixed leaf size in bytes (default 1 MiB).
* `leaf_b3_merkle_root`: hex BLAKE3 Merkle root over the leaf hashes. The signed
  root lets a client validate a fetched leaf-hash list and then verify each leaf
  as it streams in.

### `artifacts[].path`

A **relative** install path. Rejected if absolute, contains `..`, a Windows
drive letter, or NUL. Validated on import and before any filesystem write.

### Source descriptors (`type`-tagged)

| `type` | Fields |
|--------|--------|
| `huggingface` | `repo_id`, `revision`, `path`, `auth` |
| `https_mirror` | `url`, `auth` |
| `ipfs` | `cid`, `retrieval[]`, `auth` |
| `iroh` | `blob_hash`, `tickets[]`, `auth` |
| `bittorrent_v2` | `magnet_uri`, `file_merkle_root_sha256?`, `auth` — **retired**: still parses for back-compat, but the adapter was removed so this source is never fetched |
| `lan_peer` | `url`, `auth` — **retired**: LAN peering was removed; still parses for back-compat but is never fetched (superseded by Iroh) |
| `local_file` | `path` |

`auth` is `"none"` or `"token"`. When `"token"`, the engine resolves a credential
from the OS keystore (or environment, e.g. `HF_TOKEN`) for that source's service.

## Building manifests

The CLI computes hashes, sizes, and the chunk tree for you:

```sh
noema manifest build \
  --name "My Model" \
  --artifact "model.gguf=/path/to/model.gguf" \
  --source "model.gguf:hf:Org/Repo@main/model.gguf" \
  --source "model.gguf:https:https://mirror/model.gguf" \
  --sign <key_id> --out my.json
```
