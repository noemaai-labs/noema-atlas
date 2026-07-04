use crate::error::{Error, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// Versioned prefix so the format can evolve without ambiguity.
const PREFIX: &str = "atlas1:";
/// Prefix for a *multi-file* bundle link (a whole sharded model in one link).
const BUNDLE_PREFIX: &str = "atlasb1:";
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Everything a peer needs to fetch + verify a shared file.
///
/// `name` is the canonical lookup/filename; the optional `title`/`family`/
/// `quant`/`desc`/`origin` fields let a receiver render a meaningful card *fully
/// offline* — without them, a model that isn't on Hugging Face would show up as
/// nothing but a filename. They are descriptive metadata only: the sender typed
/// them, they are NOT part of the content-verification path (only the hashes
/// are), so a receiver UI must present them as "sender says", not as verified.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareTarget {
    /// Suggested file / display name (the canonical slug or filename).
    #[serde(rename = "n", default)]
    pub name: String,
    /// File size in bytes (0 if unknown).
    #[serde(rename = "sz", default)]
    pub size: u64,
    /// SHA-256 content id (lowercase hex). The primary lookup key.
    #[serde(rename = "s", default)]
    pub sha256: String,
    /// BLAKE3 content id (lowercase hex). Optional — resolved via the tracker if
    /// absent — but included when known for a direct, robust lookup.
    #[serde(rename = "b3", default)]
    pub blake3: String,
    /// License tag (SPDX or HF-style), when known. Lets the receiver decide
    /// whether the model may be auto-reseeded: an open/open-weight license is
    /// reshared by default; an unknown license is fetched but kept private until
    /// the user opts in (so a gated model can't be laundered public via a link).
    #[serde(rename = "l", default)]
    pub license: String,
    /// Human display title chosen by the sender, e.g. `Mistral-7B-Instruct-v0.3`.
    #[serde(rename = "t", default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// Model family, e.g. `Mistral`.
    #[serde(rename = "f", default, skip_serializing_if = "String::is_empty")]
    pub family: String,
    /// Quantization label, e.g. `Q4_K_M`.
    #[serde(rename = "q", default, skip_serializing_if = "String::is_empty")]
    pub quant: String,
    /// Free-text description / model-card note (kept short; it rides in the link).
    #[serde(rename = "d", default, skip_serializing_if = "String::is_empty")]
    pub desc: String,
    /// Where this came from — e.g. the old Hugging Face URL, now gone.
    #[serde(rename = "o", default, skip_serializing_if = "String::is_empty")]
    pub origin: String,
    /// BitTorrent magnet (info-hash) when the sender is seeding this file over
    /// BitTorrent — lets a receiver join the swarm straight from the link. Like the
    /// other descriptive fields it is "sender says"; the bytes are still verified
    /// against the content hashes above.
    #[serde(rename = "mag", default, skip_serializing_if = "String::is_empty")]
    pub magnet: String,
}

impl ShareTarget {
    /// Whether this target has at least one usable 64-hex content id. The hex
    /// charset check matters: a content id is later sliced/byte-indexed and used to
    /// derive on-disk names, so a 64-*byte* string with a multi-byte codepoint must
    /// not pass here (it would otherwise panic on a non-char-boundary slice).
    pub fn has_content_id(&self) -> bool {
        is_hex64(&self.sha256) || is_hex64(&self.blake3)
    }

    /// The best human title to show: the sender's `title` if set, else the name.
    pub fn display_title(&self) -> &str {
        if !self.title.trim().is_empty() {
            self.title.trim()
        } else {
            self.name.trim()
        }
    }

    /// Encode as a copy-pasteable token: `atlas1:<base64url(json)>`.
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).unwrap_or_default();
        format!("{PREFIX}{}", B64.encode(json))
    }

    /// Parse a share token, or a bare 64-hex sha256 content id. Tolerant of
    /// surrounding whitespace.
    pub fn decode(token: &str) -> Result<ShareTarget> {
        let t = token.trim();
        if let Some(b64) = t.strip_prefix(PREFIX) {
            let bytes = B64
                .decode(b64.trim())
                .map_err(|e| Error::other(format!("invalid share link: {e}")))?;
            let mut target: ShareTarget = serde_json::from_slice(&bytes)
                .map_err(|e| Error::other(format!("invalid share link: {e}")))?;
            target.sha256 = target.sha256.trim().to_lowercase();
            target.blake3 = target.blake3.trim().to_lowercase();
            if !target.has_content_id() {
                return Err(Error::other("share link has no usable content id"));
            }
            if target.name.trim().is_empty() {
                target.name = "shared-model.gguf".to_string();
            }
            Ok(target)
        } else {
            let hex = t.to_lowercase();
            if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                Ok(ShareTarget {
                    name: "shared-model.gguf".to_string(),
                    sha256: hex,
                    ..Default::default()
                })
            } else {
                Err(Error::other(
                    "not a valid Atlas share link or 64-character content id",
                ))
            }
        }
    }
}

/// A multi-file bundle: a whole sharded model (e.g. `model-00001-of-00003`,
/// `config.json`, `tokenizer.json`) shared under **one** copy-pasteable link.
/// Each file is still verified independently against its own content id; the
/// bundle just groups them so a receiver fetches the whole model in one paste.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareBundle {
    /// Display name for the whole model, e.g. `Llama-3.1-70B-Instruct`.
    #[serde(rename = "n", default)]
    pub name: String,
    /// The files that make up the model (weights shards + config/tokenizer).
    #[serde(rename = "fs", default)]
    pub files: Vec<ShareTarget>,
}

