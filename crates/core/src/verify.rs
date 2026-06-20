use crate::error::{Error, Result};
use crate::hash::{ChunkTree, DualHasher, Hashes};
use std::path::Path;

/// Verifies a byte stream incrementally against expected digests and, when a
/// chunk tree is available, against per-leaf hashes the moment each leaf is
/// complete — so a poisoning source is caught after one leaf, not after GBs.
pub struct StreamingVerifier {
    dual: DualHasher,
    chunk_tree: Option<ChunkTree>,
    leaf_buf: Vec<u8>,
    leaf_index: usize,
    expected: Hashes,
    expected_size: u64,
    what: String,
}

impl StreamingVerifier {
    pub fn new(
        expected: Hashes,
        expected_size: u64,
        chunk_tree: Option<ChunkTree>,
        what: impl Into<String>,
    ) -> Self {
        // Compute the git blob OID alongside blake3/sha256 only when the manifest
        // declares one (sidecar files) and we know the full length up front — the
        // OID hashes a `blob <len>\0` header, so the length is required.
        let dual = if expected.has_git_blob_sha1() && expected_size > 0 {
            DualHasher::with_git_blob_len(expected_size)
        } else {
            DualHasher::new()
        };
        StreamingVerifier {
            dual,
            chunk_tree,
            leaf_buf: Vec::new(),
            leaf_index: 0,
            expected,
            expected_size,
            what: what.into(),
        }
    }

    pub fn bytes_seen(&self) -> u64 {
        self.dual.len()
    }

    /// Feed a contiguous chunk of bytes (must be supplied in order from offset 0).
    pub fn feed(&mut self, mut data: &[u8]) -> Result<()> {
        self.dual.update(data);
        // Split borrows of distinct fields so we can mutate buffer/index while
        // holding an immutable borrow of the chunk tree.
        let Self {
            chunk_tree,
            leaf_buf,
            leaf_index,
            what,
            ..
        } = self;
        if let Some(tree) = chunk_tree.as_ref() {
            let leaf_size = tree.leaf_size as usize;
            while !data.is_empty() {
                let need = leaf_size - leaf_buf.len();
                let take = need.min(data.len());
                leaf_buf.extend_from_slice(&data[..take]);
                data = &data[take..];
                if leaf_buf.len() == leaf_size {
                    verify_leaf(tree, leaf_index, leaf_buf, what)?;
                }
            }
        }
        Ok(())
    }

    /// Finish: verify the trailing partial leaf, total size, and full-file
    /// digests. Returns the computed digests on success.
    pub fn finish(mut self) -> Result<Hashes> {
        if let Some(tree) = self.chunk_tree.take() {
            if !self.leaf_buf.is_empty() {
                verify_leaf(&tree, &mut self.leaf_index, &mut self.leaf_buf, &self.what)?;
            }
            if self.leaf_index != tree.num_leaves() {
                return Err(Error::HashMismatch {
                    what: format!("{} leaf count", self.what),
                    expected: tree.num_leaves().to_string(),
                    actual: self.leaf_index.to_string(),
                });
            }
        }
        let size = self.dual.len();
        // `expected_size == 0` means the size was unknown up front (e.g. a bare
        // Content-ID add); the full-file digests below are the real integrity
        // guarantee, so only enforce the size when it was actually declared.
        if self.expected_size != 0 && size != self.expected_size {
            return Err(Error::SizeMismatch {
                what: self.what.clone(),
                expected: self.expected_size,
                actual: size,
            });
        }
        let got = self.dual.finalize();
        // Compare only the digests the manifest actually declared (HF-sourced
        // artifacts carry sha256 but no blake3 until first download).
        if let Some((label, expected, actual)) = self.expected.mismatch_against(&got) {
            return Err(Error::HashMismatch {
                what: format!("{} ({label})", self.what),
                expected,
                actual,
            });
        }
        Ok(got)
    }
}

