use crate::error::{Error, Result};
use crate::hash::Hashes;
use serde::{Deserialize, Serialize};

/// The schema version this build understands.
pub const SCHEMA_VERSION: &str = "1.0";

/// Top-level signed manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub schema_version: String,
    pub manifest_id: String,
    pub publisher: Publisher,
    pub model: Model,
    pub license: License,
    pub access: Access,
    pub artifacts: Vec<Artifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    #[serde(default)]
    pub signatures: Vec<Signature>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Publisher {
    /// Stable publisher identity, e.g. `hf:Qwen/Qwen3-8B-Instruct-GGUF`.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Public keys that may sign manifests for this publisher.
    #[serde(default)]
    pub public_keys: Vec<PublicKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicKey {
    /// `ed25519:<hex32>` by convention; treated as opaque elsewhere.
    pub key_id: String,
    pub algorithm: SigAlgorithm,
    /// Raw public key material, hex-encoded (32 bytes for Ed25519).
    pub public_key: String,
    #[serde(default)]
    pub purpose: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SigAlgorithm {
    Ed25519,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Model {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
    /// Revision pin, e.g. `hf:commit:0123abcd` for reproducibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantization: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct License {
    /// SPDX identifier where possible, e.g. `apache-2.0`.
    pub spdx: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license_url: Option<String>,
    /// Redistribution policy class governing this model.
    pub redistribution: RedistributionClass,
}

/// Redistribution / policy class. Used both as the license's declared policy and
/// as the resolved policy class by the policy engine.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RedistributionClass {
    /// May be fetched from and reseeded to any allowed public source.
    PublicP2pAllowed,
    /// May be downloaded, but not reseeded onto public peer networks.
    PublicDownloadOnly,
    /// Gated: requires a signed manifest + authenticated acquisition; never
    /// redistributed publicly by default.
    GatedNoRedistribution,
    /// Confined to local cache and authorized enterprise sources only.
    EnterprisePrivate,
}

impl RedistributionClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            RedistributionClass::PublicP2pAllowed => "public_p2p_allowed",
            RedistributionClass::PublicDownloadOnly => "public_download_only",
            RedistributionClass::GatedNoRedistribution => "gated_no_redistribution",
            RedistributionClass::EnterprisePrivate => "enterprise_private",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        Some(match s {
            "public_p2p_allowed" => RedistributionClass::PublicP2pAllowed,
            "public_download_only" => RedistributionClass::PublicDownloadOnly,
            "gated_no_redistribution" => RedistributionClass::GatedNoRedistribution,
            "enterprise_private" => RedistributionClass::EnterprisePrivate,
            _ => return None,
        })
    }

    /// Whether public peer redistribution (IPFS/BT/Iroh/public LAN advertising)
    /// is permitted for this class.
    pub fn allows_public_redistribution(&self) -> bool {
        matches!(self, RedistributionClass::PublicP2pAllowed)
    }

    /// Classify a license tag (SPDX or HF-style) into a redistribution policy:
    /// open / open-weight licenses permit P2P reseed; anything unrecognized (or
    /// absent) is download-only. Shared by the Hugging Face importer and the
    /// content-link importer so both judge "is this safe to reseed?" identically.
    pub fn for_license(license: Option<&str>) -> RedistributionClass {
        match license {
            Some(s) if license_permits_redistribution(&s.trim().to_lowercase()) => {
                RedistributionClass::PublicP2pAllowed
            }
            _ => RedistributionClass::PublicDownloadOnly,
        }
    }
}

