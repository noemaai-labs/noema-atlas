use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// Default Merkle leaf size (1 MiB), balancing proof size against verification granularity.
pub const DEFAULT_LEAF_SIZE: u64 = 1 << 20;

/// A set of content digests in lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hashes {
    /// Native BLAKE3 hash of the whole artifact (hex, 64 chars).
    pub blake3: String,
    /// SHA-256 hash of the whole artifact (hex, 64 chars).
    pub sha256: String,
    /// Git blob OID (hex sha1, 40 chars): `sha1("blob " + len + "\0" + bytes)`.
    /// Set for non-LFS sidecar files (the Hub's only pre-published digest); empty otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub git_blob_sha1: String,
}

impl Hashes {
    pub fn new(blake3: impl Into<String>, sha256: impl Into<String>) -> Self {
        Hashes {
            blake3: blake3.into().to_lowercase(),
            sha256: sha256.into().to_lowercase(),
            git_blob_sha1: String::new(),
        }
    }

    /// A hashes value carrying only a sha256 (sources that publish sha256 ahead of time).
    pub fn sha256_only(sha256: impl Into<String>) -> Self {
        Hashes {
            blake3: String::new(),
            sha256: sha256.into().to_lowercase(),
            git_blob_sha1: String::new(),
        }
    }

    /// A hashes value carrying only a git blob OID (the Hub's only pre-published digest
    /// for non-LFS files); blake3/sha256 are computed on download.
    pub fn git_blob_sha1_only(git_blob_sha1: impl Into<String>) -> Self {
        Hashes {
            blake3: String::new(),
            sha256: String::new(),
            git_blob_sha1: git_blob_sha1.into().to_lowercase(),
        }
    }

    pub fn has_blake3(&self) -> bool {
        !self.blake3.is_empty()
    }

    pub fn has_sha256(&self) -> bool {
        !self.sha256.is_empty()
    }

    pub fn has_git_blob_sha1(&self) -> bool {
        !self.git_blob_sha1.is_empty()
    }

    /// Strict equality of every present digest (legacy callers / full manifests).
    pub fn matches(&self, other: &Hashes) -> bool {
        self.blake3.eq_ignore_ascii_case(&other.blake3)
            && self.sha256.eq_ignore_ascii_case(&other.sha256)
            && self
                .git_blob_sha1
                .eq_ignore_ascii_case(&other.git_blob_sha1)
    }

    /// Compare the present (non-empty) digests in `self` against freshly-`computed` ones,
    /// returning the first `(label, expected, actual)` that diverged, or `None` if all matched.
    /// At least one expected digest must be present (callers guarantee this).
    pub fn mismatch_against(&self, computed: &Hashes) -> Option<(&'static str, String, String)> {
        if self.has_blake3() && !self.blake3.eq_ignore_ascii_case(&computed.blake3) {
            return Some(("blake3", self.blake3.clone(), computed.blake3.clone()));
        }
        if self.has_sha256() && !self.sha256.eq_ignore_ascii_case(&computed.sha256) {
            return Some(("sha256", self.sha256.clone(), computed.sha256.clone()));
        }
        if self.has_git_blob_sha1()
            && !self
                .git_blob_sha1
                .eq_ignore_ascii_case(&computed.git_blob_sha1)
        {
            return Some((
                "git_blob_sha1",
                self.git_blob_sha1.clone(),
                computed.git_blob_sha1.clone(),
            ));
        }
        None
    }
}

/// Streaming hasher producing BLAKE3 + SHA-256, plus the git blob OID when a
/// git-blob length is supplied up front.
pub struct DualHasher {
    blake3: blake3::Hasher,
    sha256: Sha256,
    /// Present only when built with [`DualHasher::with_git_blob_len`] (git header fed at construction).
    git: Option<Sha1>,
    len: u64,
}

impl Default for DualHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl DualHasher {
    pub fn new() -> Self {
        DualHasher {
            blake3: blake3::Hasher::new(),
            sha256: Sha256::new(),
            git: None,
            len: 0,
        }
    }