/// Verify the buffered leaf against the tree, then advance index and clear it.
fn verify_leaf(
    tree: &ChunkTree,
    leaf_index: &mut usize,
    leaf_buf: &mut Vec<u8>,
    what: &str,
) -> Result<()> {
    if !tree.verify_leaf(*leaf_index, leaf_buf) {
        let expected = tree
            .leaves
            .get(*leaf_index)
            .map(hex::encode)
            .unwrap_or_else(|| "<out-of-range>".into());
        return Err(Error::HashMismatch {
            what: format!("{} leaf #{}", what, *leaf_index),
            expected,
            actual: hex::encode(crate::hash::leaf_hash(leaf_buf)),
        });
    }
    *leaf_index += 1;
    leaf_buf.clear();
    Ok(())
}

/// Verify an entire file on disk against expected digests + size.
pub fn verify_file(
    path: &Path,
    expected: &Hashes,
    expected_size: u64,
    what: &str,
) -> Result<Hashes> {
    let (got, size) = if expected.has_git_blob_sha1() {
        crate::hash::hash_file_with_git(path)?
    } else {
        crate::hash::hash_file(path)?
    };
    // `expected_size == 0` means the size was unknown up front; rely on digests.
    if expected_size != 0 && size != expected_size {
        return Err(Error::SizeMismatch {
            what: what.into(),
            expected: expected_size,
            actual: size,
        });
    }
    if let Some((label, exp, act)) = expected.mismatch_against(&got) {
        return Err(Error::HashMismatch {
            what: format!("{what} ({label})"),
            expected: exp,
            actual: act,
        });
    }
    Ok(got)
}
/// How risky a file is to handle, based on its type. Model weight downloaders
/// are a juicy target: pickle-based formats can execute arbitrary code on load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSafety {
    /// Pure data formats we allow by default (still header-validated).
    Safe,
    /// Ambiguous or unknown; allowed but surfaced as a warning.
    Warn,
    /// Known-dangerous; blocked unless the user sets a high-trust override.
    Blocked,
}

/// Classify a file by name/extension. Returns the safety level and a reason.
pub fn classify_file_safety(name: &str) -> (FileSafety, String) {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");

    // Executables / scripts / native libraries: hard block.
    const BLOCKED: &[&str] = &[
        "pkl", "pickle", "pt", "pth", "ckpt", // pickle-backed (arbitrary code on load)
        "exe", "dll", "so", "dylib", "msi", "scr", "com", // native exec/libs
        "sh", "bash", "zsh", "bat", "cmd", "ps1", "py", "pyc", "js", "rb", "pl", // scripts
    ];
    if BLOCKED.contains(&ext) {
        return (
            FileSafety::Blocked,
            format!("`.{ext}` can execute code or load unsafely; blocked by default"),
        );
    }

    // Pure-data model formats: allowed (and header-validated separately).
    const SAFE: &[&str] = &[
        "gguf",
        "safetensors",
        "json",
        "txt",
        "md",
        "model", // sentencepiece
        "vocab",
        "merges",
        "tiktoken",
        "yaml",
        "yml",
        "toml",
    ];
    if SAFE.contains(&ext) {
        return (FileSafety::Safe, format!("`.{ext}` is a pure-data format"));
    }

    // `.bin` is ambiguous (could be a pickle `pytorch_model.bin`): warn.
    if ext == "bin" {
        return (
            FileSafety::Warn,
            "`.bin` is ambiguous and may be a pickle; treat with caution".into(),
        );
    }

    (
        FileSafety::Warn,
        format!("unknown extension `.{ext}`; treat with caution"),
    )
}
/// Validate a downloaded file's header matches its declared format, when we
/// know how to parse it. Unknown formats are accepted (no-op).
pub fn validate_format_header(format: Option<&str>, path: &Path) -> Result<()> {
    let Some(fmt) = format else {
        return Ok(());
    };
    match fmt.to_ascii_lowercase().as_str() {
        "gguf" => validate_gguf(path),
        "safetensors" => validate_safetensors(path),
        _ => Ok(()),
    }
}