/// Whether a (normalized, lowercased) license tag permits redistributing weights.
/// Matched by family so versioned/cased variants and the common open-weight model
/// licenses (Llama, Gemma, Qwen, Mistral, Falcon, …) are recognized — not just a
/// tiny OSI subset. Anything unrecognized stays download-only; the user can still
/// opt a specific model in from the Library.
fn license_permits_redistribution(l: &str) -> bool {
    // Exact permissive/open licenses.
    const EXACT: &[&str] = &[
        "mit",
        "mit-0",
        "isc",
        "unlicense",
        "wtfpl",
        "zlib",
        "cc0-1.0",
        "cc0",
        "artistic-2.0",
        "postgresql",
    ];
    if EXACT.contains(&l) {
        return true;
    }
    // Family prefixes (covers versioned + named open-weight model licenses).
    const PREFIXES: &[&str] = &[
        "apache",     // apache-2.0
        "bsd",        // bsd-2-clause / bsd-3-clause / ...
        "mpl",        // mpl-2.0
        "cc-by",      // cc-by-4.0, cc-by-sa-4.0, cc-by-nc-* (still redistributable)
        "openrail",   // openrail, bigscience-openrail-m, creativeml-openrail-m
        "creativeml", // creativeml-openrail-m
        "bigscience", // bigscience-bloom-rail, bigscience-openrail
        "llama",      // llama2, llama3, llama3.1, llama3.2, llama3.3, llama4
        "gemma",      // gemma terms permit redistribution with the license
        "qwen",       // qwen, qwen-research, tongyi (alias below)
        "tongyi",     // tongyi-qianwen
        "mistral",    // mistral / apache-based; also "apache" above
        "falcon",     // tiiuae falcon licenses
        "tiiuae",
        "deepseek", // deepseek model license permits redistribution
        "gpl",      // gpl-*/agpl-* permit redistribution
        "lgpl",
        "epl",
    ];
    PREFIXES.iter().any(|p| l.starts_with(p))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Access {
    #[serde(default)]
    pub gated: bool,
    #[serde(default = "default_true")]
    pub require_signed_manifest: bool,
    /// Which source classes the publisher permits for this model.
    #[serde(default)]
    pub allowed_source_classes: Vec<SourceClass>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SourceClass {
    Huggingface,
    HttpsMirror,
    Ipfs,
    Iroh,
    BittorrentV2,
    LanPeer,
    LocalFile,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Artifact {
    /// Relative install path. Must be sanitized (no traversal, no absolute).
    pub path: String,
    /// Semantic role, e.g. `weights`, `tokenizer`, `config`.
    pub role: String,
    pub size_bytes: u64,
    pub hashes: Hashes,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunking: Option<Chunking>,
    /// File format, e.g. `gguf`, `safetensors`, `json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default)]
    pub sources: Vec<Source>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Chunking {
    pub leaf_size: u64,
    /// Hex BLAKE3 Merkle root over the artifact's leaf hashes.
    pub leaf_b3_merkle_root: String,
}

/// A place bytes can be fetched from. Tagged by `type` in JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Source {
    Huggingface {
        repo_id: String,
        revision: String,
        path: String,
        #[serde(default)]
        auth: AuthPolicy,
    },
    HttpsMirror {
        url: String,
        #[serde(default)]
        auth: AuthPolicy,
    },
    Ipfs {
        cid: String,
        #[serde(default)]
        retrieval: Vec<String>,
        #[serde(default)]
        auth: AuthPolicy,
    },
    Iroh {
        blob_hash: String,
        #[serde(default)]
        tickets: Vec<String>,
        #[serde(default)]
        auth: AuthPolicy,
    },
    BittorrentV2 {
        magnet_uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_merkle_root_sha256: Option<String>,
        #[serde(default)]
        auth: AuthPolicy,
    },
    /// A peer on the local network serving its content-addressed store over HTTP.
    LanPeer {
        url: String,
        #[serde(default)]
        auth: AuthPolicy,
    },
    /// Import from a local file already on disk.
    LocalFile { path: String },
}

impl Source {
    pub fn class(&self) -> SourceClass {
        match self {
            Source::Huggingface { .. } => SourceClass::Huggingface,
            Source::HttpsMirror { .. } => SourceClass::HttpsMirror,
            Source::Ipfs { .. } => SourceClass::Ipfs,
            Source::Iroh { .. } => SourceClass::Iroh,
            Source::BittorrentV2 { .. } => SourceClass::BittorrentV2,
            Source::LanPeer { .. } => SourceClass::LanPeer,
            Source::LocalFile { .. } => SourceClass::LocalFile,
        }
    }

    /// A stable identifier for health/reputation tracking.
    pub fn source_id(&self) -> String {
        match self {
            Source::Huggingface {
                repo_id,
                revision,
                path,
                ..
            } => {
                format!("hf:{repo_id}@{revision}/{path}")
            }
            Source::HttpsMirror { url, .. } => format!("https:{url}"),
            Source::Ipfs { cid, .. } => format!("ipfs:{cid}"),
            Source::Iroh { blob_hash, .. } => format!("iroh:{blob_hash}"),
            Source::BittorrentV2 { magnet_uri, .. } => format!("btv2:{magnet_uri}"),
            Source::LanPeer { url, .. } => format!("lan:{url}"),
            Source::LocalFile { path } => format!("file:{path}"),
        }
    }

    pub fn auth(&self) -> AuthPolicy {
        match self {
            Source::Huggingface { auth, .. }
            | Source::HttpsMirror { auth, .. }
            | Source::Ipfs { auth, .. }
            | Source::Iroh { auth, .. }
            | Source::BittorrentV2 { auth, .. }
            | Source::LanPeer { auth, .. } => *auth,
            Source::LocalFile { .. } => AuthPolicy::None,
        }
    }

    /// Whether this source class is reachable purely with public discovery (and
    /// therefore subject to redistribution policy when advertising).
    pub fn is_public_distribution(&self) -> bool {
        matches!(self, Source::Ipfs { .. } | Source::Iroh { .. })
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthPolicy {
    /// No authentication required.
    #[default]
    None,
    /// A bearer token is required; the engine looks it up in the OS keystore.
    Token,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Provenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// A reference (often a URL) to the model's source / card — e.g. the old
    /// Hugging Face page a rescued model used to live at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_card_ref: Option<String>,
    /// A free-text note the sharer wrote describing the model (what it is, what
    /// it was fine-tuned for, why it left its original home).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub malware_badges_observed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Signature {
    pub key_id: String,
    pub algorithm: SigAlgorithm,
    /// Base64-encoded raw signature (64 bytes for Ed25519).
    pub signature: String,
}

impl Manifest {
    /// Parse a manifest from JSON bytes.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let m: Manifest = serde_json::from_slice(bytes)?;
        Ok(m)
    }

    /// Whether obtaining this model is access-controlled — it's marked gated, or
    /// any of its sources needs a bearer token (e.g. a gated Hugging Face repo).
    /// Such weights are NOT auto-reshared to the public mesh (reseeding them
    /// would circumvent the access control the author set); the user can still
    /// opt a specific one in.
    pub fn is_gated(&self) -> bool {
        self.access.gated
            || self
                .artifacts
                .iter()
                .any(|a| a.sources.iter().any(|s| s.auth() == AuthPolicy::Token))
    }

    /// Whether the license class explicitly forbids public redistribution.
    pub fn is_restrictive(&self) -> bool {
        matches!(
            self.license.redistribution,
            RedistributionClass::GatedNoRedistribution | RedistributionClass::EnterprisePrivate
        )
    }

    /// Whether this model came from the *public* ecosystem — Hugging Face, the
    /// Noema mesh, or a public web source — as opposed to a private local import.
    /// Only publicly-sourced models are auto-shared by default; a file the user
    /// dragged in (publisher `local`, no public source) stays private until they
    /// opt it in.
    pub fn has_public_provenance(&self) -> bool {
        let pid = self.publisher.id.as_str();
        if pid.starts_with("hf:") {
            return true;
        }
        // A content-link (`p2p`) manifest reconstructs a model from a bare
        // content id, so its provenance is only as trustworthy as the license the
        // link vouched. Treat it as public *only* when that license actually
        // permits public redistribution — an opaque/unknown-license link must NOT
        // be auto-reseeded (it could be a gated or private model whose original
        // manifest never reached this device).
        if pid == "p2p" {
            return self.license.redistribution.allows_public_redistribution();
        }
        self.artifacts.iter().any(|a| {
            a.sources.iter().any(|s| {
                s.auth() == AuthPolicy::None
                    && matches!(
                        s.class(),
                        SourceClass::Huggingface
                            | SourceClass::HttpsMirror
                            | SourceClass::Ipfs
                            | SourceClass::Iroh
                    )
            })
        })
    }

    /// Whether Atlas seeds this model to the public mesh *by default* when
    /// worldwide sharing is on. Openly-licensed models from the public ecosystem
    /// it) always qualify. Gated/token-walled or restrictively-licensed *public*
    /// models qualify only when `include_gated` is set — the operator's "also
    /// share gated/licensed models" opt-in (default off). A *private* local
    /// import (no public provenance) never auto-shares; it stays opt-in. A
    /// per-model user override always wins, in either direction.
    pub fn auto_shareable(&self, include_gated: bool) -> bool {
        if !self.has_public_provenance() {
            return false;
        }
        include_gated || (!self.is_gated() && !self.is_restrictive())
    }

    /// Serialize to pretty JSON (for storage / display).
    pub fn to_json_pretty(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// The canonical byte representation used for signing and verification:
    /// the manifest with `signatures` removed, serialized as compact JSON with
    /// recursively sorted object keys.
    ///
    /// keys deterministically. We must NOT enable the `preserve_order` feature.)
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let mut value = serde_json::to_value(self)?;
        if let serde_json::Value::Object(map) = &mut value {
            map.remove("signatures");
        }
        serde_json::to_vec(&value).map_err(Error::from)
    }

    /// Find an artifact by its install path.
    pub fn artifact(&self, path: &str) -> Option<&Artifact> {
        self.artifacts.iter().find(|a| a.path == path)
    }

    /// Validate structural invariants that do not require network or crypto.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(Error::InvalidManifest(format!(
                "unsupported schema_version `{}` (this build understands `{}`)",
                self.schema_version, SCHEMA_VERSION
            )));
        }
        validate_manifest_id(&self.manifest_id)?;
        if self.publisher.id.trim().is_empty() {
            return Err(Error::InvalidManifest("publisher.id is empty".into()));
        }
        if self.artifacts.is_empty() {
            return Err(Error::InvalidManifest("manifest has no artifacts".into()));
        }
        for art in &self.artifacts {
            validate_artifact_path(&art.path)?;
            // At least one strong digest is required. blake3 may be absent for
            // sources that only publish sha256 ahead of time (e.g. Hugging Face);
            // the engine computes blake3 during download and keys the cache on it.
            // A multi-file model's small non-LFS sidecars carry only a git blob
            // OID — the only digest the Hub publishes for them.
            if !art.hashes.has_blake3()
                && !art.hashes.has_sha256()
                && !art.hashes.has_git_blob_sha1()
            {
                return Err(Error::InvalidManifest(format!(
                    "artifact `{}` has no blake3, sha256, or git_blob_sha1 digest",
                    art.path
                )));
            }
            if art.hashes.has_blake3() {
                validate_hex_digest("blake3", &art.hashes.blake3)?;
            }
            if art.hashes.has_sha256() {
                validate_hex_digest("sha256", &art.hashes.sha256)?;
            }
            if art.hashes.has_git_blob_sha1() {
                validate_hex_digest_len("git_blob_sha1", &art.hashes.git_blob_sha1, 40)?;
            }
            if let Some(c) = &art.chunking {
                validate_hex_digest("leaf_b3_merkle_root", &c.leaf_b3_merkle_root)?;
                if c.leaf_size == 0 {
                    return Err(Error::InvalidManifest(format!(
                        "artifact `{}` has zero leaf_size",
                        art.path
                    )));
                }
            }
        }
        if self.access.gated && !self.access.require_signed_manifest {
            return Err(Error::InvalidManifest(
                "gated model must set require_signed_manifest = true".into(),
            ));
        }
        Ok(())
    }
}

