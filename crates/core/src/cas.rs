use crate::error::{Error, Result};
use crate::hash::{ChunkTree, Hashes};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Sidecar metadata stored next to each committed blob.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlobMeta {
    pub blake3: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub committed_at: String,
}

/// How an install view was materialized from its backing blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    /// Copy-on-write clone (APFS, btrfs, XFS, ReFS). Cheapest + safest.
    Reflink,
    /// Hard link (same inode). Zero extra bytes, shares storage.
    Hardlink,
    /// Full byte copy (fallback when links are unsupported / cross-device).
    Copy,
}

impl LinkKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            LinkKind::Reflink => "reflink",
            LinkKind::Hardlink => "hardlink",
            LinkKind::Copy => "copy",
        }
    }
}

/// Handle to a content-addressed store rooted at a directory.
#[derive(Debug, Clone)]
pub struct Cas {
    root: PathBuf,
}

impl Cas {
    /// Open (creating if needed) a store at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let cas = Cas { root };
        for d in [
            cas.cas_dir(),
            cas.chunks_dir(),
            cas.manifests_dir(),
            cas.installs_dir(),
            cas.quarantine_dir(),
            cas.tmp_dir(),
            cas.db_dir(),
            cas.auth_dir(),
        ] {
            std::fs::create_dir_all(&d).map_err(|e| Error::fs(&d, e))?;
        }
        Ok(cas)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn cas_dir(&self) -> PathBuf {
        self.root.join("cas").join("blake3")
    }
    pub fn chunks_dir(&self) -> PathBuf {
        self.root.join("chunks").join("blake3")
    }
    pub fn manifests_dir(&self) -> PathBuf {
        self.root.join("manifests")
    }
    pub fn installs_dir(&self) -> PathBuf {
        self.root.join("installs")
    }
    pub fn quarantine_dir(&self) -> PathBuf {
        self.root.join("quarantine")
    }
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }
    pub fn db_dir(&self) -> PathBuf {
        self.root.join("db")
    }
    pub fn auth_dir(&self) -> PathBuf {
        self.root.join("auth")
    }
    pub fn db_path(&self) -> PathBuf {
        self.db_dir().join("index.sqlite")
    }
    fn shard(blake3: &str) -> Result<(String, String)> {
        if blake3.len() < 4 || !blake3.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::other(format!("invalid blake3 key `{blake3}`")));
        }
        Ok((blake3[0..2].to_lowercase(), blake3[2..4].to_lowercase()))
    }

    pub fn blob_path(&self, blake3: &str) -> Result<PathBuf> {
        let (a, b) = Self::shard(blake3)?;
        Ok(self
            .cas_dir()
            .join(a)
            .join(b)
            .join(format!("{}.blob", blake3.to_lowercase())))
    }

    pub fn blob_meta_path(&self, blake3: &str) -> Result<PathBuf> {
        let (a, b) = Self::shard(blake3)?;
        Ok(self
            .cas_dir()
            .join(a)
            .join(b)
            .join(format!("{}.meta.json", blake3.to_lowercase())))
    }

    pub fn chunk_tree_path(&self, blake3: &str) -> Result<PathBuf> {
        Ok(self
            .chunks_dir()
            .join(blake3.to_lowercase())
            .join("leaves.merkle"))
    }

    pub fn manifest_path(&self, manifest_id: &str) -> PathBuf {
        self.manifests_dir().join(format!("{manifest_id}.json"))
    }

    pub fn install_dir(&self, model_slug: &str) -> PathBuf {
        self.installs_dir().join(model_slug).join("current")
    }
    pub fn has_blob(&self, blake3: &str) -> bool {
        self.blob_path(blake3).map(|p| p.is_file()).unwrap_or(false)
    }

    pub fn blob_meta(&self, blake3: &str) -> Result<Option<BlobMeta>> {
        let p = self.blob_meta_path(blake3)?;
        if !p.is_file() {
            return Ok(None);
        }
        let bytes = std::fs::read(&p).map_err(|e| Error::fs(&p, e))?;
        Ok(Some(serde_json::from_slice(&bytes)?))
    }
    /// A fresh temp path for an in-flight download under `tmp/`.
    pub fn new_temp_path(&self, download_id: &str, suffix: &str) -> PathBuf {
        self.tmp_dir().join(format!("{download_id}.{suffix}.part"))
    }

    /// Every in-flight `.part` temp for a download id. The trailing `.` in the
    /// `{download_id}.` match prefix stops a longer id sharing the leading hex.
    fn download_temp_paths(&self, download_id: &str) -> Vec<PathBuf> {
        let prefix = format!("{download_id}.");
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.tmp_dir()) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&prefix) && name.ends_with(".part") {
                    out.push(entry.path());
                }
            }
        }
        out
    }

    /// Whether a resumable `.part` temp exists for a download id.
    pub fn download_temp_exists(&self, download_id: &str) -> bool {
        !self.download_temp_paths(download_id).is_empty()
    }

    /// Remove every `.part` temp for a download id (a discarded/orphaned
    /// download). Returns how many were removed; best-effort, never an error.
    pub fn remove_download_temps(&self, download_id: &str) -> usize {
        self.download_temp_paths(download_id)
            .into_iter()
            .filter(|p| std::fs::remove_file(p).is_ok())
            .count()
    }

    /// Commit a finished, *already verified* temp file into the CAS atomically.
    /// If the blob already exists, the temp file is removed and the existing
    /// metadata is returned (this is the dedup path).
    pub fn commit_blob(&self, tmp_path: &Path, hashes: &Hashes, size: u64) -> Result<BlobMeta> {
        let blob_path = self.blob_path(&hashes.blake3)?;
        let meta = BlobMeta {
            blake3: hashes.blake3.clone(),
            sha256: hashes.sha256.clone(),
            size_bytes: size,
            committed_at: crate::util::now_rfc3339(),
        };

        if blob_path.is_file() {
            // Already have it — discard the temp copy (dedup). Return the
            // existing sidecar so the result is idempotent (same committed_at).
            let _ = std::fs::remove_file(tmp_path);
            if let Some(existing) = self.blob_meta(&hashes.blake3)? {
                return Ok(existing);
            }
            self.write_meta(&meta)?;
            return Ok(meta);
        }

        if let Some(parent) = blob_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::fs(parent, e))?;
        }
        // Atomic publish: rename within the same filesystem (tmp is under root).
        match std::fs::rename(tmp_path, &blob_path) {
            Ok(()) => {}
            Err(_) => {
                // Cross-device or other: copy to a sibling temp in the destination
                // directory, then rename it into place — so a crash mid-copy never
                // leaves a truncated blob at the content-addressed path.
                let staging = blob_path.with_extension("blob.incoming");
                std::fs::copy(tmp_path, &staging).map_err(|e| Error::fs(&staging, e))?;
                std::fs::rename(&staging, &blob_path).map_err(|e| Error::fs(&blob_path, e))?;
                let _ = std::fs::remove_file(tmp_path);
            }
        }
        self.make_readonly(&blob_path);
        self.write_meta(&meta)?;
        Ok(meta)
    }

    /// Import an existing on-disk file into the CAS by copying it into tmp and
    /// committing under the given (already computed) hashes.
    pub fn import_file(&self, src: &Path, hashes: &Hashes, size: u64) -> Result<BlobMeta> {
        if self.has_blob(&hashes.blake3) {
            let meta = BlobMeta {
                blake3: hashes.blake3.clone(),
                sha256: hashes.sha256.clone(),
                size_bytes: size,
                committed_at: crate::util::now_rfc3339(),
            };
            self.write_meta(&meta)?;
            return Ok(meta);
        }
        let tmp = self.new_temp_path("import", &hashes.blake3[..8.min(hashes.blake3.len())]);
        if reflink_copy::reflink_or_copy(src, &tmp).is_err() {
            std::fs::copy(src, &tmp).map_err(|e| Error::fs(&tmp, e))?;
        }
        self.commit_blob(&tmp, hashes, size)
    }

    fn write_meta(&self, meta: &BlobMeta) -> Result<()> {
        let p = self.blob_meta_path(&meta.blake3)?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::fs(parent, e))?;
        }
        let json = serde_json::to_vec_pretty(meta)?;
        std::fs::write(&p, json).map_err(|e| Error::fs(&p, e))
    }

    /// Persist a leaf-hash chunk tree sidecar for streaming verification reuse.
    pub fn store_chunk_tree(&self, blake3: &str, tree: &ChunkTree) -> Result<()> {
        let p = self.chunk_tree_path(blake3)?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::fs(parent, e))?;
        }
        std::fs::write(&p, tree.to_bytes()).map_err(|e| Error::fs(&p, e))
    }

    pub fn load_chunk_tree(&self, blake3: &str) -> Result<Option<ChunkTree>> {
        let p = self.chunk_tree_path(blake3)?;
        if !p.is_file() {
            return Ok(None);
        }
        let bytes = std::fs::read(&p).map_err(|e| Error::fs(&p, e))?;
        Ok(Some(ChunkTree::from_bytes(&bytes)?))
    }

    /// Materialize an install view at `dest` pointing at the blob's content.
    /// Tries reflink, then hardlink, then a full copy.
    pub fn materialize(&self, blake3: &str, dest: &Path) -> Result<LinkKind> {
        let blob = self.blob_path(blake3)?;
        if !blob.is_file() {
            return Err(Error::other(format!("blob {blake3} not present in cache")));
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::fs(parent, e))?;
        }
        if dest.exists() {
            // The blob (and thus a prior hardlinked dest) is read-only; on Windows
            // an unlink of a read-only file is denied, so clear the bit first.
            self.make_writable(dest);
            std::fs::remove_file(dest).map_err(|e| Error::fs(dest, e))?;
        }
        if reflink_copy::reflink(&blob, dest).is_ok() {
            self.make_writable(dest);
            return Ok(LinkKind::Reflink);
        }
        if std::fs::hard_link(&blob, dest).is_ok() {
            return Ok(LinkKind::Hardlink);
        }
        std::fs::copy(&blob, dest).map_err(|e| Error::fs(dest, e))?;
        self.make_writable(dest);
        Ok(LinkKind::Copy)
    }

    /// Allocate a quarantine directory for a failed download. A monotonic
    /// process-wide counter is appended so two quarantines in the same
    /// millisecond do not collide into a shared directory.
    pub fn new_quarantine_dir(&self, download_id: &str) -> Result<PathBuf> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = self.quarantine_dir().join(format!(
            "{download_id}-{}-{n}",
            crate::util::now_unix_millis()
        ));
        std::fs::create_dir_all(&dir).map_err(|e| Error::fs(&dir, e))?;
        Ok(dir)
    }

    /// Move bytes into quarantine rather than deleting (forensics / debugging).
    pub fn quarantine(&self, download_id: &str, src: &Path, label: &str) -> Result<PathBuf> {
        let dir = self.new_quarantine_dir(download_id)?;
        let dest = dir.join(label);
        if src.exists() && std::fs::rename(src, &dest).is_err() {
            std::fs::copy(src, &dest).map_err(|e| Error::fs(&dest, e))?;
            let _ = std::fs::remove_file(src);
        }
        Ok(dest)
    }

    /// Delete a blob and its sidecars (cache eviction).
    pub fn remove_blob(&self, blake3: &str) -> Result<()> {
        for p in [self.blob_path(blake3)?, self.blob_meta_path(blake3)?] {
            if p.exists() {
                // Blobs are read-only; clear the bit before unlink on Windows.
                self.make_writable(&p);
                std::fs::remove_file(&p).map_err(|e| Error::fs(&p, e))?;
            }
        }
        let chunk = self.chunk_tree_path(blake3)?;
        if let Some(dir) = chunk.parent() {
            if dir.exists() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
        Ok(())
    }

    /// Total bytes currently held in the blob store.
    pub fn total_blob_bytes(&self) -> Result<u64> {
        let mut total = 0u64;
        let cas = self.cas_dir();
        if !cas.exists() {
            return Ok(0);
        }
        for entry in walk_files(&cas) {
            if entry.extension().and_then(|e| e.to_str()) == Some("blob") {
                if let Ok(md) = std::fs::metadata(&entry) {
                    total += md.len();
                }
            }
        }
        Ok(total)
    }

    fn make_readonly(&self, path: &Path) {
        if let Ok(md) = std::fs::metadata(path) {
            let mut perms = md.permissions();
            perms.set_readonly(true);
            let _ = std::fs::set_permissions(path, perms);
        }
    }

    fn make_writable(&self, path: &Path) {
        if let Ok(md) = std::fs::metadata(path) {
            let mut perms = md.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
}

/// Recursively collect regular files under a directory (small, sync helper).
fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hash_bytes;

    #[test]
    fn commit_and_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();

        let data = b"hello weights";
        let hashes = hash_bytes(data);
        let tmp1 = cas.new_temp_path("dl1", "x");
        std::fs::write(&tmp1, data).unwrap();
        let meta1 = cas.commit_blob(&tmp1, &hashes, data.len() as u64).unwrap();
        assert!(cas.has_blob(&hashes.blake3));
        assert_eq!(meta1.size_bytes, data.len() as u64);
        assert!(!tmp1.exists(), "temp consumed by rename");

        // Second commit of identical content dedups (temp removed, no new blob).
        let tmp2 = cas.new_temp_path("dl2", "x");
        std::fs::write(&tmp2, data).unwrap();
        let meta2 = cas.commit_blob(&tmp2, &hashes, data.len() as u64).unwrap();
        assert_eq!(meta1, meta2);
        assert!(!tmp2.exists());
        assert_eq!(cas.total_blob_bytes().unwrap(), data.len() as u64);
    }

    #[test]
    fn materialize_links_content() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        let data = vec![42u8; 4096];
        let hashes = hash_bytes(&data);
        let tmp = cas.new_temp_path("dl", "x");
        std::fs::write(&tmp, &data).unwrap();
        cas.commit_blob(&tmp, &hashes, data.len() as u64).unwrap();

        let dest = dir.path().join("install").join("model.gguf");
        let kind = cas.materialize(&hashes.blake3, &dest).unwrap();
        assert!(matches!(
            kind,
            LinkKind::Reflink | LinkKind::Hardlink | LinkKind::Copy
        ));
        assert_eq!(std::fs::read(&dest).unwrap(), data);
    }

    #[test]
    fn import_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        let src = dir.path().join("orig.gguf");
        let data = vec![7u8; 2048];
        std::fs::write(&src, &data).unwrap();
        let hashes = hash_bytes(&data);
        let meta = cas.import_file(&src, &hashes, data.len() as u64).unwrap();
        assert!(cas.has_blob(&meta.blake3));
    }

    #[test]
    fn rejects_bad_key() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        assert!(cas.blob_path("xyz").is_err());
        assert!(cas.blob_path("zz").is_err());
    }
}