/// GGUF: 4-byte magic `GGUF`, then a little-endian u32 version (1, 2, or 3).
pub fn validate_gguf(path: &Path) -> Result<()> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| Error::fs(path, e))?;
    let mut hdr = [0u8; 8];
    f.read_exact(&mut hdr).map_err(|_| Error::FormatInvalid {
        format: "gguf".into(),
        reason: "file shorter than 8-byte header".into(),
    })?;
    if &hdr[0..4] != b"GGUF" {
        return Err(Error::FormatInvalid {
            format: "gguf".into(),
            reason: format!("bad magic: {:02x?}", &hdr[0..4]),
        });
    }
    let version = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
    if !(1..=3).contains(&version) {
        return Err(Error::FormatInvalid {
            format: "gguf".into(),
            reason: format!("unexpected version {version}"),
        });
    }
    Ok(())
}

/// Safetensors: 8-byte little-endian header length N, then N bytes of JSON.
pub fn validate_safetensors(path: &Path) -> Result<()> {
    use std::io::Read;
    let md = std::fs::metadata(path).map_err(|e| Error::fs(path, e))?;
    let file_len = md.len();
    let mut f = std::fs::File::open(path).map_err(|e| Error::fs(path, e))?;
    let mut len_bytes = [0u8; 8];
    f.read_exact(&mut len_bytes)
        .map_err(|_| Error::FormatInvalid {
            format: "safetensors".into(),
            reason: "file shorter than 8-byte length prefix".into(),
        })?;
    let header_len = u64::from_le_bytes(len_bytes);
    if header_len == 0 || header_len.saturating_add(8) > file_len {
        return Err(Error::FormatInvalid {
            format: "safetensors".into(),
            reason: format!("implausible header length {header_len} for {file_len}-byte file"),
        });
    }
    // Cap how much header JSON we'll parse to avoid a huge allocation on a
    // hostile file (genuine safetensors headers are small relative to weights).
    let to_read = header_len.min(64 * 1024 * 1024) as usize;
    let mut buf = vec![0u8; to_read];
    f.read_exact(&mut buf).map_err(|e| Error::fs(path, e))?;
    serde_json::from_slice::<serde_json::Value>(&buf).map_err(|e| Error::FormatInvalid {
        format: "safetensors".into(),
        reason: format!("header is not valid JSON: {e}"),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::{hash_bytes, ChunkTree, DEFAULT_LEAF_SIZE};

    #[test]
    fn streaming_verifier_accepts_good_stream() {
        let data = vec![3u8; (DEFAULT_LEAF_SIZE as usize) + 17];
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("b");
        std::fs::write(&p, &data).unwrap();
        let tree = ChunkTree::from_file(&p, DEFAULT_LEAF_SIZE).unwrap();
        let expected = hash_bytes(&data);

        let mut v = StreamingVerifier::new(expected, data.len() as u64, Some(tree), "art");
        // Feed in awkward 7000-byte chunks to exercise leaf buffering.
        for chunk in data.chunks(7000) {
            v.feed(chunk).unwrap();
        }
        let got = v.finish().unwrap();
        assert_eq!(got, hash_bytes(&data));
    }

    #[test]
    fn streaming_verifier_rejects_corrupted_leaf() {
        let data = vec![5u8; (DEFAULT_LEAF_SIZE as usize) * 2];
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("b");
        std::fs::write(&p, &data).unwrap();
        let tree = ChunkTree::from_file(&p, DEFAULT_LEAF_SIZE).unwrap();
        let expected = hash_bytes(&data);

        let mut corrupt = data.clone();
        corrupt[10] ^= 0xFF; // poison the first leaf
        let mut v = StreamingVerifier::new(expected, data.len() as u64, Some(tree), "art");
        let err = v.feed(&corrupt).unwrap_err();
        assert!(matches!(err, Error::HashMismatch { .. }));
    }

    #[test]
    fn streaming_verifier_accepts_unknown_size_sha256_only() {
        // A bare Content-ID add: size unknown (0), only the sha256 is declared.
        let data = b"peer-shared model bytes".to_vec();
        let full = hash_bytes(&data);
        let expected = crate::hash::Hashes::sha256_only(&full.sha256);
        let mut v = StreamingVerifier::new(expected, 0, None, "art");
        v.feed(&data).unwrap();
        let got = v.finish().unwrap();
        assert_eq!(got.sha256, full.sha256);
    }

    #[test]
    fn streaming_verifier_still_rejects_wrong_hash_when_size_unknown() {
        // Relaxing the size check for size==0 must NOT weaken digest integrity.
        let data = b"the real bytes";
        let wrong = crate::hash::Hashes::sha256_only(&hash_bytes(b"different").sha256);
        let mut v = StreamingVerifier::new(wrong, 0, None, "art");
        v.feed(data).unwrap();
        assert!(matches!(
            v.finish().unwrap_err(),
            Error::HashMismatch { .. }
        ));
    }

    #[test]
    fn streaming_verifier_rejects_wrong_full_hash_without_tree() {
        let data = b"the real bytes";
        let wrong = hash_bytes(b"different");
        let mut v = StreamingVerifier::new(wrong, data.len() as u64, None, "art");
        v.feed(data).unwrap();
        assert!(matches!(
            v.finish().unwrap_err(),
            Error::HashMismatch { .. }
        ));
    }

    #[test]
    fn streaming_verifier_checks_git_blob_oid_for_sidecars() {
        // A config-style sidecar: only the git blob OID is published up front.
        let data = b"{\"model_type\": \"qwen2\"}\n".to_vec();
        let expected = crate::hash::Hashes::git_blob_sha1_only(crate::hash::git_blob_oid(&data));
        let mut v =
            StreamingVerifier::new(expected.clone(), data.len() as u64, None, "config.json");
        for chunk in data.chunks(5) {
            v.feed(chunk).unwrap();
        }
        let got = v.finish().unwrap();
        assert_eq!(got.git_blob_sha1, expected.git_blob_sha1);
        let mut tampered = data.clone();
        tampered.push(b' ');
        let mut v = StreamingVerifier::new(expected, tampered.len() as u64, None, "config.json");
        v.feed(&tampered).unwrap();
        assert!(matches!(
            v.finish().unwrap_err(),
            Error::HashMismatch { .. }
        ));
    }

    #[test]
    fn safety_classification() {
        assert_eq!(classify_file_safety("model.gguf").0, FileSafety::Safe);
        assert_eq!(classify_file_safety("x.safetensors").0, FileSafety::Safe);
        assert_eq!(classify_file_safety("evil.pkl").0, FileSafety::Blocked);
        assert_eq!(
            classify_file_safety("pytorch_model.bin").0,
            FileSafety::Warn
        );
        assert_eq!(classify_file_safety("run.sh").0, FileSafety::Blocked);
        assert_eq!(classify_file_safety("weights.pt").0, FileSafety::Blocked);
    }

    #[test]
    fn gguf_header_validation() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.gguf");
        let mut bytes = b"GGUF".to_vec();
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 16]);
        std::fs::write(&good, &bytes).unwrap();
        assert!(validate_gguf(&good).is_ok());

        let bad = dir.path().join("bad.gguf");
        std::fs::write(&bad, b"NOPExxxx").unwrap();
        assert!(validate_gguf(&bad).is_err());
    }

    #[test]
    fn safetensors_header_validation() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("m.safetensors");
        let header = br#"{"__metadata__":{"k":"v"}}"#;
        let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
        bytes.extend_from_slice(header);
        bytes.extend_from_slice(&[0u8; 32]); // fake tensor data
        std::fs::write(&p, &bytes).unwrap();
        assert!(validate_safetensors(&p).is_ok());
    }
}