/// Reject artifact paths that could escape the install root.
pub fn validate_artifact_path(path: &str) -> Result<()> {
    if path.is_empty() {
        return Err(Error::UnsafePath("empty artifact path".into()));
    }
    if path.contains('\0') {
        return Err(Error::UnsafePath("path contains NUL".into()));
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(Error::UnsafePath(format!("absolute path `{path}`")));
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Err(Error::UnsafePath(format!("windows drive path `{path}`")));
    }
    // Reject any component that is `..`, or empty/`.`-only weirdness.
    for comp in path.split(['/', '\\']) {
        if comp == ".." {
            return Err(Error::UnsafePath(format!("parent traversal in `{path}`")));
        }
    }
    Ok(())
}

/// A manifest id is used to derive on-disk filenames (`<id>.json`) in both the
/// engine cache and the registry, so it must be a strict, traversal-proof token.
pub fn validate_manifest_id(id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(Error::InvalidManifest("manifest_id is empty".into()));
    }
    if id.len() > 200 {
        return Err(Error::InvalidManifest("manifest_id too long".into()));
    }
    if id.starts_with('.') {
        return Err(Error::UnsafePath(format!(
            "manifest_id starts with `.`: `{id}`"
        )));
    }
    // Allow only a safe filename charset: alphanumerics plus `. _ : -`.
    // (`:` is permitted because ids look like `mdl_b3_...`; it is not a path
    // separator on the platforms we target and is rejected by serve routes too.)
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
    {
        return Err(Error::UnsafePath(format!(
            "manifest_id `{id}` contains disallowed characters"
        )));
    }
    if id.contains("..") {
        return Err(Error::UnsafePath(format!(
            "manifest_id contains `..`: `{id}`"
        )));
    }
    Ok(())
}