    /// Build a hasher that *also* computes the git blob OID. `content_len` must be the
    /// whole artifact's byte length (not a partial), even across a resumed download.
    pub fn with_git_blob_len(content_len: u64) -> Self {
        let mut git = Sha1::new();
        git.update(format!("blob {content_len}\0").as_bytes());
        DualHasher {
            blake3: blake3::Hasher::new(),
            sha256: Sha256::new(),
            git: Some(git),
            len: 0,
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.blake3.update(data);
        self.sha256.update(data);
        if let Some(git) = self.git.as_mut() {
            git.update(data);
        }
        self.len += data.len() as u64;
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn finalize(self) -> Hashes {
        let b3 = self.blake3.finalize();
        let s256 = self.sha256.finalize();
        Hashes {
            blake3: hex::encode(b3.as_bytes()),
            sha256: hex::encode(s256),
            git_blob_sha1: self
                .git
                .map(|g| hex::encode(g.finalize()))
                .unwrap_or_default(),
        }
    }
}

/// Compute the git blob OID of a byte slice (`sha1("blob <len>\0" + data)`), lowercase hex.
pub fn git_blob_oid(data: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", data.len()).as_bytes());
    h.update(data);
    hex::encode(h.finalize())
}

/// Hash an entire file from disk, returning dual digests and the byte length.
pub fn hash_file(path: &Path) -> Result<(Hashes, u64)> {
    hash_file_inner(path, false, &mut |_, _| {})
}

/// Like [`hash_file`], but *also* computes the git blob OID (for non-LFS sidecar files).
pub fn hash_file_with_git(path: &Path) -> Result<(Hashes, u64)> {
    hash_file_inner(path, true, &mut |_, _| {})
}

/// Like [`hash_file`], but reports `(bytes_hashed, bytes_total)` after each chunk
/// so a UI can show live progress. `bytes_total` is the file's size, taken up front.
pub fn hash_file_with_progress(
    path: &Path,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<(Hashes, u64)> {
    hash_file_inner(path, false, &mut on_progress)
}

fn hash_file_inner(
    path: &Path,
    want_git: bool,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<(Hashes, u64)> {
    let mut f = std::fs::File::open(path).map_err(|e| Error::fs(path, e))?;
    let total = f.metadata().map_err(|e| Error::fs(path, e))?.len();
    let mut hasher = if want_git {
        DualHasher::with_git_blob_len(total)
    } else {
        DualHasher::new()
    };
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    loop {
        let n = f.read(&mut buf).map_err(|e| Error::fs(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        on_progress(done, total);
    }
    let len = hasher.len();
    Ok((hasher.finalize(), len))
}

/// Hash a byte slice, returning dual digests.
pub fn hash_bytes(data: &[u8]) -> Hashes {
    let mut h = DualHasher::new();
    h.update(data);
    h.finalize()
}
const LEAF_DOMAIN: u8 = 0x00;
const NODE_DOMAIN: u8 = 0x01;

/// BLAKE3 hash of a single leaf's bytes (domain-separated to prevent
/// second-preimage collisions with interior nodes).
pub fn leaf_hash(data: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&[LEAF_DOMAIN]);
    h.update(data);
    *h.finalize().as_bytes()
}

fn node_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&[NODE_DOMAIN]);
    h.update(left);
    h.update(right);
    *h.finalize().as_bytes()
}

/// Compute the Merkle root over an ordered list of leaf hashes. A lone node at
/// the end of a level is promoted unchanged to the next level.
pub fn merkle_root(leaves: &[[u8; 32]]) -> [u8; 32] {
    if leaves.is_empty() {
        // Domain-separated hash of the empty input.
        return leaf_hash(&[]);
    }
    let mut level: Vec<[u8; 32]> = leaves.to_vec();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            if pair.len() == 2 {
                next.push(node_hash(&pair[0], &pair[1]));
            } else {
                next.push(pair[0]);
            }
        }
        level = next;
    }
    level[0]
}

/// An explicit Merkle tree over fixed-size leaves of an artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkTree {
    pub leaf_size: u64,
    /// Per-leaf BLAKE3 hashes, in order.
    pub leaves: Vec<[u8; 32]>,
}

impl ChunkTree {
    /// Build a chunk tree by reading a file from disk.
    pub fn from_file(path: &Path, leaf_size: u64) -> Result<Self> {
        Self::from_file_cancellable(path, leaf_size, None)
    }

    /// As [`from_file`], but aborts with [`Error::Cancelled`] when `cancel` flips.
    pub fn from_file_cancellable(
        path: &Path,
        leaf_size: u64,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<Self> {
        // Clamp at the allocation site too: the scratch buffer is `leaf_size` bytes,
        // so a caller passing an unbounded leaf_size must not be able to OOM us.
        let leaf_size = leaf_size.clamp(1, crate::manifest::MAX_LEAF_SIZE);
        let mut f = std::fs::File::open(path).map_err(|e| Error::fs(path, e))?;
        let mut leaves = Vec::new();
        let mut buf = vec![0u8; leaf_size as usize];
        loop {
            if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::SeqCst)) {
                return Err(Error::Cancelled);
            }
            let mut filled = 0usize;
            // Read until we fill a full leaf or hit EOF.
            while filled < buf.len() {
                let n = f.read(&mut buf[filled..]).map_err(|e| Error::fs(path, e))?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            leaves.push(leaf_hash(&buf[..filled]));
            if filled < buf.len() {
                break;
            }
        }
        Ok(ChunkTree { leaf_size, leaves })
    }

    pub fn root(&self) -> [u8; 32] {
        merkle_root(&self.leaves)
    }

    pub fn root_hex(&self) -> String {
        hex::encode(self.root())
    }

    pub fn num_leaves(&self) -> usize {
        self.leaves.len()
    }

