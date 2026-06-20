use noema_core::engine::{Engine, EngineConfig};
use std::sync::Arc;

uniffi::setup_scaffolding!();

/// Errors surfaced across the FFI boundary.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FfiError {
    #[error("{message}")]
    Engine { message: String },
}

impl From<noema_core::Error> for FfiError {
    fn from(e: noema_core::Error) -> Self {
        FfiError::Engine {
            message: e.to_string(),
        }
    }
}

/// A model summary, mirroring `Db::ManifestSummary` in FFI-friendly form.
#[derive(uniffi::Record)]
pub struct ManifestInfo {
    pub manifest_id: String,
    pub model_name: String,
    pub license_spdx: String,
    pub redistribution: String,
    pub gated: bool,
    pub signed: bool,
}

/// Outcome of importing a manifest.
#[derive(uniffi::Record)]
pub struct ImportInfo {
    pub manifest_id: String,
    pub signed: bool,
    pub policy_allowed: bool,
    pub policy_reason: String,
    pub warnings: Vec<String>,
}

/// Per-artifact download result.
#[derive(uniffi::Record)]
pub struct ArtifactInfo {
    pub artifact_path: String,
    pub blake3: String,
    pub from_cache: bool,
    pub source_id: Option<String>,
    pub size_bytes: u64,
}

/// Result of downloading a manifest.
#[derive(uniffi::Record)]
pub struct DownloadInfo {
    pub manifest_id: String,
    pub artifacts: Vec<ArtifactInfo>,
}

/// A cached blob.
#[derive(uniffi::Record)]
pub struct CacheInfo {
    pub blake3: String,
    pub size_bytes: u64,
    pub state: String,
}

/// The engine handle exposed to the platform layer.
#[derive(uniffi::Object)]
pub struct NoemaEngine {
    inner: Arc<Engine>,
    rt: tokio::runtime::Runtime,
}

#[uniffi::export]
impl NoemaEngine {
    /// Open (creating if needed) a store rooted at `root`.
    #[uniffi::constructor]
    pub fn new(root: String) -> Result<Arc<Self>, FfiError> {
        let engine = Engine::open(EngineConfig::new(root))?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| FfiError::Engine {
                message: format!("runtime: {e}"),
            })?;
        Ok(Arc::new(NoemaEngine {
            inner: Arc::new(engine),
            rt,
        }))
    }

    /// Import a manifest from raw JSON bytes.
    pub fn import_manifest(&self, json: Vec<u8>) -> Result<ImportInfo, FfiError> {
        let r = self.inner.import_manifest(&json)?;
        Ok(ImportInfo {
            manifest_id: r.manifest_id,
            signed: r.report.is_signed(),
            policy_allowed: r.policy.allowed,
            policy_reason: r.policy.reason,
            warnings: r.policy.warnings,
        })
    }

    /// List imported manifests.
    pub fn list_manifests(&self) -> Result<Vec<ManifestInfo>, FfiError> {
        Ok(self
            .inner
            .list_manifests()?
            .into_iter()
            .map(|m| ManifestInfo {
                manifest_id: m.manifest_id,
                model_name: m.model_name,
                license_spdx: m.license_spdx,
                redistribution: m.redistribution.as_str().to_string(),
                gated: m.gated,
                signed: m.signed,
            })
            .collect())
    }

    /// Download and verify every artifact of a manifest. **Blocking** — call off
    /// the platform main thread.
    pub fn download(&self, manifest_id: String) -> Result<DownloadInfo, FfiError> {
        let out = self.rt.block_on(self.inner.download(&manifest_id, None))?;
        Ok(DownloadInfo {
            manifest_id: out.manifest_id,
            artifacts: out
                .artifacts
                .into_iter()
                .map(|a| ArtifactInfo {
                    artifact_path: a.artifact_path,
                    blake3: a.blake3,
                    from_cache: a.from_cache,
                    source_id: a.source_id,
                    size_bytes: a.size_bytes,
                })
                .collect(),
        })
    }

    /// Pause the in-flight [`download`](Self::download): stop promptly but keep
    /// the partial on disk so a later `download` of the same manifest resumes.
    /// No-op if nothing is downloading. Cheap and thread-safe — call it from the
    /// UI thread while `download` blocks on a worker thread.
    pub fn pause(&self) {
        self.inner.request_pause();
    }

    /// Stop the in-flight [`download`](Self::download) and discard its progress:
    /// the partial temp and the download row are dropped so the next attempt
    /// starts clean. No-op if nothing is downloading. Cheap and thread-safe.
    pub fn stop(&self) {
        self.inner.request_stop();
    }

    /// Materialize an install of a cached manifest into `target_dir`.
    pub fn materialize(&self, manifest_id: String, target_dir: String) -> Result<u32, FfiError> {
        let views = self
            .inner
            .materialize_install(&manifest_id, std::path::Path::new(&target_dir))?;
        Ok(views.len() as u32)
    }

    /// Import a local file as a manifest artifact (avoids re-downloading).
    pub fn import_file(
        &self,
        manifest_id: String,
        artifact_path: String,
        file_path: String,
    ) -> Result<ArtifactInfo, FfiError> {
        let a = self.inner.import_artifact_file(
            &manifest_id,
            &artifact_path,
            std::path::Path::new(&file_path),
        )?;
        Ok(ArtifactInfo {
            artifact_path: a.artifact_path,
            blake3: a.blake3,
            from_cache: a.from_cache,
            source_id: a.source_id,
            size_bytes: a.size_bytes,
        })
    }

    /// List cached blobs.
    pub fn list_cache(&self) -> Result<Vec<CacheInfo>, FfiError> {
        Ok(self
            .inner
            .list_cache()?
            .into_iter()
            .map(|b| CacheInfo {
                blake3: b.blake3,
                size_bytes: b.size_bytes,
                state: b.state,
            })
            .collect())
    }
}