fn validate_hex_digest(label: &str, s: &str) -> Result<()> {
    validate_hex_digest_len(label, s, 64)
}

fn validate_hex_digest_len(label: &str, s: &str, len: usize) -> Result<()> {
    if s.len() != len || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::InvalidManifest(format!(
            "{label} digest must be {len} hex chars, got `{s}`"
        )));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use crate::hash::Hashes;

    /// A representative manifest used across the crate's unit tests.
    pub fn sample_manifest() -> Manifest {
        Manifest {
            schema_version: SCHEMA_VERSION.into(),
            manifest_id: "mdl_b3_test".into(),
            publisher: Publisher {
                id: "hf:Qwen/Qwen3-8B-Instruct-GGUF".into(),
                display_name: Some("Qwen".into()),
                public_keys: vec![],
            },
            model: Model {
                name: "Qwen3 8B Instruct GGUF".into(),
                family: Some("Qwen3".into()),
                architecture: Some("transformer".into()),
                revision: Some("hf:commit:0123".into()),
                format: Some("gguf".into()),
                quantization: Some("Q4_K_M".into()),
            },
            license: License {
                spdx: "apache-2.0".into(),
                license_url: None,
                redistribution: RedistributionClass::PublicP2pAllowed,
            },
            access: Access {
                gated: false,
                require_signed_manifest: true,
                allowed_source_classes: vec![SourceClass::Huggingface, SourceClass::HttpsMirror],
            },
            artifacts: vec![Artifact {
                path: "qwen3-8b-instruct-q4_k_m.gguf".into(),
                role: "weights".into(),
                size_bytes: 4920000000,
                hashes: Hashes::new(
                    "6a4f000000000000000000000000000000000000000000000000000000000000",
                    "c2de000000000000000000000000000000000000000000000000000000000000",
                ),
                chunking: None,
                format: Some("gguf".into()),
                sources: vec![Source::HttpsMirror {
                    url: "https://mirror.example/model.gguf".into(),
                    auth: AuthPolicy::None,
                }],
            }],
            provenance: None,
            signatures: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::tests_support::sample_manifest as sample;

    #[test]
    fn roundtrip_json() {
        let m = sample();
        let json = m.to_json_pretty().unwrap();
        let back = Manifest::from_json(json.as_bytes()).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn canonical_bytes_ignore_signatures_and_are_stable() {
        let mut m = sample();
        let c1 = m.canonical_bytes().unwrap();
        m.signatures.push(Signature {
            key_id: "ed25519:abc".into(),
            algorithm: SigAlgorithm::Ed25519,
            signature: "AAAA".into(),
        });
        let c2 = m.canonical_bytes().unwrap();
        assert_eq!(c1, c2, "signatures must not affect canonical bytes");
    }

    #[test]
    fn validate_ok() {
        sample().validate().unwrap();
    }

    #[test]
    fn rejects_path_traversal() {
        assert!(validate_artifact_path("../etc/passwd").is_err());
        assert!(validate_artifact_path("/abs").is_err());
        assert!(validate_artifact_path("C:\\win").is_err());
        assert!(validate_artifact_path("ok/sub/file.gguf").is_ok());
    }

    #[test]
    fn rejects_unsafe_manifest_id() {
        assert!(validate_manifest_id("mdl_b3_abc123").is_ok());
        assert!(validate_manifest_id("hf:Org/Repo").is_err()); // slash
        assert!(validate_manifest_id("../../etc/cron").is_err());
        assert!(validate_manifest_id("..\\win").is_err());
        assert!(validate_manifest_id(".hidden").is_err());
        assert!(validate_manifest_id("a/b").is_err());
        assert!(validate_manifest_id("").is_err());
        let mut m = sample();
        m.manifest_id = "../evil".into();
        assert!(m.validate().is_err());
    }

    #[test]
    fn source_tagging() {
        let s = Source::Ipfs {
            cid: "bafy123".into(),
            retrieval: vec!["gateway".into()],
            auth: AuthPolicy::None,
        };
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains("\"type\":\"ipfs\""));
        let back: Source = serde_json::from_str(&j).unwrap();
        assert_eq!(s, back);
    }
}