    /// Verify a single leaf's bytes against the recorded hash for that index.
    pub fn verify_leaf(&self, index: usize, data: &[u8]) -> bool {
        match self.leaves.get(index) {
            Some(expected) => &leaf_hash(data) == expected,
            None => false,
        }
    }

    /// Serialize the leaf hashes to a compact binary blob (raw 32-byte hashes,
    /// prefixed by an 8-byte little-endian leaf_size header).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.leaves.len() * 32);
        out.extend_from_slice(&self.leaf_size.to_le_bytes());
        for leaf in &self.leaves {
            out.extend_from_slice(leaf);
        }
        out
    }

    /// Parse the binary representation produced by [`ChunkTree::to_bytes`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 8 || (bytes.len() - 8) % 32 != 0 {
            return Err(Error::other("malformed chunk tree blob"));
        }
        let leaf_size = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let mut leaves = Vec::with_capacity((bytes.len() - 8) / 32);
        for chunk in bytes[8..].chunks_exact(32) {
            let mut h = [0u8; 32];
            h.copy_from_slice(chunk);
            leaves.push(h);
        }
        Ok(ChunkTree { leaf_size, leaves })
    }
}

/// Parse a 64-char hex digest into a 32-byte array.
pub fn parse_hex32(s: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(s.trim()).map_err(|e| Error::other(format!("bad hex: {e}")))?;
    if bytes.len() != 32 {
        return Err(Error::other(format!(
            "expected 32-byte hash, got {} bytes",
            bytes.len()
        )));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dual_hash_known_vectors() {
        // BLAKE3 and SHA-256 of the empty input.
        let h = hash_bytes(b"");
        assert_eq!(
            h.sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            h.blake3,
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn dual_hash_abc() {
        let h = hash_bytes(b"abc");
        assert_eq!(
            h.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn git_blob_oid_matches_git() {
        // `git hash-object` of an empty file and of "test content\n" — the exact
        // OIDs the Hub publishes as `blobId` for non-LFS files.
        assert_eq!(
            git_blob_oid(b""),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            git_blob_oid(b"test content\n"),
            "d670460b4b4aece5915caf5c68d12f560a9fe3e4"
        );
    }

    #[test]
    fn dual_hasher_streams_git_blob_oid() {
        // Fed in arbitrary chunks, the streaming git OID equals the one-shot one.
        let data = b"test content\n";
        let mut h = DualHasher::with_git_blob_len(data.len() as u64);
        h.update(&data[..4]);
        h.update(&data[4..]);
        assert_eq!(h.finalize().git_blob_sha1, git_blob_oid(data));
    }

    #[test]
    fn merkle_root_is_stable_and_order_sensitive() {
        let a = leaf_hash(b"alpha");
        let b = leaf_hash(b"beta");
        let c = leaf_hash(b"gamma");
        let r1 = merkle_root(&[a, b, c]);
        let r2 = merkle_root(&[a, b, c]);
        assert_eq!(r1, r2);
        let r3 = merkle_root(&[c, b, a]);
        assert_ne!(r1, r3);
    }

    #[test]
    fn single_leaf_root_equals_leaf() {
        let a = leaf_hash(b"only");
        assert_eq!(merkle_root(&[a]), a);
    }

    #[test]
    fn chunk_tree_roundtrip_and_verify() {
        let data = vec![7u8; (DEFAULT_LEAF_SIZE as usize) * 2 + 123];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.bin");
        std::fs::write(&path, &data).unwrap();

        let tree = ChunkTree::from_file(&path, DEFAULT_LEAF_SIZE).unwrap();
        assert_eq!(tree.num_leaves(), 3);
        let bytes = tree.to_bytes();
        let parsed = ChunkTree::from_bytes(&bytes).unwrap();
        assert_eq!(tree, parsed);
        assert_eq!(tree.root(), parsed.root());
        let leaf0 = &data[..DEFAULT_LEAF_SIZE as usize];
        assert!(tree.verify_leaf(0, leaf0));
        let mut bad = leaf0.to_vec();
        bad[0] ^= 0xFF;
        assert!(!tree.verify_leaf(0, &bad));
    }

    #[test]
    fn from_file_cancellable_aborts_when_flag_is_set() {
        let data = vec![7u8; (DEFAULT_LEAF_SIZE as usize) * 2 + 123];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.bin");
        std::fs::write(&path, &data).unwrap();

        let cancel = std::sync::atomic::AtomicBool::new(true);
        assert!(matches!(
            ChunkTree::from_file_cancellable(&path, DEFAULT_LEAF_SIZE, Some(&cancel)),
            Err(Error::Cancelled)
        ));

        cancel.store(false, std::sync::atomic::Ordering::SeqCst);
        let tree =
            ChunkTree::from_file_cancellable(&path, DEFAULT_LEAF_SIZE, Some(&cancel)).unwrap();
        assert_eq!(tree.num_leaves(), 3);
    }
}