impl ShareBundle {
    /// At least one file with a usable content id.
    pub fn is_usable(&self) -> bool {
        self.files.iter().any(|f| f.has_content_id())
    }

    /// Encode as `atlasb1:<base64url(json)>`.
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).unwrap_or_default();
        format!("{BUNDLE_PREFIX}{}", B64.encode(json))
    }

    /// Parse a bundle token (`atlasb1:…`). Whitespace-tolerant.
    pub fn decode(token: &str) -> Result<ShareBundle> {
        let t = token.trim();
        let b64 = t
            .strip_prefix(BUNDLE_PREFIX)
            .ok_or_else(|| Error::other("not an Atlas bundle link"))?;
        let bytes = B64
            .decode(b64.trim())
            .map_err(|e| Error::other(format!("invalid bundle link: {e}")))?;
        let mut bundle: ShareBundle = serde_json::from_slice(&bytes)
            .map_err(|e| Error::other(format!("invalid bundle link: {e}")))?;
        for f in &mut bundle.files {
            f.sha256 = f.sha256.trim().to_lowercase();
            f.blake3 = f.blake3.trim().to_lowercase();
        }
        bundle.files.retain(|f| f.has_content_id());
        if !bundle.is_usable() {
            return Err(Error::other("bundle link has no usable files"));
        }
        Ok(bundle)
    }
}

/// Whether a token looks like a multi-file bundle link (vs a single share link).
pub fn is_bundle_link(token: &str) -> bool {
    token.trim().starts_with(BUNDLE_PREFIX)
}

/// A 64-character lowercase/uppercase ASCII-hex string (a sha256/blake3 content id).
fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_round_trip() {
        let bundle = ShareBundle {
            name: "Llama-3.1-70B-Instruct".into(),
            files: vec![
                ShareTarget {
                    name: "model-00001-of-00002.safetensors".into(),
                    size: 5_000_000_000,
                    sha256: "11".repeat(32),
                    blake3: "22".repeat(32),
                    quant: "F16".into(),
                    ..Default::default()
                },
                ShareTarget {
                    name: "config.json".into(),
                    size: 700,
                    sha256: "33".repeat(32),
                    ..Default::default()
                },
            ],
        };
        let token = bundle.encode();
        assert!(token.starts_with("atlasb1:"));
        assert!(is_bundle_link(&token));
        assert!(!is_bundle_link(&bundle.files[0].encode()));
        assert_eq!(ShareBundle::decode(&token).unwrap(), bundle);
    }

    #[test]
    fn bundle_drops_idless_files_and_rejects_empty() {
        let bundle = ShareBundle {
            name: "x".into(),
            files: vec![ShareTarget {
                name: "no-id.gguf".into(),
                ..Default::default()
            }],
        };
        assert!(ShareBundle::decode(&bundle.encode()).is_err());
    }

    #[test]
    fn round_trip_full() {
        let t = ShareTarget {
            name: "qwen3-8b-q4_k_m.gguf".into(),
            size: 4_920_000_000,
            sha256: "c2de".repeat(16),
            blake3: "6a4f".repeat(16),
            license: "apache-2.0".into(),
            title: "Qwen3-8B".into(),
            family: "Qwen3".into(),
            quant: "Q4_K_M".into(),
            desc: "Rescued reupload after the repo was removed.".into(),
            origin: "huggingface.co/Qwen/Qwen3-8B-GGUF".into(),
            magnet: "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567".into(),
        };
        let token = t.encode();
        assert!(token.starts_with("atlas1:"));
        assert_eq!(ShareTarget::decode(&token).unwrap(), t);

        // The optional metadata fields are backward-compatible: a v1 link with
        // only the original five fields still decodes, with empties for the rest.
        let legacy = ShareTarget {
            name: "old.gguf".into(),
            sha256: "ab".repeat(32),
            ..Default::default()
        };
        let back = ShareTarget::decode(&legacy.encode()).unwrap();
        assert_eq!(back.display_title(), "old.gguf");
        assert!(back.title.is_empty());
    }

    #[test]
    fn bare_sha256_content_id() {
        let sha = "ab".repeat(32);
        let t = ShareTarget::decode(&format!("  {}\n", sha.to_uppercase())).unwrap();
        assert_eq!(t.sha256, sha);
        assert!(t.blake3.is_empty());
        assert_eq!(t.size, 0);
        assert!(t.has_content_id());
    }

    #[test]
    fn rejects_garbage() {
        assert!(ShareTarget::decode("hello").is_err());
        assert!(ShareTarget::decode("atlas1:!!!notbase64").is_err());
    }

    #[test]
    fn rejects_non_hex_content_id() {
        // 64 *bytes* but containing a multi-byte codepoint: must be rejected, not
        // accepted (it would later panic on a non-char-boundary slice).
        let mut sha = "a".repeat(62);
        sha.push('é'); // +2 bytes -> 64 bytes, non-hex
        let t = ShareTarget {
            name: "x.gguf".into(),
            sha256: sha,
            ..Default::default()
        };
        assert!(!t.has_content_id());
        assert!(ShareTarget::decode(&t.encode()).is_err());
    }
}
