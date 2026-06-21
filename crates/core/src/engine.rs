use crate::cas::{BlobMeta, Cas, LinkKind};
use crate::db::{CacheBlobRow, Db, InstallRow, ManifestSummary, SourceHealth};
use crate::error::{Error, Result, TransportErrorKind};
use crate::hash::{ChunkTree, Hashes};
use crate::manifest::{Artifact, Manifest, Source, SourceClass};
use crate::planner::{plan_artifact, Plan};
use crate::platform::PlatformProfile;
use crate::policy::{PolicyConfig, PolicyDecision, PolicyEngine};
use crate::secret::{self, SecretStore};
use crate::sign::{verify_manifest, VerificationReport};
use crate::transport::{
    service_for_source, AuthRequirement, ByteRange, FetchCtx, TransportConfig, Transports,
};
use crate::verify::{validate_format_header, StreamingVerifier};
use futures_util::StreamExt;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

/// Minimum artifact size before a multi-connection (segmented) download is worth
/// the extra connections; smaller files stream single-connection.
const SEGMENT_MIN_BYTES: u64 = 32 * 1024 * 1024; // 32 MiB

/// Whether a source class is range-capable for segmented downloads.
fn http_range_class(class: SourceClass) -> bool {
    matches!(class, SourceClass::Huggingface | SourceClass::HttpsMirror)
}

/// A live-adjustable global download rate limit (bytes/sec; 0 = unlimited).
/// Cloneable handle so the UI's Settings can change the cap on the fly.
#[derive(Clone, Default)]
pub struct RateLimit {
    bps: Arc<std::sync::atomic::AtomicU64>,
}

impl RateLimit {
    pub fn unlimited() -> Self {
        RateLimit::default()
    }
    pub fn set_bps(&self, bps: u64) {
        self.bps.store(bps, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn bps(&self) -> u64 {
        self.bps.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Pace a transfer: call after each chunk with the running window state.
    async fn pace(&self, window_start: &mut Instant, window_bytes: &mut u64, n: usize) {
        let limit = self.bps();
        if limit == 0 {
            return;
        }
        *window_bytes += n as u64;
        let expected = std::time::Duration::from_secs_f64(*window_bytes as f64 / limit as f64);
        let elapsed = window_start.elapsed();
        if expected > elapsed {
            tokio::time::sleep(expected - elapsed).await;
        }
        if window_start.elapsed() >= Duration::from_secs(1) {
            *window_start = Instant::now();
            *window_bytes = 0;
        }
    }
}

/// Engine configuration.
#[derive(Clone)]
pub struct EngineConfig {
    pub root: PathBuf,
    pub platform: PlatformProfile,
    pub policy: PolicyConfig,
    pub transport: TransportConfig,
    /// Per-source attempt cap before moving on (transient retries).
    pub max_attempts_per_source: u32,
    /// Global download speed cap (shared, live-adjustable).
    pub rate_limit: RateLimit,
    /// Worldwide content tracker base URL (for P2P discovery beyond the LAN).
    pub tracker_url: Option<String>,
    /// Max parallel connections for range-capable HTTP downloads above the size threshold.
    /// Ignored when a speed cap is set.
    pub max_download_connections: usize,
    /// Opt-in to also auto-share gated/token-walled/restrictively-licensed models.
    /// A per-model override still wins.
    pub share_gated: bool,
}

impl EngineConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let transport = TransportConfig {
            iroh_store_dir: root.join("iroh-store"),
            ..TransportConfig::default()
        };
        EngineConfig {
            root,
            platform: PlatformProfile::detect(),
            policy: PolicyConfig::default(),
            transport,
            max_attempts_per_source: 2,
            rate_limit: RateLimit::unlimited(),
            tracker_url: None,
            max_download_connections: 4,
            share_gated: false,
        }
    }
}

/// Progress callback payload.
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub manifest_id: String,
    pub artifact_path: String,
    pub source_id: Option<String>,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub phase: &'static str,
    /// Human-readable reason for leaving a source, emitted only at source
    /// boundaries so UIs can narrate failover without per-chunk cost.
    pub failover_reason: Option<String>,
    /// The verified byte offset this source attempt effectively started from.
    /// `Some(n > 0)` means a resumed partial was reused.
    pub effective_start: Option<u64>,
}

/// A callback the engine invokes as a download progresses.
pub type Progress = Arc<dyn Fn(DownloadProgress) + Send + Sync>;

/// Outcome of importing a manifest.
#[derive(Debug, Clone)]
pub struct ImportResult {
    pub manifest_id: String,
    pub report: VerificationReport,
    pub policy: PolicyDecision,
}

/// Per-artifact result of a download.
#[derive(Debug, Clone)]
pub struct ArtifactOutcome {
    pub artifact_path: String,
    pub blake3: String,
    pub from_cache: bool,
    pub source_id: Option<String>,
    pub size_bytes: u64,
    pub warnings: Vec<String>,
}

/// Overall result of downloading a manifest.
#[derive(Debug, Clone)]
pub struct DownloadOutcome {
    pub manifest_id: String,
    pub artifacts: Vec<ArtifactOutcome>,
}

/// A materialized install view.
#[derive(Debug, Clone)]
pub struct InstallView {
    pub artifact_path: String,
    pub dest: PathBuf,
    pub link_kind: LinkKind,
}

/// Cache eviction policy.
#[derive(Debug, Clone)]
pub enum EvictPolicy {
    /// Remove every blob.
    All,
    /// Remove a specific blob.
    Blob(String),
    /// Remove blobs not referenced by any install view.
    Unreferenced,
}

#[derive(Debug, Clone, Default)]
pub struct EvictReport {
    pub removed: Vec<String>,
    pub freed_bytes: u64,
}

/// Outcome of importing an already-downloaded local model file.
#[derive(Debug, Clone)]
pub struct LocalImportOutcome {
    pub manifest_id: String,
    pub model_name: String,
    pub blake3: String,
    pub sha256: String,
    pub size_bytes: u64,
    /// True if matched to a Hugging Face model by sha256 (enables P2P provenance).
    pub matched: bool,
    pub matched_model_id: Option<String>,
    /// Whether its license permits sharing it to peers.
    pub shareable: bool,
}

/// User-supplied metadata for importing/sharing a model that has no Hugging
/// Face match — the "title your file for sending" inputs. Every field is
/// optional; empty fields fall back to whatever was auto-parsed from the file's
/// header and filename (see [`crate::inspect`]).
#[derive(Debug, Clone, Default)]
pub struct LocalShareMeta {
    /// Human display title, e.g. `Mistral-7B-Instruct-v0.3`.
    pub title: Option<String>,
    pub family: Option<String>,
    pub quant: Option<String>,
    pub architecture: Option<String>,
    /// License tag (SPDX / open-weight family). Empty/None => `unknown`.
    pub license: Option<String>,
    /// Free-text description / model-card note.
    pub description: Option<String>,
    /// Where the model came from (e.g. the now-gone Hugging Face URL).
    pub origin_url: Option<String>,
    /// Skip the (slow, network) Hugging Face lookup — set when the user already
    /// knows the model isn't on HF.
    pub skip_hf_match: bool,
    /// Opt this model into the public mesh (Explore) right away — the explicit
    /// equivalent of ticking "Share on the open mesh" in the Library.
    pub publish: bool,
}

/// Callback for local-import hashing progress: `(bytes_hashed, bytes_total)`.
/// `Send + Sync` so it can be handed to the `spawn_blocking` that does the read;
/// `Arc`-wrapped so the caller keeps a clone if it wants.
pub type ImportProgress = std::sync::Arc<dyn Fn(u64, u64) + Send + Sync>;

impl LocalShareMeta {
    /// A bare import with no user metadata: auto-parse, still try HF, stay private.
    pub fn auto() -> Self {
        Self::default()
    }

    fn merge_into(&self, parsed: &mut crate::inspect::ParsedModel) {
        if let Some(t) = nonempty(&self.title) {
            parsed.title = t;
        }
        if let Some(f) = nonempty(&self.family) {
            parsed.family = Some(f);
        }
        if let Some(q) = nonempty(&self.quant) {
            parsed.quant = Some(q);
        }
        if let Some(a) = nonempty(&self.architecture) {
            parsed.architecture = Some(a);
        }
        if let Some(l) = nonempty(&self.license) {
            parsed.license = Some(l);
        }
        if let Some(o) = nonempty(&self.origin_url) {
            parsed.source_url = Some(o);
        }
    }
}

/// Trim an optional string and drop it if empty, returning an owned copy.
fn nonempty(o: &Option<String>) -> Option<String> {
    o.as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Result of pruning the index to match what's actually on disk.
#[derive(Debug, Clone, Default)]
pub struct ReconcileReport {
    pub removed_blobs: usize,
    pub removed_installs: usize,
    /// Orphaned download rows reaped: non-`complete` downloads whose `.part` temp
    /// or manifest is gone, so they can never resume (their `.part` temps, if any,
    /// are deleted too).
    pub removed_downloads: usize,
    /// BLAKE3 ids removed from the local cache index because their files vanished
    /// on disk. Callers can withdraw these from the tracker immediately instead
    /// of waiting for provider TTL expiry.
    pub removed_blake3s: Vec<String>,
}

/// One shareable file: its on-disk blob path + the catalog metadata to announce.
#[cfg(feature = "http")]
pub type ShareItem = (PathBuf, crate::tracker::AnnounceItem);

/// Shared announce identity (device name + group). Held behind a mutex so the UI
/// can update it live without tearing down and rebuilding the Iroh node.
#[cfg(feature = "iroh")]
type SharedIdentity = Arc<std::sync::Mutex<crate::tracker::Identity>>;

/// A running worldwide-share session (Iroh seeder + periodic tracker announce).
#[cfg(feature = "iroh")]
pub struct WorldwideShare {
    _node: Arc<crate::iroh_node::IrohNode>,
    ticket: String,
    metrics: crate::iroh_node::IrohMetrics,
    /// Stable device identity (hex) — how the tracker de-duplicates this peer.
    node_id: String,
    identity: SharedIdentity,
    /// App proxy ("VPN tunnel") for tracker traffic, if configured.
    proxy: Option<String>,
    announce_task: tokio::task::JoinHandle<()>,
}

#[cfg(feature = "iroh")]
impl WorldwideShare {
    /// This node's Iroh ticket (its worldwide address).
    pub fn node_ticket(&self) -> &str {
        &self.ticket
    }
    /// Upload counters from the live Iroh provider, used by desktop transfer
    /// graphs to show worldwide peer uploads (not just LAN HTTP uploads).
    pub fn metrics(&self) -> crate::iroh_node::IrohMetrics {
        self.metrics.clone()
    }
    /// How many peers are pulling bytes from us right now. The UI checks this
    /// before turning sharing off so it can warn that stopping disconnects them.
    pub fn active_uploads(&self) -> u64 {
        self.metrics.active_uploads()
    }
    /// How many peers are pulling *this one blob* right now. The UI checks this
    /// before turning a single model's share off, so it can warn that stopping
    /// disconnects the peers using that specific file.
    pub fn active_uploads_for(&self, blake3: &str) -> u64 {
        self.metrics.active_uploads_for_hex(blake3)
    }
    /// Update the announce identity (device name / group) live — no node restart.
    /// Takes effect on the next (re-)announce.
    pub fn set_identity(&self, id: crate::tracker::Identity) {
        if let Ok(mut g) = self.identity.lock() {
            *g = id;
        }
    }
    /// Stop serving a blob over Iroh right now (drops its store entry), so a
    /// per-model share-off or delete actually severs uploads instead of only
    /// hiding the model from the tracker.
    pub async fn unseed(&self, blake3: &str) {
        let _ = self._node.unseed(blake3).await;
    }
    /// Stop serving a blob **and** hard-disconnect peers mid-transfer of it — the
    /// per-file equivalent of [`stop`](Self::stop), used when a single model's
    /// share is turned off while peers are pulling that model.
    pub async fn unseed_and_disconnect(&self, blake3: &str) {
        let _ = self._node.unseed_and_disconnect(blake3).await;
    }
    /// A cloneable handle to the live seeder, so callers can re-seed +
    /// re-announce out-of-band (e.g. from a UI thread) without blocking — used
    /// after a new import or a per-model opt-in to publish it right away.
    pub fn seeder_handle(&self) -> SeederHandle {
        SeederHandle {
            node: self._node.clone(),
            ticket: self.ticket.clone(),
            node_id: self.node_id.clone(),
            identity: self.identity.clone(),
            proxy: self.proxy.clone(),
        }
    }
    /// Stop announcing **and** hard-disconnect: abort the re-announce loop, shut
    /// the router down (stop accepting), and close the QUIC endpoint so any peers
    /// currently pulling from us are dropped immediately. This is what makes
    /// the tracker. Pair with [`Engine::withdraw_from_tracker`] so we also leave
    /// the catalog right away.
    pub async fn stop(self) {
        self.announce_task.abort();
        self._node.shutdown_handle().await;
    }
}

/// A cloneable, `Send` handle to a running worldwide seeder for out-of-band
/// re-announces (the `WorldwideShare` itself stays owned by the UI).
#[cfg(feature = "iroh")]
#[derive(Clone)]
pub struct SeederHandle {
    node: Arc<crate::iroh_node::IrohNode>,
    ticket: String,
    node_id: String,
    identity: SharedIdentity,
    /// App proxy ("VPN tunnel") for tracker traffic, if configured.
    proxy: Option<String>,
}

#[cfg(feature = "iroh")]
impl SeederHandle {
    fn identity(&self) -> crate::tracker::Identity {
        self.identity.lock().map(|g| g.clone()).unwrap_or_default()
    }
    /// Stop serving a blob over Iroh (see [`WorldwideShare::unseed`]). Cloneable +
    /// `Send`, so the UI can fire it off-thread when a share is turned off.
    pub async fn unseed(&self, blake3: &str) {
        let _ = self.node.unseed(blake3).await;
    }
    /// Stop serving a blob **and** hard-disconnect peers mid-transfer of it (see
    /// [`WorldwideShare::unseed_and_disconnect`]). Cloneable + `Send` so the UI can
    /// fire it off-thread when a single model's share is turned off.
    pub async fn unseed_and_disconnect(&self, blake3: &str) {
        let _ = self.node.unseed_and_disconnect(blake3).await;
    }
    /// Seed each item by reference and announce it (with catalog metadata) to the
    /// tracker. Safe to call repeatedly; already-seeded blobs are cheap. Use for
    /// newly added content (a fresh download/import).
    pub async fn announce(&self, items: &[ShareItem], tracker_url: &str) {
        let mut ann: Vec<crate::tracker::AnnounceItem> = Vec::new();
        for (path, item) in items {
            if self.node.seed_file(path).await.is_ok() {
                ann.push(item.clone());
            }
        }
        if !ann.is_empty() {
            let _ = crate::tracker::announce(
                tracker_url,
                self.proxy.as_deref(),
                &self.ticket,
                &self.node_id,
                &self.identity(),
                &ann,
            )
            .await;
        }
    }
    /// Re-announce already-seeded content with the current identity, without
    /// re-hashing. Use after a device-name / group change.
    pub async fn reannounce(&self, items: &[crate::tracker::AnnounceItem], tracker_url: &str) {
        if !items.is_empty() {
            let _ = crate::tracker::announce(
                tracker_url,
                self.proxy.as_deref(),
                &self.ticket,
                &self.node_id,
                &self.identity(),
                items,
            )
            .await;
        }
    }
}

/// A model that is actually downloaded (its bytes are in the cache).
#[derive(Debug, Clone)]
pub struct InstalledModel {
    pub manifest_id: String,
    pub name: String,
    pub size_bytes: u64,
    pub blake3: String,
    pub sha256: String,
    pub from_hf: bool,
    /// License tag (SPDX/HF-style) from the model's manifest; empty if unknown.
    /// Carried into a share link so a receiver can tell whether it's reseedable.
    pub license: String,
    /// Model family, e.g. `Mistral` (from the manifest; carried into share links).
    pub family: Option<String>,
    /// Quantization label, e.g. `Q4_K_M`.
    pub quant: Option<String>,
    /// A free-text description the sharer wrote (provenance note).
    pub description: Option<String>,
    /// Where the model originally came from (e.g. a now-gone Hugging Face URL).
    pub origin: Option<String>,
    /// Whether this model carries a valid publisher signature.
    pub signed: bool,
    /// Whether this file is currently shared to the mesh (public by default).
    pub shareable: bool,
    /// Whether this model is access-controlled (gated / token-walled) — off by
    /// default for sharing; the user can opt it in deliberately.
    pub gated: bool,
    pub install_path: Option<String>,
}

/// A model discovered on the worldwide network catalog (browsable on the
/// Network tab). One row per content-addressed file, with a live peer count.
#[derive(Debug, Clone, Default)]
pub struct NetworkModel {
    pub blake3: String,
    pub sha256: String,
    pub name: String,
    pub size: u64,
    pub quant: String,
    pub license: String,
    /// Live worldwide peers currently sharing this file.
    pub peers: usize,
    /// Human device names sharing it (for "from your devices").
    pub devices: Vec<String>,
    /// True when this belongs to the querier's own device group.
    pub mine: bool,
    /// True when this exact file is already in the local Library.
    pub in_library: bool,
}

/// One place a file can be fetched from (an Explore search result row).
#[derive(Debug, Clone)]
pub struct SourceLocation {
    pub class: crate::manifest::SourceClass,
    pub locator: String,
    pub manifest_id: String,
    pub requires_auth: bool,
}

/// An Explore result: one *file* (content-addressed), surfaced with every place
/// it can be downloaded from across all known manifests. This is the system's
/// core value made visible — the same bytes, many interchangeable sources.
#[derive(Debug, Clone)]
pub struct FileResult {
    pub blake3: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub display_name: String,
    pub format: Option<String>,
    pub models: Vec<String>,
    pub cached: bool,
    pub sources: Vec<SourceLocation>,
    pub manifest_ids: Vec<String>,
}

/// Aggregate manifests into per-file results keyed by BLAKE3, keeping every
/// source for each file and filtering by a case-insensitive `query` over the
/// model name, file path, publisher, and hash. An empty query matches all.
pub fn aggregate_results(
    manifests: &[Manifest],
    cached: &std::collections::HashSet<String>,
    query: &str,
) -> Vec<FileResult> {
    use std::collections::HashMap;
    let q = query.trim().to_lowercase();
    let mut map: HashMap<String, FileResult> = HashMap::new();

    for m in manifests {
        for art in &m.artifacts {
            let entry = map
                .entry(art.hashes.blake3.clone())
                .or_insert_with(|| FileResult {
                    blake3: art.hashes.blake3.clone(),
                    sha256: art.hashes.sha256.clone(),
                    size_bytes: art.size_bytes,
                    display_name: art.path.clone(),
                    format: art.format.clone(),
                    models: Vec::new(),
                    cached: cached.contains(&art.hashes.blake3),
                    sources: Vec::new(),
                    manifest_ids: Vec::new(),
                });
            if !entry.models.contains(&m.model.name) {
                entry.models.push(m.model.name.clone());
            }
            if !entry.manifest_ids.contains(&m.manifest_id) {
                entry.manifest_ids.push(m.manifest_id.clone());
            }
            for src in &art.sources {
                let locator = src.source_id();
                if !entry.sources.iter().any(|s| s.locator == locator) {
                    entry.sources.push(SourceLocation {
                        class: src.class(),
                        locator,
                        manifest_id: m.manifest_id.clone(),
                        requires_auth: matches!(src.auth(), crate::manifest::AuthPolicy::Token),
                    });
                }
            }
        }
    }

    let mut results: Vec<FileResult> = map
        .into_values()
        .filter(|r| {
            if q.is_empty() {
                return true;
            }
            let hay = format!(
                "{} {} {}",
                r.display_name.to_lowercase(),
                r.models.join(" ").to_lowercase(),
                r.blake3
            );
            hay.contains(&q)
        })
        .collect();
    // Cached first, then by name, for a stable, useful ordering.
    results.sort_by(|a, b| {
        b.cached
            .cmp(&a.cached)
            .then_with(|| a.display_name.cmp(&b.display_name))
    });
    results
}

/// The engine.
pub struct Engine {
    cas: Cas,
    db: Arc<Db>,
    transports: Transports,
    policy: PolicyEngine,
    secret: Box<dyn SecretStore>,
    cfg: EngineConfig,
    /// Our own worldwide-seeder NodeId once sharing is running, so peer lookups
    /// can exclude ourselves (don't count or fetch from our own node).
    self_node_id: Arc<std::sync::Mutex<Option<String>>>,
    /// Cooperative cancel flag for the in-flight download. Set by
    /// [`Engine::request_pause`] / [`Engine::request_stop`], checked in the
    /// streaming loop so a user can interrupt a running (multi-GB) transfer. One
    /// flag because the engine runs at most one `download` at a time (the desktop
    /// guards on `busy`).
    cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Distinguishes the two ways a transfer is interrupted: `false` (the default)
    /// is a Pause — the partial temp is kept and the row marked `paused` so a later
    /// `download` resumes; `true` is a Stop — the partial temp and the download row
    /// are discarded so the next attempt restarts clean. Read after the streaming
    /// loop unwinds. A Stop sets this *before* `cancel` so a loop that observes the
    /// cancel also sees the discard intent.
    discard_partial: Arc<std::sync::atomic::AtomicBool>,
    /// Live override of `cfg.platform.huggingface_download` so the desktop's
    /// rebuilding the engine (i.e. without an app restart). Initialized from the
    /// platform profile; read into a per-plan profile copy in [`Engine::live_platform`].
    hf_download: Arc<std::sync::atomic::AtomicBool>,
    /// Live max parallel connections for a large HTTP download (segmented fetch).
    /// Mirrors `cfg.max_download_connections` but adjustable at runtime; read per
    /// download in [`Engine::try_source`]. `1` disables segmentation.
    max_conns: Arc<AtomicUsize>,
    /// The manifest id of the in-flight download, set at the top of [`Engine::download`]
    /// and stamped onto every progress event. Single-flight (see `cancel`), so one
    /// slot is enough. Lets the UI tie a live transfer back to its manifest — needed
    /// to resume a content/link download, whose id is synthesized at import time and
    /// is otherwise never seen on the wire.
    current_manifest_id: Arc<std::sync::Mutex<String>>,
}

impl Engine {
    /// Open (creating if needed) an engine rooted at `cfg.root`.
    pub fn open(cfg: EngineConfig) -> Result<Self> {
        let cas = Cas::open(&cfg.root)?;
        let db = Arc::new(Db::open(&cas.db_path())?);
        db.set_share_gated(cfg.share_gated);
        let transports = Transports::new(&cfg.transport)?;
        let policy = PolicyEngine::new(cfg.policy.clone());
        let secret = secret::default_store();
        let hf_download = Arc::new(std::sync::atomic::AtomicBool::new(
            cfg.platform.huggingface_download,
        ));
        let max_conns = Arc::new(AtomicUsize::new(cfg.max_download_connections.max(1)));
        Ok(Engine {
            cas,
            db,
            transports,
            policy,
            secret,
            cfg,
            self_node_id: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            discard_partial: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            hf_download,
            max_conns,
            current_manifest_id: Arc::new(std::sync::Mutex::new(String::new())),
        })
    }

    /// Set the max parallel connections for large HTTP downloads at runtime (no
    /// restart). `1` disables segmented (multi-connection) downloading; the new
    /// value applies to the next download.
    pub fn set_max_download_connections(&self, n: usize) {
        self.max_conns.store(n.max(1), Ordering::Relaxed);
    }

    /// The current max parallel connections (live value).
    pub fn max_download_connections(&self) -> usize {
        self.max_conns.load(Ordering::Relaxed)
    }

    /// The platform profile with live runtime overrides applied — currently the
    /// Hugging Face download toggle, which the desktop flips at runtime. Used for
    /// every source plan so a toggle takes effect on the next download, not after
    /// a restart.
    fn live_platform(&self) -> PlatformProfile {
        let mut p = self.cfg.platform.clone();
        p.huggingface_download = self.hf_download.load(std::sync::atomic::Ordering::Relaxed);
        p
    }

    /// Enable/disable Hugging Face as a byte-download source at runtime (no
    /// restart). Catalog search is unaffected — this only gates fetching bytes
    /// from `SourceClass::Huggingface`.
    pub fn set_hf_download_enabled(&self, on: bool) {
        self.hf_download
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether Hugging Face byte-downloads are currently enabled (live value).
    pub fn hf_download_enabled(&self) -> bool {
        self.hf_download.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Enable/disable auto-sharing of gated/restrictively-licensed *public*
    /// models at runtime (no restart). Off by default — only openly-licensed
    /// public models auto-share. Per-model overrides still win. The change
    /// applies on the next announce refresh.
    pub fn set_share_gated_enabled(&self, on: bool) {
        self.db.set_share_gated(on);
    }

    /// Whether gated/licensed public models are currently auto-shared (live).
    pub fn share_gated_enabled(&self) -> bool {
        self.db.share_gated()
    }

    /// Pause the in-flight download. The streaming loop notices promptly and
    /// returns [`Error::Cancelled`]; already-downloaded bytes are kept on disk and
    /// the row marked `paused`, so a later `download` of the same artifact resumes.
    /// No-op if nothing is downloading.
    pub fn request_pause(&self) {
        self.discard_partial
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Stop the in-flight download and discard its progress. The streaming loop
    /// notices promptly and the engine returns [`Error::Stopped`] after deleting
    /// the partial temp file and the download row, so the next attempt starts
    /// clean. No-op if nothing is downloading.
    pub fn request_stop(&self) {
        // Order matters: set the discard intent before the cancel flag so a
        // streaming loop that observes `cancel` also sees `discard_partial`
        // (SeqCst stores pair the two for the reader).
        self.discard_partial
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether a pause/stop has been requested for the current download.
    fn is_cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether the active interruption is a Stop (discard the partial) rather than
    /// a Pause (keep it for resume).
    fn discard_partial(&self) -> bool {
        self.discard_partial
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Finalize a user-interrupted artifact and return the error to propagate.
    /// A Pause keeps the partial temp and marks the row `paused` (so a later
    /// `download` resumes); a Stop deletes the partial temp and the download row
    /// dropped by the time this runs, so removing the temp is safe.
    async fn finish_cancelled(&self, download_id: &str, temp: &Path) -> Error {
        if self.discard_partial() {
            let _ = tokio::fs::remove_file(temp).await;
            self.db.delete_download(download_id).ok();
            Error::Stopped
        } else {
            self.db.set_download_state(download_id, "paused").ok();
            Error::Cancelled
        }
    }

    /// Our own stable seeder NodeId — so peer lookups, the catalog count, and
    /// tracker withdraws can all exclude/target *ourselves*.
    ///
    /// Uses the live seeder's id once worldwide sharing has started, but otherwise
    /// falls back to the NodeId persisted in the share store. That fallback is what
    /// keeps a *prior* session's announces (which linger on the tracker for their
    /// TTL, keyed on this same stable id) from counting as a phantom extra peer,
    /// being offered as a download source for ourselves, or surviving a delete —
    /// even when worldwide sharing isn't running this session.
    fn self_node_id(&self) -> Option<String> {
        if let Some(id) = self.self_node_id.lock().ok().and_then(|g| g.clone()) {
            return Some(id);
        }
        #[cfg(feature = "iroh")]
        {
            let store_dir = self.cfg.root.join("iroh-share-store");
            if let Some(id) = crate::iroh_node::node_id_from_store(&store_dir) {
                if let Ok(mut g) = self.self_node_id.lock() {
                    *g = Some(id.clone());
                }
                return Some(id);
            }
        }
        None
    }

    pub fn cas(&self) -> &Cas {
        &self.cas
    }
    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }
    pub fn policy(&self) -> &PolicyEngine {
        &self.policy
    }
    pub fn profile(&self) -> &PlatformProfile {
        &self.cfg.platform
    }

    /// The live download speed-limit handle (so the UI can change it on the fly).
    pub fn rate_limit(&self) -> RateLimit {
        self.cfg.rate_limit.clone()
    }

    /// Import a manifest from raw bytes: validate, verify signatures, persist.
    pub fn import_manifest(&self, bytes: &[u8]) -> Result<ImportResult> {
        let manifest = Manifest::from_json(bytes)?;
        manifest.validate()?;
        let report = verify_manifest(&manifest)?;
        let policy = self.policy.evaluate_download(&manifest, &report);

        // Persist manifest file + index regardless of policy outcome (so the UI
        // can show why something was blocked), but record the policy event.
        let path = self.cas.manifest_path(&manifest.manifest_id);
        std::fs::write(&path, manifest.to_json_pretty()?).map_err(|e| Error::fs(&path, e))?;
        self.db.insert_manifest(&manifest, &report)?;
        self.db.record_policy_event(
            Some(&manifest.manifest_id),
            if policy.allowed { "allow" } else { "deny" },
            &policy.reason,
        )?;

        Ok(ImportResult {
            manifest_id: manifest.manifest_id,
            report,
            policy,
        })
    }

    pub fn import_manifest_path(&self, path: &Path) -> Result<ImportResult> {
        let bytes = std::fs::read(path).map_err(|e| Error::fs(path, e))?;
        self.import_manifest(&bytes)
    }

    pub fn get_manifest(&self, manifest_id: &str) -> Result<Option<Manifest>> {
        self.db.get_manifest(manifest_id)
    }

    pub fn list_manifests(&self) -> Result<Vec<ManifestSummary>> {
        self.db.list_manifests()
    }

    /// All imported manifests in full (used for Explore aggregation).
    pub fn all_manifests(&self) -> Result<Vec<Manifest>> {
        let mut out = Vec::new();
        for s in self.db.list_manifests()? {
            if let Some(m) = self.db.get_manifest(&s.manifest_id)? {
                out.push(m);
            }
        }
        Ok(out)
    }

    pub fn verify_manifest(&self, manifest_id: &str) -> Result<VerificationReport> {
        let m = self
            .db
            .get_manifest(manifest_id)?
            .ok_or_else(|| Error::ManifestNotFound(manifest_id.into()))?;
        verify_manifest(&m)
    }

    fn require_manifest(&self, manifest_id: &str) -> Result<Manifest> {
        self.db
            .get_manifest(manifest_id)?
            .ok_or_else(|| Error::ManifestNotFound(manifest_id.into()))
    }

    /// Search imported manifests, returning one result per *file* with all of
    /// its known download sources aggregated by content hash.
    pub fn search(&self, query: &str) -> Result<Vec<FileResult>> {
        let cached: std::collections::HashSet<String> = self
            .db
            .list_cache_blobs()?
            .into_iter()
            .map(|b| b.blake3)
            .collect();
        let mut manifests = Vec::new();
        for s in self.db.list_manifests()? {
            if let Some(m) = self.db.get_manifest(&s.manifest_id)? {
                manifests.push(m);
            }
        }
        Ok(aggregate_results(&manifests, &cached, query))
    }

    /// Query a remote registry for manifests matching `query`. The returned
    /// manifests are NOT imported — the caller decides what to bring in.
    #[cfg(feature = "http")]
    pub async fn registry_search(&self, base_url: &str, query: &str) -> Result<Vec<Manifest>> {
        let builder = reqwest::Client::builder()
            .user_agent(concat!("noema-atlas/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20));
        let client = crate::transport::apply_proxy(builder, self.proxy())?
            .build()
            .map_err(|e| Error::other(format!("registry client: {e}")))?;
        let url = format!("{}/search", base_url.trim_end_matches('/'));
        let resp = client
            .get(&url)
            .query(&[("q", query)])
            .send()
            .await
            .map_err(|e| Error::other(format!("registry request: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::other(format!("registry returned {}", resp.status())));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Error::other(format!("registry response: {e}")))?;
        serde_json::from_slice::<Vec<Manifest>>(&bytes).map_err(Error::from)
    }

    /// Search the Hugging Face Hub for models matching `query`. Queries the
    /// configured Hub endpoint (the real Hub, or an HF mirror when one is set).
    #[cfg(feature = "http")]
    pub async fn hf_search(&self, query: &str, limit: usize) -> Result<Vec<crate::hf::HfModel>> {
        let token = secret::resolve_token(self.secret.as_ref(), "huggingface", "default")?;
        let client = self.http_client()?;
        let endpoint = &self.cfg.transport.hf_endpoint;
        crate::hf::search_models(&client, endpoint, query, limit, token.as_deref()).await
    }

    /// The most-downloaded models on the Hub, for a default "home" listing.
    #[cfg(feature = "http")]
    pub async fn hf_popular(&self, limit: usize) -> Result<Vec<crate::hf::HfModel>> {
        let token = secret::resolve_token(self.secret.as_ref(), "huggingface", "default")?;
        let client = self.http_client()?;
        let endpoint = &self.cfg.transport.hf_endpoint;
        crate::hf::popular_models(&client, endpoint, limit, token.as_deref()).await
    }

    /// Fetch a Hugging Face model's revision-pinned file list (from the configured
    /// Hub endpoint — real Hub or mirror).
    #[cfg(feature = "http")]
    pub async fn hf_model_detail(&self, id: &str) -> Result<crate::hf::HfModelDetail> {
        let token = secret::resolve_token(self.secret.as_ref(), "huggingface", "default")?;
        let client = self.http_client()?;
        let endpoint = &self.cfg.transport.hf_endpoint;
        crate::hf::model_detail(&client, endpoint, id, token.as_deref()).await
    }

    /// Synthesize + import a manifest for one HF file, returning it ready to
    /// download. The user never sees the manifest; this is the magic glue.
    #[cfg(feature = "http")]
    pub fn hf_import_file(
        &self,
        detail: &crate::hf::HfModelDetail,
        file: &crate::hf::HfFile,
    ) -> Result<ImportResult> {
        let manifest = crate::hf::manifest_for(detail, file)?;
        let bytes = serde_json::to_vec(&manifest)?;
        self.import_manifest(&bytes)
    }

    /// Synthesize + import a single manifest covering an entire sharded
    /// safetensors/MLX model (all weight shards + the config/tokenizer sidecars).
    /// One import ⇒ the desktop's one-click "Download model" button. There are no
    /// quants to choose among, so this replaces the per-file rows for such repos.
    #[cfg(feature = "http")]
    pub fn hf_import_bundle(&self, detail: &crate::hf::HfModelDetail) -> Result<ImportResult> {
        let manifest = crate::hf::manifest_for_bundle(detail)?;
        let bytes = serde_json::to_vec(&manifest)?;
        self.import_manifest(&bytes)
    }

    /// Synthesize + import a manifest for a GGUF quant, folding a split quant's
    /// shards (`…-00001-of-00009.gguf`) into one downloadable model. Works for a
    /// single-file quant too, so every GGUF quant can route through it.
    #[cfg(feature = "http")]
    pub fn hf_import_gguf_quant(
        &self,
        detail: &crate::hf::HfModelDetail,
        files: &[crate::hf::HfFile],
    ) -> Result<ImportResult> {
        let manifest = crate::hf::manifest_for_gguf_quant(detail, files)?;
        let bytes = serde_json::to_vec(&manifest)?;
        self.import_manifest(&bytes)
    }

    /// The configured proxy ("VPN tunnel") for this engine's internet traffic, if
    /// any.
    #[cfg(feature = "http")]
    fn proxy(&self) -> Option<&str> {
        self.cfg.transport.proxy.as_deref()
    }

    #[cfg(feature = "http")]
    fn http_client(&self) -> Result<reqwest::Client> {
        let builder = reqwest::Client::builder()
            .user_agent(concat!("noema-atlas/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30));
        crate::transport::apply_proxy(builder, self.proxy())?
            .build()
            .map_err(|e| Error::other(format!("http client: {e}")))
    }

    /// The set of cache blob hashes (for marking results as already-available).
    pub fn cached_hashes(&self) -> Result<std::collections::HashSet<String>> {
        Ok(self
            .db
            .list_cache_blobs()?
            .into_iter()
            .map(|b| b.blake3)
            .collect())
    }

    /// Build a per-artifact fetch plan (diagnostics / UI preview).
    pub fn plan_download(&self, manifest_id: &str) -> Result<Vec<(String, Plan)>> {
        let m = self.require_manifest(manifest_id)?;
        let db = self.db.clone();
        let platform = self.live_platform();
        let mut out = Vec::new();
        for art in &m.artifacts {
            let plan = plan_artifact(&m, art, &platform, &self.policy, |sid| {
                db.get_source_health(sid).unwrap_or_else(|_| SourceHealth {
                    source_id: sid.to_string(),
                    ..Default::default()
                })
            });
            out.push((art.path.clone(), plan));
        }
        Ok(out)
    }

    /// Download (and verify) every artifact of a manifest into the CAS.
    pub async fn download(
        &self,
        manifest_id: &str,
        progress: Option<Progress>,
    ) -> Result<DownloadOutcome> {
        // Fresh start: clear any pause/stop flags left set by a previous run.
        self.cancel
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.discard_partial
            .store(false, std::sync::atomic::Ordering::SeqCst);
        // Stamp this download's id onto every progress event (see emit_raw) so the
        if let Ok(mut g) = self.current_manifest_id.lock() {
            g.clear();
            g.push_str(manifest_id);
        }
        let manifest = self.require_manifest(manifest_id)?;
        let report = verify_manifest(&manifest)?;
        let decision = self.policy.evaluate_download(&manifest, &report);
        if !decision.allowed {
            self.db
                .record_policy_event(Some(manifest_id), "deny", &decision.reason)?;
            return Err(Error::PolicyDenied(decision.reason));
        }

        let mut artifacts = Vec::new();
        for art in &manifest.artifacts {
            let outcome = self
                .download_artifact(&manifest, art, &decision, progress.clone())
                .await?;
            artifacts.push(outcome);
        }
        Ok(DownloadOutcome {
            manifest_id: manifest_id.to_string(),
            artifacts,
        })
    }

    async fn download_artifact(
        &self,
        manifest: &Manifest,
        artifact: &Artifact,
        decision: &PolicyDecision,
        progress: Option<Progress>,
    ) -> Result<ArtifactOutcome> {
        let download_id = artifact_download_id(&manifest.manifest_id, &artifact.path);
        let cached_blake3 =
            if artifact.hashes.has_blake3() && self.cas.has_blob(&artifact.hashes.blake3) {
                Some(artifact.hashes.blake3.clone())
            } else if artifact.hashes.has_sha256() {
                self.db.blake3_for_sha256(&artifact.hashes.sha256)?
            } else {
                None
            };
        if let Some(blake3) = cached_blake3 {
            self.db.upsert_cache_blob(
                &BlobMeta {
                    blake3: blake3.clone(),
                    sha256: artifact.hashes.sha256.clone(),
                    size_bytes: artifact.size_bytes,
                    committed_at: crate::util::now_rfc3339(),
                },
                "ready",
            )?;
            self.emit(
                &progress,
                manifest,
                artifact,
                None,
                artifact.size_bytes,
                "cache-hit",
            );
            return Ok(ArtifactOutcome {
                artifact_path: artifact.path.clone(),
                blake3,
                from_cache: true,
                source_id: None,
                size_bytes: artifact.size_bytes,
                warnings: artifact_warnings(decision, &artifact.path),
            });
        }

        // 1b. Worldwide discovery: ask the tracker who has this file anywhere,
        let mut artifact = artifact.clone();
        #[cfg(feature = "http")]
        if let Some(tracker) = self.cfg.tracker_url.clone() {
            let key = if artifact.hashes.has_sha256() {
                artifact.hashes.sha256.clone()
            } else {
                artifact.hashes.blake3.clone()
            };
            if !key.is_empty() {
                // Tell the UI we're looking for peers (this is the step the user
                // perceives as "finding peers"), then cap the lookup so a slow or
                // unreachable tracker can't dominate the connect time — on timeout
                // we just plan with whatever sources we already have.
                self.emit(
                    &progress,
                    manifest,
                    &artifact,
                    None,
                    artifact.size_bytes,
                    "discovering peers",
                );
                let lookup = tokio::time::timeout(
                    Duration::from_secs(5),
                    crate::tracker::providers(
                        &tracker,
                        self.proxy(),
                        &key,
                        self.self_node_id().as_deref(),
                    ),
                )
                .await;
                if let Ok(Ok(set)) = lookup {
                    if !set.nodes.is_empty() && set.blake3.len() == 64 {
                        tracing::info!(
                            peers = set.nodes.len(),
                            "tracker found worldwide P2P providers"
                        );
                        // Refresh the Iroh source with the tracker's *current* view
                        // rather than keeping whatever tickets were planned before.
                        // A peer's relay/direct addresses drift, so a resume that
                        // re-used a stale ticket would sit on "connecting" until it
                        // timed out, while a fresh fetch connects at once. Replace
                        // the tickets in place if a source already exists, else add.
                        if let Some(Source::Iroh {
                            blob_hash, tickets, ..
                        }) = artifact
                            .sources
                            .iter_mut()
                            .find(|s| matches!(s, Source::Iroh { .. }))
                        {
                            *blob_hash = set.blake3;
                            *tickets = set.nodes;
                        } else {
                            artifact.sources.push(Source::Iroh {
                                blob_hash: set.blake3,
                                tickets: set.nodes,
                                auth: crate::manifest::AuthPolicy::None,
                            });
                        }
                    }
                }
            }
        }
        let artifact = &artifact;

        // A Pause/Stop clicked during peer discovery (the lookup above can take a
        // few seconds) is noticed here, before we write any download state — so the
        // UI's "Pausing/Stopping…" isn't left hanging until a source opens. Nothing
        // is on disk yet, so there's nothing to clean up; just surface the right
        // error (no `paused` row is needed when zero bytes were fetched).
        if self.is_cancelled() {
            return Err(if self.discard_partial() {
                Error::Stopped
            } else {
                Error::Cancelled
            });
        }
        let db = self.db.clone();
        let platform = self.live_platform();
        let plan = plan_artifact(manifest, artifact, &platform, &self.policy, |sid| {
            db.get_source_health(sid).unwrap_or_else(|_| SourceHealth {
                source_id: sid.to_string(),
                ..Default::default()
            })
        });
        if plan.is_empty() {
            let reason = plan
                .excluded
                .iter()
                .map(|s| s.reason.as_str())
                .find(|r| !r.trim().is_empty())
                .unwrap_or("no source is currently eligible");
            return Err(Error::NoEligibleSource(format!(
                "{} ({reason})",
                artifact.path
            )));
        }

        self.db.upsert_download(
            &download_id,
            &manifest.manifest_id,
            &artifact.path,
            "running",
            artifact.size_bytes,
        )?;

        // Optional chunk tree (validated against the signed Merkle root) enables
        // per-leaf streaming verification; otherwise we fall back to full-file.
        let chunk_tree = self.cached_chunk_tree(artifact)?;
        // Name the temp by whichever digest the manifest provides (blake3 may be
        // unknown for sha256-only manifests).
        let art_key = if artifact.hashes.has_blake3() {
            &artifact.hashes.blake3
        } else {
            &artifact.hashes.sha256
        };
        let temp = self.cas.new_temp_path(&download_id, &short(art_key));
        let mut last_err: Option<Error> = None;
        let mut partial_owner: Option<String> = None;
        for scored in &plan.eligible {
            if self.is_cancelled() {
                return Err(self.finish_cancelled(&download_id, &temp).await);
            }
            let source = &scored.source;
            let sid = source.source_id();
            if self
                .db
                .get_source_health(&sid)
                .map(|h| h.banned)
                .unwrap_or(false)
            {
                continue;
            }
            if chunk_tree.is_none() {
                if let Some(owner) = &partial_owner {
                    if owner != &sid && tokio::fs::metadata(&temp).await.is_ok() {
                        truncate_to(&temp, 0).await?;
                    }
                }
            }

            match self
                .try_source(
                    artifact,
                    source,
                    &temp,
                    &download_id,
                    chunk_tree.clone(),
                    &progress,
                )
                .await
            {
                Ok(meta) => {
                    self.db.set_download_state(&download_id, "complete")?;
                    self.finalize_blob(artifact, &meta).await?;
                    let warnings = artifact_warnings(decision, &artifact.path);
                    return Ok(ArtifactOutcome {
                        artifact_path: artifact.path.clone(),
                        blake3: meta.blake3,
                        from_cache: false,
                        source_id: Some(sid),
                        size_bytes: meta.size_bytes,
                        warnings,
                    });
                }
                Err(e) => {
                    // User interrupt: stop the whole artifact now (Pause keeps the
                    // partial for resume; Stop discards it), don't fall over to the
                    // next source.
                    if matches!(e, Error::Cancelled) {
                        return Err(self.finish_cancelled(&download_id, &temp).await);
                    }
                    tracing::warn!(source = %sid, error = %e, "source failed");
                    let reason = failover_reason(&e);
                    let partial_len = tokio::fs::metadata(&temp)
                        .await
                        .map(|m| m.len())
                        .unwrap_or(0);
                    self.emit_raw(
                        &progress,
                        artifact,
                        Some(source),
                        partial_len,
                        artifact.size_bytes,
                        "source-failed",
                        Some(reason.clone()),
                        None,
                    );
                    last_err = Some(e);
                    // Record who wrote the surviving partial (none if quarantined).
                    partial_owner = if partial_len > 0 { Some(sid) } else { None };
                }
            }
        }
        self.db.set_download_state(&download_id, "failed")?;
        Err(last_err.unwrap_or_else(|| Error::NoEligibleSource(artifact.path.clone())))
    }

    /// Attempt to fetch an artifact from a single source, with transient retry
    /// and resume. Returns the committed blob metadata on full verification.
    #[allow(clippy::too_many_arguments)]
    async fn try_source(
        &self,
        artifact: &Artifact,
        source: &Source,
        temp: &Path,
        download_id: &str,
        chunk_tree: Option<ChunkTree>,
        progress: &Option<Progress>,
    ) -> Result<BlobMeta> {
        let adapter = self.transports.for_class(source.class())?;
        // In-`open()` transports (Iroh) download the whole blob before
        // the engine's stream loop runs, so they'd otherwise show no progress for
        // the entire (often multi-minute) network transfer — the UI would sit on
        // its initial "finding peers" label the whole time. Hand them a sink that
        // forwards live byte counts straight to the progress callback.
        let on_bytes = progress.as_ref().map(|cb| {
            let cb = cb.clone();
            let artifact_path = artifact.path.clone();
            let source_id = source.source_id();
            // The closure is `'static` and can't borrow `self`, so snapshot the
            // in-flight manifest id now (single-flight, so it won't change under us).
            let manifest_id = self.current_manifest_id();
            crate::transport::ProgressSink(Arc::new(move |done: u64, total: u64| {
                cb(DownloadProgress {
                    manifest_id: manifest_id.clone(),
                    artifact_path: artifact_path.clone(),
                    source_id: Some(source_id.clone()),
                    bytes_done: done,
                    bytes_total: total,
                    phase: "downloading",
                    failover_reason: None,
                    effective_start: None,
                });
            }))
        });
        let mut ctx = FetchCtx {
            timeout: Some(self.cfg.transport.request_timeout),
            token: None,
            // Let in-`open()` transports (Iroh) abort promptly on Pause/Stop, and
            // tell them whether a Stop should discard their own partial (Iroh keeps
            // an incomplete blob in its store that the engine's `.part` cleanup
            // can't reach).
            cancel: Some(self.cancel.clone()),
            discard_partial: Some(self.discard_partial.clone()),
            on_bytes,
        };
        if let AuthRequirement::Token { service } = adapter.auth_requirements(source) {
            let token = secret::resolve_token(self.secret.as_ref(), &service, "default")?;
            if token.is_none() {
                return Err(Error::AuthRequired(source.source_id()));
            }
            ctx.token = token;
        }

        // Multi-connection (segmented) fast path: large, range-capable HTTP
        // sources, fresh download (no partial to resume), no speed cap. Falls
        // back to the single-stream loop below on any failure — and because the
        // full-file dual-hash gates the commit, a bad parallel assembly fails
        // safe (it never enters the cache), it doesn't corrupt anything.
        let n_conns = self.max_download_connections();
        if n_conns > 1
            && artifact.size_bytes >= 2 * SEGMENT_MIN_BYTES
            && self.cfg.rate_limit.bps() == 0
            && http_range_class(source.class())
            && !self.is_cancelled()
            && tokio::fs::metadata(temp)
                .await
                .map(|m| m.len())
                .unwrap_or(0)
                == 0
        {
            match self
                .stream_segmented(
                    artifact,
                    source,
                    adapter,
                    &ctx,
                    temp,
                    download_id,
                    n_conns,
                    progress,
                )
                .await
            {
                Ok(meta) => return Ok(meta),
                // A user interrupt stops the whole download — don't fall through.
                Err(e @ Error::Cancelled) => return Err(e),
                // Any other failure (range ignored, dropped segment, …): reset and
                // fall back to the resumable single-stream path below.
                Err(_) => {
                    let _ = truncate_to(temp, 0).await;
                }
            }
        }

        let mut attempts = 0;
        loop {
            attempts += 1;
            match self
                .stream_once(
                    artifact,
                    source,
                    adapter,
                    &ctx,
                    temp,
                    chunk_tree.clone(),
                    download_id,
                    progress,
                )
                .await
            {
                Ok(meta) => return Ok(meta),
                Err(e) => {
                    // A user cancel isn't a source failure — don't penalize/ban or
                    // retry; just propagate so the whole download stops.
                    if matches!(e, Error::Cancelled) {
                        return Err(e);
                    }
                    let kind = transport_kind(&e);
                    // Verifier failures (HashMismatch/SizeMismatch) AND transport
                    // integrity errors both mean: this source served bad bytes.
                    let integrity =
                        is_integrity(&e) || kind.map(|k| k.is_poisoning()).unwrap_or(false);
                    let retriable = !integrity && kind.map(|k| k.is_retriable()).unwrap_or(false);

                    self.db
                        .record_source_result(&source.source_id(), false, integrity, None)?;

                    if integrity {
                        // Poisoned bytes: quarantine and never trust this source again.
                        let label = if artifact.hashes.has_blake3() {
                            short(&artifact.hashes.blake3)
                        } else {
                            short(&artifact.hashes.sha256)
                        };
                        let q = self.cas.quarantine(download_id, temp, &label)?;
                        self.db.record_quarantine(
                            download_id,
                            &artifact.path,
                            Some(&source.source_id()),
                            &e.to_string(),
                            &q.to_string_lossy(),
                        )?;
                        return Err(e);
                    }
                    if retriable && attempts < self.cfg.max_attempts_per_source {
                        tokio::time::sleep(Duration::from_millis(250 * attempts as u64)).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// One streaming attempt: resume from any existing temp bytes, verify each
    /// byte, write to the temp file, and on completion verify + commit.
    #[allow(clippy::too_many_arguments)]
    async fn stream_once(
        &self,
        artifact: &Artifact,
        source: &Source,
        adapter: &dyn crate::transport::TransportAdapter,
        ctx: &FetchCtx,
        temp: &Path,
        chunk_tree: Option<ChunkTree>,
        download_id: &str,
        progress: &Option<Progress>,
    ) -> Result<BlobMeta> {
        let total = artifact.size_bytes;
        let temp_len = tokio::fs::metadata(temp)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        // Discard a leftover that can't be safely resumed:
        if (total > 0 && temp_len >= total) || (total == 0 && temp_len > 0) {
            truncate_to(temp, 0).await?;
        }
        let want = if temp_len > 0 && temp_len < total {
            temp_len
        } else {
            0
        };

        // Validate the existing prefix BEFORE opening the stream, so the resume
        // offset we request always corresponds to bytes the verifier trusts. A
        // corrupt local partial resets us cleanly to 0 (no stale-range desync).
        let (mut verifier, resume_from) = self
            .prepare_resume(temp, want, artifact, chunk_tree.clone())
            .await?;

        // Announce the connection attempt *before* opening: for in-`open()`
        // transports the open call blocks for the whole transfer, so without this
        // the UI would stay on its initial "finding peers" label until the first
        // byte progress arrives. The live `ctx.on_bytes` sink takes over from here.
        self.emit_raw(
            progress,
            artifact,
            Some(source),
            resume_from,
            total,
            "connecting",
            None,
            Some(resume_from),
        );

        let started = Instant::now();
        let opened = adapter
            .open(
                source,
                artifact,
                if resume_from > 0 {
                    Some(ByteRange {
                        start: resume_from,
                        end: None,
                    })
                } else {
                    None
                },
                ctx,
            )
            .await?;
        let effective_start = opened.effective_start.min(total);
        // In-`open()` transports (Iroh) already fetched the whole blob and
        // reported it live via `ctx.on_bytes`; this stream just re-reads the
        // local file to verify it. Report that pass as "verifying" (not a second
        // "downloading") so the UI holds the bar at 100% instead of resetting,
        // and the session/throughput counters don't double-count it.
        let prefetched = opened.prefetched;
        let stream_phase = if prefetched {
            "verifying"
        } else {
            "downloading"
        };

        // Reconcile: a server that ignored our range restarts at 0. Re-align the
        // temp + verifier to exactly [0, effective_start) so the write offset,
        // byte counter, and verifier never desync (no sparse holes).
        if effective_start != resume_from {
            truncate_to(temp, effective_start).await?;
            let (v2, _) = self
                .prepare_resume(temp, effective_start, artifact, chunk_tree.clone())
                .await?;
            verifier = v2;
        }
        // Skip this for prefetched transports: their `effective_start` is always
        // flicker the just-completed bar back to 0% before the verify pass.
        if !prefetched {
            self.emit_raw(
                progress,
                artifact,
                Some(source),
                effective_start,
                total,
                "connecting",
                None,
                Some(effective_start),
            );
        }

        // Open temp for writing at effective_start.
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false) // resume: keep already-downloaded bytes
            .open(temp)
            .await
            .map_err(|e| Error::fs(temp, e))?;
        file.seek(std::io::SeekFrom::Start(effective_start))
            .await
            .map_err(|e| Error::fs(temp, e))?;

        let mut bytes_done = effective_start;
        let mut stream = opened.stream;
        let mut last_emit = 0u64;
        let mut win_start = Instant::now();
        let mut win_bytes = 0u64;
        while let Some(chunk) = stream.next().await {
            // Cooperative cancel: stop promptly but keep the partial on disk so a
            // later attempt resumes instead of restarting from zero.
            if self.is_cancelled() {
                file.flush().await.ok();
                self.db
                    .update_download_progress(download_id, bytes_done, Some(&source.source_id()))
                    .ok();
                return Err(Error::Cancelled);
            }
            let chunk = chunk?;
            // Guard against a source sending more than declared. `total == 0`
            // means the size is unknown (e.g. a bare Content-ID add), so the
            // cap doesn't apply — integrity still rests on the digests.
            if total > 0 && bytes_done + chunk.len() as u64 > total {
                return Err(Error::transport(
                    source.source_id(),
                    TransportErrorKind::Integrity(
                        "source sent more bytes than declared size".into(),
                    ),
                ));
            }
            verifier.feed(&chunk)?; // per-leaf integrity (when chunk tree present)
            file.write_all(&chunk)
                .await
                .map_err(|e| Error::fs(temp, e))?;
            bytes_done += chunk.len() as u64;
            self.cfg
                .rate_limit
                .pace(&mut win_start, &mut win_bytes, chunk.len())
                .await;

            if bytes_done - last_emit > 4 * 1024 * 1024 || bytes_done == total {
                last_emit = bytes_done;
                self.db.update_download_progress(
                    download_id,
                    bytes_done,
                    Some(&source.source_id()),
                )?;
                self.emit_raw(
                    progress,
                    artifact,
                    Some(source),
                    bytes_done,
                    total,
                    stream_phase,
                    None,
                    None,
                );
            }
        }
        file.flush().await.map_err(|e| Error::fs(temp, e))?;

        if bytes_done < total {
            // Connection dropped early; keep temp for resume by another source.
            return Err(Error::transport(
                source.source_id(),
                TransportErrorKind::Other(format!(
                    "incomplete: {bytes_done}/{total} bytes (will resume)"
                )),
            ));
        }
        let hashes = verifier.finish()?;
        let latency_ms = started.elapsed().as_millis() as i64;
        // When the size was unknown up front (`total == 0`, e.g. a bare
        // Content-ID add), the bytes we actually received are the truth.
        let final_total = if total > 0 { total } else { bytes_done };

        // Commit into the CAS (atomic) on a blocking thread (rename/copy I/O).
        // Format header is validated post-hash in `finalize_blob`; since the
        // bytes already match the signed manifest, a header mismatch is a
        // manifest-declaration warning, surfaced rather than fatal.
        let cas = self.cas.clone();
        let temp_buf = temp.to_path_buf();
        let hashes_for_commit = hashes.clone();
        let meta = tokio::task::spawn_blocking(move || {
            cas.commit_blob(&temp_buf, &hashes_for_commit, final_total)
        })
        .await
        .map_err(|e| Error::other(format!("commit task join: {e}")))??;
        self.db
            .record_source_result(&source.source_id(), true, false, Some(latency_ms))?;
        self.emit_raw(
            progress,
            artifact,
            Some(source),
            final_total,
            final_total,
            "verified",
            None,
            None,
        );
        Ok(meta)
    }

    /// Multi-connection (segmented) download of one range-capable HTTP source.
    /// Splits `[0, total)` into `n` contiguous segments fetched **concurrently**
    /// to disjoint offsets of `temp`, then runs the same full-file BLAKE3+SHA-256
    /// to let the caller fall back to single-stream; a failure here can never
    /// corrupt the cache because the commit is gated on the full-file hash.
    #[allow(clippy::too_many_arguments)]
    async fn stream_segmented(
        &self,
        artifact: &Artifact,
        source: &Source,
        adapter: &dyn crate::transport::TransportAdapter,
        ctx: &FetchCtx,
        temp: &Path,
        download_id: &str,
        n_conns: usize,
        progress: &Option<Progress>,
    ) -> Result<BlobMeta> {
        let total = artifact.size_bytes;
        let started = Instant::now();

        // Choose a connection count that keeps each segment >= SEGMENT_MIN_BYTES
        // (don't open many tiny connections), then split into contiguous ranges.
        let max_by_size = (total / SEGMENT_MIN_BYTES).max(1) as usize;
        let n = n_conns.min(max_by_size).max(1);
        if n < 2 {
            return Err(Error::other("segmentation not beneficial"));
        }
        let seg = total.div_ceil(n as u64);
        let mut ranges = Vec::new();
        let mut s = 0u64;
        while s < total {
            let e = (s + seg).min(total);
            ranges.push((s, e));
            s = e;
        }

        // Pre-size the temp so each segment can seek to its offset and write.
        // Eligibility guaranteed an empty/absent temp; size it explicitly via
        // `set_len` rather than truncating.
        {
            let f = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(temp)
                .await
                .map_err(|e| Error::fs(temp, e))?;
            f.set_len(total).await.map_err(|e| Error::fs(temp, e))?;
        }

        self.emit_raw(
            progress,
            artifact,
            Some(source),
            0,
            total,
            "connecting",
            None,
            Some(0),
        );

        let progressed = Arc::new(AtomicU64::new(0));
        let last_emit = Arc::new(AtomicU64::new(0));

        let futs = ranges.iter().map(|&(start, end)| {
            let progressed = progressed.clone();
            let last_emit = last_emit.clone();
            async move {
                let seg_len = end - start;
                let opened = adapter
                    .open(
                        source,
                        artifact,
                        Some(ByteRange {
                            start,
                            end: Some(end),
                        }),
                        ctx,
                    )
                    .await?;
                // The server must honor the range. For a non-zero start, a 200
                // (range ignored) delivers bytes from 0, so `effective_start`
                // would be 0 — bail so the caller falls back to single-stream.
                if opened.effective_start != start {
                    return Err(Error::transport(
                        source.source_id(),
                        TransportErrorKind::Other("server ignored range request".into()),
                    ));
                }
                let mut file = tokio::fs::OpenOptions::new()
                    .write(true)
                    .open(temp)
                    .await
                    .map_err(|e| Error::fs(temp, e))?;
                file.seek(std::io::SeekFrom::Start(start))
                    .await
                    .map_err(|e| Error::fs(temp, e))?;

                let mut got = 0u64;
                let mut stream = opened.stream;
                while got < seg_len {
                    if self.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    let Some(chunk) = stream.next().await else {
                        break;
                    };
                    let chunk = chunk?;
                    // Enforce the segment boundary — take at most the bytes this
                    // segment owns (defends against a server ignoring the range
                    // end and streaming to EOF).
                    let take = ((seg_len - got) as usize).min(chunk.len());
                    file.write_all(&chunk[..take])
                        .await
                        .map_err(|e| Error::fs(temp, e))?;
                    got += take as u64;

                    let done = progressed.fetch_add(take as u64, Ordering::Relaxed) + take as u64;
                    let prev = last_emit.load(Ordering::Relaxed);
                    if (done - prev > 4 * 1024 * 1024 || done == total)
                        && last_emit
                            .compare_exchange(prev, done, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                    {
                        self.db
                            .update_download_progress(download_id, done, Some(&source.source_id()))
                            .ok();
                        self.emit_raw(
                            progress,
                            artifact,
                            Some(source),
                            done,
                            total,
                            "downloading",
                            None,
                            None,
                        );
                    }
                }
                file.flush().await.map_err(|e| Error::fs(temp, e))?;
                if got < seg_len {
                    return Err(Error::transport(
                        source.source_id(),
                        TransportErrorKind::Other(format!(
                            "segment incomplete: {got}/{seg_len} bytes"
                        )),
                    ));
                }
                Ok::<(), Error>(())
            }
        });

        // Run all segments concurrently (in-flight on one task — no spawn — so
        // they can borrow the adapter/ctx). The first error aborts the rest.
        futures_util::future::try_join_all(futs).await?;

        if self.is_cancelled() {
            return Err(Error::Cancelled);
        }

        // Full-file verification — the same non-negotiable gate as single-stream.
        let hashes = self.verify_temp_full(temp, artifact).await?;
        let latency_ms = started.elapsed().as_millis() as i64;
        let cas = self.cas.clone();
        let temp_buf = temp.to_path_buf();
        let hashes_for_commit = hashes.clone();
        let meta = tokio::task::spawn_blocking(move || {
            cas.commit_blob(&temp_buf, &hashes_for_commit, total)
        })
        .await
        .map_err(|e| Error::other(format!("commit task join: {e}")))??;
        self.db
            .record_source_result(&source.source_id(), true, false, Some(latency_ms))?;
        self.emit_raw(
            progress,
            artifact,
            Some(source),
            total,
            total,
            "verified",
            None,
            None,
        );
        Ok(meta)
    }

    /// Read the entire temp file and verify it against the artifact's digests +
    /// size, returning the computed hashes. Unlike [`Engine::prepare_resume`]
    /// this **errors** (rather than silently resetting) on any mismatch — it's
    /// the final gate before committing a segmented download.
    async fn verify_temp_full(&self, temp: &Path, artifact: &Artifact) -> Result<Hashes> {
        let expected = artifact.hashes.clone();
        let size = artifact.size_bytes;
        let what = artifact.path.clone();
        let temp_buf = temp.to_path_buf();
        tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut v = StreamingVerifier::new(expected, size, None, what);
            let mut f = std::fs::File::open(&temp_buf).map_err(|e| Error::fs(&temp_buf, e))?;
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = f.read(&mut buf).map_err(|e| Error::fs(&temp_buf, e))?;
                if n == 0 {
                    break;
                }
                v.feed(&buf[..n])?;
            }
            v.finish()
        })
        .await
        .map_err(|e| Error::other(format!("verify task join: {e}")))?
    }

    /// Build a streaming verifier that has consumed exactly `[0, want)` of the
    /// temp file, returning `(verifier, resume_from)`. If the existing prefix is
    /// corrupt or shorter than `want`, the temp is truncated to 0 and a fresh
    /// verifier is returned with `resume_from == 0`. The re-hash runs on a
    /// blocking thread to keep the async runtime responsive.
    async fn prepare_resume(
        &self,
        temp: &Path,
        want: u64,
        artifact: &Artifact,
        chunk_tree: Option<ChunkTree>,
    ) -> Result<(StreamingVerifier, u64)> {
        let fresh = || {
            StreamingVerifier::new(
                artifact.hashes.clone(),
                artifact.size_bytes,
                chunk_tree.clone(),
                artifact.path.clone(),
            )
        };
        if want == 0 {
            return Ok((fresh(), 0));
        }

        let expected = artifact.hashes.clone();
        let size = artifact.size_bytes;
        let ct = chunk_tree.clone();
        let what = artifact.path.clone();
        let temp_buf = temp.to_path_buf();
        let validated: Option<StreamingVerifier> = tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut v = StreamingVerifier::new(expected, size, ct, what);
            let mut f = match std::fs::File::open(&temp_buf) {
                Ok(f) => f,
                Err(_) => return None,
            };
            let mut remaining = want;
            let mut buf = vec![0u8; 1 << 20];
            while remaining > 0 {
                let take = (buf.len() as u64).min(remaining) as usize;
                let n = match f.read(&mut buf[..take]) {
                    Ok(0) | Err(_) => return None, // short read or io error => discard
                    Ok(n) => n,
                };
                if v.feed(&buf[..n]).is_err() {
                    return None; // corrupt prefix (caught with a chunk tree)
                }
                remaining -= n as u64;
            }
            Some(v)
        })
        .await
        .map_err(|e| Error::other(format!("resume task join: {e}")))?;

        match validated {
            Some(v) => Ok((v, want)),
            None => {
                truncate_to(temp, 0).await?;
                Ok((fresh(), 0))
            }
        }
    }

    /// After a successful commit: index the blob and build/store a chunk tree
    /// for future streaming verification and LAN serving. The whole-blob read is
    /// offloaded to a blocking thread.
    async fn finalize_blob(&self, artifact: &Artifact, meta: &BlobMeta) -> Result<()> {
        self.db.upsert_cache_blob(meta, "ready")?;
        if self.cas.load_chunk_tree(&meta.blake3)?.is_none() {
            let leaf_size = artifact
                .chunking
                .as_ref()
                .map(|c| c.leaf_size)
                .unwrap_or(crate::hash::DEFAULT_LEAF_SIZE);
            if let Ok(blob_path) = self.cas.blob_path(&meta.blake3) {
                let cas = self.cas.clone();
                let blake3 = meta.blake3.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Ok(tree) = ChunkTree::from_file(&blob_path, leaf_size) {
                        let _ = cas.store_chunk_tree(&blake3, &tree);
                    }
                })
                .await;
            }
        }
        if let Ok(blob_path) = self.cas.blob_path(&meta.blake3) {
            if let Err(e) = validate_format_header(artifact.format.as_deref(), &blob_path) {
                tracing::warn!(artifact = %artifact.path, "format header note: {e}");
            }
        }
        Ok(())
    }

    /// Load a cached chunk tree only if its Merkle root matches the signed root.
    fn cached_chunk_tree(&self, artifact: &Artifact) -> Result<Option<ChunkTree>> {
        let Some(chunking) = &artifact.chunking else {
            return Ok(None);
        };
        let Some(tree) = self.cas.load_chunk_tree(&artifact.hashes.blake3)? else {
            return Ok(None);
        };
        if tree
            .root_hex()
            .eq_ignore_ascii_case(&chunking.leaf_b3_merkle_root)
        {
            Ok(Some(tree))
        } else {
            Ok(None)
        }
    }

    /// Import a file already on disk as a manifest artifact, if it matches the
    /// manifest's declared digests (avoids re-downloading).
    pub fn import_artifact_file(
        &self,
        manifest_id: &str,
        artifact_path: &str,
        file_path: &Path,
    ) -> Result<ArtifactOutcome> {
        let m = self.require_manifest(manifest_id)?;
        let artifact = m
            .artifact(artifact_path)
            .ok_or_else(|| Error::ArtifactNotFound(artifact_path.into()))?
            .clone();
        let (hashes, size) = crate::hash::hash_file(file_path)?;
        if size != artifact.size_bytes {
            return Err(Error::SizeMismatch {
                what: format!("local import of {artifact_path}"),
                expected: artifact.size_bytes,
                actual: size,
            });
        }
        if let Some((label, expected, actual)) = artifact.hashes.mismatch_against(&hashes) {
            return Err(Error::HashMismatch {
                what: format!("local import of {artifact_path} ({label})"),
                expected,
                actual,
            });
        }
        let meta = self.cas.import_file(file_path, &hashes, size)?;
        self.index_blob(&artifact, &meta)?;
        Ok(ArtifactOutcome {
            artifact_path: artifact.path.clone(),
            blake3: meta.blake3,
            from_cache: false,
            source_id: Some(format!("file:{}", file_path.display())),
            size_bytes: size,
            warnings: Vec::new(),
        })
    }

    /// Synchronous blob indexing (cache row + chunk tree + format check) for the
    /// import path, where blocking I/O is acceptable (a one-shot CLI/API call).
    fn index_blob(&self, artifact: &Artifact, meta: &BlobMeta) -> Result<()> {
        self.db.upsert_cache_blob(meta, "ready")?;
        if self.cas.load_chunk_tree(&meta.blake3)?.is_none() {
            let leaf_size = artifact
                .chunking
                .as_ref()
                .map(|c| c.leaf_size)
                .unwrap_or(crate::hash::DEFAULT_LEAF_SIZE);
            if let Ok(blob_path) = self.cas.blob_path(&meta.blake3) {
                if let Ok(tree) = ChunkTree::from_file(&blob_path, leaf_size) {
                    let _ = self.cas.store_chunk_tree(&meta.blake3, &tree);
                }
            }
        }
        if let Ok(blob_path) = self.cas.blob_path(&meta.blake3) {
            if let Err(e) = validate_format_header(artifact.format.as_deref(), &blob_path) {
                tracing::warn!(artifact = %artifact.path, "format header note: {e}");
            }
        }
        Ok(())
    }

    /// Import an already-downloaded model file: hash it, try to match it to its
    /// Hugging Face origin by sha256 (which equals the file's content hash), and
    /// bring it into the cache. A match gives it canonical provenance + license
    /// private local model titled from the file's own header + filename.
    pub async fn import_local_file(&self, path: &Path) -> Result<LocalImportOutcome> {
        self.import_local_file_with_meta(path, LocalShareMeta::auto())
            .await
    }

    /// Import a local model file with explicit, user-supplied metadata — the
    /// filename are auto-parsed first ([`crate::inspect`]); the user's `meta`
    /// overrides whatever it wants. When `meta.skip_hf_match` is false we still
    /// attempt the Hugging Face match (so a model that *is* on HF gets canonical
    /// provenance); a match always wins over a synthesized local manifest. When
    /// `meta.publish` is set and policy permits, the model is opted into the mesh.
    pub async fn import_local_file_with_meta(
        &self,
        path: &Path,
        meta: LocalShareMeta,
    ) -> Result<LocalImportOutcome> {
        self.import_local_file_with_meta_progress(path, meta, None)
            .await
    }

    /// [`import_local_file_with_meta`] with an optional hashing-progress callback.
    /// The callback fires from the blocking hash thread with
    /// `(bytes_hashed, bytes_total)`, letting a UI show a live percentage instead
    /// of an indefinite spinner while a multi-gigabyte file is read.
    pub async fn import_local_file_with_meta_progress(
        &self,
        path: &Path,
        meta: LocalShareMeta,
        on_hash: Option<ImportProgress>,
    ) -> Result<LocalImportOutcome> {
        let owned = path.to_path_buf();
        let (hashes, size) = tokio::task::spawn_blocking(move || match on_hash {
            Some(cb) => crate::hash::hash_file_with_progress(&owned, |done, total| cb(done, total)),
            None => crate::hash::hash_file(&owned),
        })
        .await
        .map_err(|e| Error::other(format!("hash task: {e}")))??;
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("model.gguf")
            .to_string();

        #[cfg(feature = "http")]
        let matched: Option<(Manifest, String)> = if meta.skip_hf_match {
            None
        } else {
            self.match_local_to_hf(&filename, &hashes.sha256)
                .await
                .unwrap_or(None)
        };
        #[cfg(not(feature = "http"))]
        let matched: Option<(Manifest, String)> = None;

        let (mut manifest, matched_model_id) = match matched {
            Some((m, id)) => (m, Some(id)),
            None => {
                // No HF match: synthesize a titled local manifest from the file's
                // header + filename, with the user's overrides applied.
                let owned = path.to_path_buf();
                let file_meta =
                    tokio::task::spawn_blocking(move || crate::inspect::read_file_meta(&owned))
                        .await
                        .unwrap_or_default();
                let mut parsed = crate::inspect::parse_model(&filename, &file_meta);
                meta.merge_into(&mut parsed);
                let desc = meta
                    .description
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                (
                    titled_manifest(&parsed, &filename, desc, &hashes, size),
                    None,
                )
            }
        };

        // Ensure the artifact carries the real blake3 we just computed. HF-matched
        // manifests are sha256-only (blake3 unknown ahead of download) — without
        // this, the shareability join on `artifacts.blake3` can never match the
        // cached blob, so even a permissively-licensed import would never seed.
        if let Some(art) = manifest.artifacts.first_mut() {
            if !art.hashes.has_blake3() {
                art.hashes.blake3 = hashes.blake3.clone();
            }
        }

        self.import_manifest(&serde_json::to_vec(&manifest)?)?;
        let cas_meta = self.cas.import_file(path, &hashes, size)?;
        if let Some(art) = manifest.artifacts.first() {
            self.index_blob(art, &cas_meta)?;
        }

        // Explicit opt-in to the public mesh (the user chose "publish"). This
        // mirrors the per-model Library toggle; the engine honors the explicit
        // choice rather than relying on auto-share provenance (a `local`
        // manifest never auto-shares — see `Manifest::has_public_provenance`).
        if meta.publish {
            // Honor the explicit publish choice for any content — the operator
            // chose to share it (Atlas verifies bytes, not licenses).
            let _ = self
                .db
                .set_share_override(&cas_meta.blake3, &hashes.sha256, true);
        }

        Ok(LocalImportOutcome {
            manifest_id: manifest.manifest_id.clone(),
            model_name: manifest.model.name.clone(),
            shareable: self.db.is_blob_shareable(&cas_meta.blake3).unwrap_or(false),
            blake3: cas_meta.blake3,
            sha256: hashes.sha256,
            size_bytes: size,
            matched: matched_model_id.is_some(),
            matched_model_id,
        })
    }

    /// Retitle / relicense / describe a model already in the Library (typically a
    /// local import, but works for any manifest). Loads the manifest, applies the
    /// `meta` overrides, and persists. Optionally opts it into the mesh. This is
    /// the post-import "give it a real title before sharing" path.
    pub fn rename_model(&self, manifest_id: &str, meta: &LocalShareMeta) -> Result<()> {
        let mut m = self.require_manifest(manifest_id)?;
        if let Some(t) = nonempty(&meta.title) {
            if let Some(art) = m.artifacts.first_mut() {
                art.path = sanitize_local_name(&t);
            }
            m.model.name = t;
        }
        if let Some(f) = nonempty(&meta.family) {
            m.model.family = Some(f);
        }
        if let Some(q) = nonempty(&meta.quant) {
            m.model.quantization = Some(q);
        }
        if let Some(a) = nonempty(&meta.architecture) {
            m.model.architecture = Some(a);
        }
        if let Some(l) = nonempty(&meta.license) {
            m.license.redistribution = crate::manifest::RedistributionClass::for_license(Some(&l));
            m.license.spdx = l;
        }
        let note = nonempty(&meta.description);
        let origin = nonempty(&meta.origin_url);
        if note.is_some() || origin.is_some() {
            let p = m
                .provenance
                .get_or_insert_with(|| crate::manifest::Provenance {
                    origin: Some("local-import".to_string()),
                    model_card_ref: None,
                    note: None,
                    malware_badges_observed: None,
                    generated_at: Some(crate::util::now_rfc3339()),
                });
            if let Some(n) = note {
                p.note = Some(n);
            }
            if let Some(o) = origin {
                p.model_card_ref = Some(o);
            }
        }
        self.import_manifest(&serde_json::to_vec(&m)?)?;
        if let Some(art) = m.artifacts.first() {
            if art.hashes.has_blake3() || art.hashes.has_sha256() {
                let _ = self.db.set_share_override(
                    &art.hashes.blake3,
                    &art.hashes.sha256,
                    meta.publish,
                );
            }
        }
        Ok(())
    }

    /// Search the Hub by the file's name and confirm by sha256, returning the
    /// canonical manifest + model id on an exact byte-for-byte match.
    #[cfg(feature = "http")]
    async fn match_local_to_hf(
        &self,
        filename: &str,
        sha256: &str,
    ) -> Result<Option<(Manifest, String)>> {
        let query = crate::hf::query_from_filename(filename);
        let models = self.hf_search(&query, 8).await?;
        for m in models {
            let Ok(detail) = self.hf_model_detail(&m.id).await else {
                continue;
            };
            if let Some(f) = detail
                .files
                .iter()
                .find(|f| f.sha256.as_deref() == Some(sha256))
            {
                let manifest = crate::hf::manifest_for(&detail, f)?;
                return Ok(Some((manifest, detail.id.clone())));
            }
        }
        Ok(None)
    }

    /// Prune the index so it reflects what's actually on disk: drop cache blobs
    /// whose files are gone and install views whose destinations were deleted
    pub fn reconcile(&self) -> Result<ReconcileReport> {
        let mut report = ReconcileReport::default();
        for b in self.db.list_cache_blobs()? {
            if !self.cas.has_blob(&b.blake3) {
                let _ = self.cas.remove_blob(&b.blake3);
                self.db.delete_cache_blob(&b.blake3)?;
                report.removed_blobs += 1;
                report.removed_blake3s.push(b.blake3);
            }
        }
        for i in self.db.list_installs()? {
            if !Path::new(&i.dest_path).exists() {
                self.db.delete_install_by_dest(&i.dest_path)?;
                report.removed_installs += 1;
            }
        }
        // Reap orphaned downloads so paused/failed rows and their `.part` temps
        // don't accumulate. A user *Pause* deliberately keeps a non-`complete` row
        // and the manifest survive (resume re-reads the temp and re-validates it
        // against the manifest's hashes). If either is gone the row is dead weight,
        // so drop it and any leftover temp. A still-resumable paused download is
        // left untouched.
        for d in self.db.list_downloads()? {
            if d.state == "complete" {
                continue;
            }
            let resumable = self.cas.download_temp_exists(&d.download_id)
                && self.db.get_manifest(&d.manifest_id)?.is_some();
            if !resumable {
                self.cas.remove_download_temps(&d.download_id);
                self.db.delete_download(&d.download_id)?;
                report.removed_downloads += 1;
            }
        }
        Ok(report)
    }

    /// Cached blobs the policy (or the user's opt-in) permits sharing, each as
    /// its on-disk path + the catalog metadata to announce.
    #[cfg(feature = "http")]
    pub fn share_announce_items(&self) -> Result<Vec<ShareItem>> {
        Ok(build_share_items(&self.db, &self.cas))
    }

    /// Browse the worldwide network catalog of shared models. `q` filters by
    /// name; the device-group id (if set) also surfaces your own devices' private
    /// shares. Rows are flagged `in_library` if already cached locally.
    #[cfg(feature = "http")]
    pub async fn network_catalog(
        &self,
        q: &str,
        group: Option<String>,
    ) -> Result<Vec<NetworkModel>> {
        let Some(tracker) = self.cfg.tracker_url.clone() else {
            return Ok(Vec::new());
        };
        let self_id = self.self_node_id();
        let rows = crate::tracker::catalog(
            &tracker,
            self.proxy(),
            q,
            group.as_deref(),
            self_id.as_deref(),
        )
        .await?;
        let installed: std::collections::HashSet<String> = self
            .installed_models()?
            .into_iter()
            .map(|m| m.sha256)
            .collect();
        Ok(rows
            .into_iter()
            .filter(|r| {
                // Hide "ghosts": a row flagged ours (a stale announce still in the
                // tracker from this device) that no other peer has *and* whose bytes
                // we no longer hold. That's a model we deleted/stopped sharing — it
                // shouldn't linger in Explore as a dead "download from yourself" row.
                // (Withdraw-on-delete clears it server-side too; this also covers the
                // window before the tracker has processed that.)
                !(r.mine && r.peers == 0 && !self.cas.has_blob(&r.blake3))
            })
            .map(|r| NetworkModel {
                in_library: !r.sha256.is_empty() && installed.contains(&r.sha256),
                blake3: r.blake3,
                sha256: r.sha256,
                name: r.name,
                size: r.size,
                quant: r.quant,
                license: r.license,
                peers: r.peers,
                devices: r.devices,
                mine: r.mine,
            })
            .collect())
    }

    /// Un-announce content from the worldwide tracker — call after deleting a
    /// model or turning a share off so it leaves Explore right away instead of
    /// lingering for its 30-minute TTL. `blake3s` empty withdraws *everything*
    /// this device announced (e.g. stopping worldwide sharing). Best-effort and a
    /// no-op unless worldwide sharing has run (so we know our own NodeId).
    #[cfg(feature = "http")]
    pub async fn withdraw_from_tracker(&self, blake3s: &[String]) {
        let (Some(tracker), Some(node_id)) = (self.cfg.tracker_url.clone(), self.self_node_id())
        else {
            return;
        };
        let _ = crate::tracker::withdraw(&tracker, self.proxy(), &node_id, blake3s).await;
    }

    /// Live count of worldwide peers currently sharing a given content hash
    #[cfg(feature = "http")]
    pub async fn worldwide_peers(&self, hash: &str) -> usize {
        let Some(tracker) = self.cfg.tracker_url.clone() else {
            return 0;
        };
        // Exclude ourselves: "N seeding worldwide" should mean *other* peers, so a
        // model only you seed reads as 0, not 1 (you). Bound the lookup (the model
        // detail view re-samples this every ~15s) so a slow tracker can't stall the
        // peer-count refresh; an unknown count reads as 0 rather than blocking.
        let lookup = tokio::time::timeout(
            Duration::from_secs(5),
            crate::tracker::providers(&tracker, self.proxy(), hash, self.self_node_id().as_deref()),
        )
        .await;
        match lookup {
            Ok(Ok(set)) => set.nodes.len(),
            _ => 0,
        }
    }

    /// Per-model share override: stop sharing one of your models, or opt a
    /// gated/off-by-default one in. Most models are shared publicly by default;
    /// the running worldwide session picks the change up on its next refresh.
    pub fn set_model_shared(&self, blake3: &str, sha256: &str, on: bool) -> Result<()> {
        // The operator's explicit choice is honored for any content — Atlas
        // verifies bytes, not licenses. (Gated/restrictive content is not
        // auto-shared by default, but the user can opt it in here.)
        self.db.set_share_override(blake3, sha256, on)
    }

    /// Add a model by its content id / share link: synthesize a verifiable,
    /// download-only manifest and fetch it. The tracker (if configured) resolves
    /// the content id to worldwide peers; the bytes are verified against the id.
    #[cfg(feature = "http")]
    pub async fn add_by_content(
        &self,
        target: crate::share::ShareTarget,
        progress: Option<Progress>,
    ) -> Result<DownloadOutcome> {
        if !target.has_content_id() {
            return Err(Error::other("share target has no content id"));
        }
        let manifest = content_manifest(&target);
        let res = self.import_manifest(&serde_json::to_vec(&manifest)?)?;
        self.download(&res.manifest_id, progress).await
    }

    /// Receive a whole multi-file model from a bundle link: fetch every file in
    /// the bundle (each verified independently against its own content id) and
    /// return one outcome per file. A single failed file aborts with that error
    #[cfg(feature = "http")]
    pub async fn add_bundle(
        &self,
        bundle: crate::share::ShareBundle,
        progress: Option<Progress>,
    ) -> Result<Vec<DownloadOutcome>> {
        let mut outcomes = Vec::new();
        for file in bundle.files {
            if !file.has_content_id() {
                continue;
            }
            outcomes.push(self.add_by_content(file, progress.clone()).await?);
        }
        if outcomes.is_empty() {
            return Err(Error::other("bundle had no fetchable files"));
        }
        Ok(outcomes)
    }

    /// Start sharing this node's permitted models **worldwide**: spin up an Iroh
    /// node (NAT-traversing via relays), seed every shareable blob by reference,
    /// and announce them to the tracker, re-announcing as new models arrive.
    #[cfg(feature = "iroh")]
    pub async fn start_worldwide_share(
        &self,
        tracker_url: String,
        identity: crate::tracker::Identity,
    ) -> Result<WorldwideShare> {
        let store_dir = self.cfg.root.join("iroh-share-store");
        let node = Arc::new(crate::iroh_node::IrohNode::spawn(&store_dir).await?);
        let metrics = node.metrics();
        let ticket = node.node_ticket().await?;
        let node_id = node.node_id();
        if let Ok(mut g) = self.self_node_id.lock() {
            *g = Some(node_id.clone());
        }

        // Seed + announce on a background task so the caller (the UI thread)
        // never blocks hashing multi-GB weights. Runs an immediate pass, then
        // refreshes every 5 minutes (tracker TTL is 15), picking up newly-shared
        // models and refreshing the announcement well before it expires.
        // Identity lives behind a mutex so a device-name/group edit is picked up
        // here without restarting the node.
        let identity: SharedIdentity = Arc::new(std::sync::Mutex::new(identity));
        let proxy = self.cfg.transport.proxy.clone();
        let db = self.db.clone();
        let cas = self.cas.clone();
        let node_bg = node.clone();
        let ticket_bg = ticket.clone();
        let node_id_bg = node_id.clone();
        let id_bg = identity.clone();
        let proxy_bg = proxy.clone();
        let announce_task = tokio::spawn(async move {
            // Purge anything this device left on the tracker in a *previous* session
            // before announcing fresh. The loop below only ever withdraws what this
            // session announced and later stopped, so without this a session that
            // was killed (or that deleted models while not running) would leave
            // phantom "from My device" rows lingering for their full TTL — exactly
            // the stale-persistence the user hit. Keyed on our stable NodeId, so it
            // only clears our own records; the re-announce republishes only what we
            // still hold.
            let _ =
                crate::tracker::withdraw(&tracker_url, proxy_bg.as_deref(), &node_id_bg, &[]).await;
            let mut seeded = std::collections::HashSet::new();
            let mut announced = std::collections::HashSet::new();
            let mut first = true;
            loop {
                if !first {
                    tokio::time::sleep(Duration::from_secs(300)).await;
                }
                first = false;
                let items = build_share_items(&db, &cas);
                let mut current = std::collections::HashSet::new();
                let mut ann: Vec<crate::tracker::AnnounceItem> = Vec::new();
                for (path, it) in &items {
                    let already_seeded = seeded.contains(&it.blake3);
                    let seeded_now = already_seeded || node_bg.seed_file(path).await.is_ok();
                    if seeded_now {
                        seeded.insert(it.blake3.clone());
                        current.insert(it.blake3.clone());
                        ann.push(it.clone());
                    }
                }
                let stale: Vec<String> = announced.difference(&current).cloned().collect();
                if !stale.is_empty() {
                    let _ = crate::tracker::withdraw(
                        &tracker_url,
                        proxy_bg.as_deref(),
                        &node_id_bg,
                        &stale,
                    )
                    .await;
                }
                if !ann.is_empty() {
                    let id = id_bg.lock().map(|g| g.clone()).unwrap_or_default();
                    let _ = crate::tracker::announce(
                        &tracker_url,
                        proxy_bg.as_deref(),
                        &ticket_bg,
                        &node_id_bg,
                        &id,
                        &ann,
                    )
                    .await;
                }
                announced = current;
            }
        });

        Ok(WorldwideShare {
            _node: node,
            ticket,
            metrics,
            node_id,
            identity,
            proxy,
            announce_task,
        })
    }

    /// The models that are actually downloaded (their bytes are cached), one row
    /// per distinct file, with provenance + share status for the Library view.
    pub fn installed_models(&self) -> Result<Vec<InstalledModel>> {
        use std::collections::{HashMap, HashSet};
        let blobs = self.db.list_cache_blobs()?;
        let cached_sha: HashSet<String> = blobs.iter().map(|b| b.sha256.clone()).collect();
        let cached_b3: HashSet<String> = blobs.iter().map(|b| b.blake3.clone()).collect();
        let installs = self.db.list_installs()?;

        let mut by_sha: HashMap<String, InstalledModel> = HashMap::new();
        for s in self.db.list_manifests()? {
            let Some(m) = self.db.get_manifest(&s.manifest_id)? else {
                continue;
            };
            let Some(art) = m.artifacts.first() else {
                continue;
            };
            let cached = (art.hashes.has_sha256() && cached_sha.contains(&art.hashes.sha256))
                || (art.hashes.has_blake3() && cached_b3.contains(&art.hashes.blake3));
            if !cached {
                continue;
            }
            // Resolve a real blake3 (manifest may be sha256-only).
            let blake3 = if art.hashes.has_blake3() {
                art.hashes.blake3.clone()
            } else {
                self.db
                    .blake3_for_sha256(&art.hashes.sha256)?
                    .unwrap_or_default()
            };
            let install_path = installs
                .iter()
                .find(|i| i.manifest_id == s.manifest_id)
                .map(|i| i.dest_path.clone());
            // Derive both flags from the same all-manifests provenance (with the
            // user's per-model override applied), so the Library row is consistent.
            let (shareable, gated) = if blake3.is_empty() {
                (m.auto_shareable(self.db.share_gated()), m.is_gated())
            } else {
                // Single source of truth: `is_blob_shareable` applies the
                // per-model override AND the gated/restrictive redistribution
                // gate, so the Library row can never claim a gated model is
                // shared even if an override row says so.
                let gated = self
                    .db
                    .blob_provenance(&blake3)
                    .map(|p| p.1)
                    .unwrap_or_else(|_| m.is_gated());
                let shared = self.db.is_blob_shareable(&blake3).unwrap_or(false);
                (shared, gated)
            };
            let (description, origin) = m
                .provenance
                .as_ref()
                .map(|p| (p.note.clone(), p.model_card_ref.clone()))
                .unwrap_or((None, None));
            let model = InstalledModel {
                manifest_id: s.manifest_id.clone(),
                name: s.model_name.clone(),
                size_bytes: art.size_bytes,
                blake3,
                sha256: art.hashes.sha256.clone(),
                from_hf: m.publisher.id.starts_with("hf:"),
                license: m.license.spdx.clone(),
                family: m.model.family.clone(),
                quant: m.model.quantization.clone(),
                description,
                origin,
                signed: !m.signatures.is_empty(),
                shareable,
                gated,
                install_path,
            };
            // Dedup distinct manifests of the same file; prefer the HF-matched one.
            by_sha
                .entry(model.sha256.clone())
                .and_modify(|existing| {
                    if model.from_hf && !existing.from_hf {
                        *existing = model.clone();
                    } else if model.install_path.is_some() && existing.install_path.is_none() {
                        existing.install_path = model.install_path.clone();
                    }
                })
                .or_insert(model);
        }
        let mut out: Vec<InstalledModel> = by_sha.into_values().collect();
        out.sort_by_key(|a| a.name.to_lowercase());
        Ok(out)
    }

    /// Materialize install views for all of a manifest's cached artifacts into
    /// `target_dir`, linking to CAS blobs where possible.
    pub fn materialize_install(
        &self,
        manifest_id: &str,
        target_dir: &Path,
    ) -> Result<Vec<InstallView>> {
        let m = self.require_manifest(manifest_id)?;
        let mut views = Vec::new();
        for art in &m.artifacts {
            crate::manifest::validate_artifact_path(&art.path)?;
            // Resolve the content key: blake3 if the manifest declared it, else
            // via the sha256 index (sha256-only / Hugging Face manifests).
            let blake3 = if art.hashes.has_blake3() && self.cas.has_blob(&art.hashes.blake3) {
                art.hashes.blake3.clone()
            } else if let Some(b) = art
                .hashes
                .has_sha256()
                .then(|| self.db.blake3_for_sha256(&art.hashes.sha256))
                .transpose()?
                .flatten()
            {
                b
            } else {
                return Err(Error::other(format!(
                    "artifact `{}` not in cache; download it first",
                    art.path
                )));
            };
            let dest = target_dir.join(&art.path);
            let kind = self.cas.materialize(&blake3, &dest)?;
            self.db.record_install(
                manifest_id,
                &art.path,
                &dest.to_string_lossy(),
                &blake3,
                kind.as_str(),
            )?;
            views.push(InstallView {
                artifact_path: art.path.clone(),
                dest,
                link_kind: kind,
            });
        }
        Ok(views)
    }

    pub fn list_cache(&self) -> Result<Vec<CacheBlobRow>> {
        self.db.list_cache_blobs()
    }

    pub fn list_installs(&self) -> Result<Vec<InstallRow>> {
        self.db.list_installs()
    }

    pub fn report_source_health(&self) -> Result<Vec<SourceHealth>> {
        self.db.list_source_health()
    }

    pub fn evict_cache(&self, policy: EvictPolicy) -> Result<EvictReport> {
        let mut report = EvictReport::default();
        let installs = self.db.list_installs()?;
        let to_remove: Vec<String> = match policy {
            EvictPolicy::All => self
                .db
                .list_cache_blobs()?
                .into_iter()
                .map(|b| b.blake3)
                .collect(),
            EvictPolicy::Blob(b) => vec![b],
            EvictPolicy::Unreferenced => {
                let installed: std::collections::HashSet<String> = self
                    .db
                    .list_installs()?
                    .into_iter()
                    .map(|i| i.blake3)
                    .collect();
                self.db
                    .list_cache_blobs()?
                    .into_iter()
                    .filter(|b| !installed.contains(&b.blake3))
                    .map(|b| b.blake3)
                    .collect()
            }
        };
        for blake3 in to_remove {
            for install in installs.iter().filter(|i| i.blake3 == blake3) {
                remove_install_dest(&install.dest_path)?;
                self.db.delete_install_by_dest(&install.dest_path)?;
            }
            if let Some(meta) = self.cas.blob_meta(&blake3)? {
                report.freed_bytes += meta.size_bytes;
            }
            self.cas.remove_blob(&blake3)?;
            self.db.delete_cache_blob(&blake3)?;
            report.removed.push(blake3);
        }
        Ok(report)
    }

    /// Export a JSON diagnostics bundle (no secrets).
    pub fn export_diagnostics(&self) -> Result<serde_json::Value> {
        let manifests = self.list_manifests()?;
        Ok(serde_json::json!({
            "platform": format!("{:?}", self.cfg.platform.platform),
            "root": self.cfg.root.to_string_lossy(),
            "manifests": manifests.len(),
            "cache_blobs": self.list_cache()?.len(),
            "total_blob_bytes": self.cas.total_blob_bytes()?,
            "installs": self.list_installs()?.len(),
            "source_health": self.report_source_health()?.iter().map(|h| serde_json::json!({
                "source_id": h.source_id,
                "success": h.success_count,
                "failure": h.failure_count,
                "integrity_failures": h.integrity_failures,
                "banned": h.banned,
                "last_latency_ms": h.last_latency_ms,
            })).collect::<Vec<_>>(),
            "quarantine": self.db.list_quarantine()?.len(),
        }))
    }

    pub fn set_token(&self, service: &str, token: &str) -> Result<()> {
        self.secret.set(service, "default", token)
    }

    pub fn delete_token(&self, service: &str) -> Result<()> {
        self.secret.delete(service, "default")
    }

    pub fn token_status(&self, source: &Source) -> Result<bool> {
        let service = service_for_source(source);
        Ok(secret::resolve_token(self.secret.as_ref(), &service, "default")?.is_some())
    }

    fn emit(
        &self,
        progress: &Option<Progress>,
        manifest: &Manifest,
        artifact: &Artifact,
        source: Option<&Source>,
        bytes_done: u64,
        phase: &'static str,
    ) {
        let _ = manifest;
        self.emit_raw(
            progress,
            artifact,
            source,
            bytes_done,
            artifact.size_bytes,
            phase,
            None,
            None,
        );
    }

    // Internal progress fan-out; the wide signature mirrors the ProgressEvent it
    // builds, so a struct would just shuffle the same fields around.
    #[allow(clippy::too_many_arguments)]
    fn emit_raw(
        &self,
        progress: &Option<Progress>,
        artifact: &Artifact,
        source: Option<&Source>,
        bytes_done: u64,
        bytes_total: u64,
        phase: &'static str,
        failover_reason: Option<String>,
        effective_start: Option<u64>,
    ) {
        if let Some(cb) = progress {
            cb(DownloadProgress {
                manifest_id: self.current_manifest_id(),
                artifact_path: artifact.path.clone(),
                source_id: source.map(|s| s.source_id()),
                bytes_done,
                bytes_total,
                phase,
                failover_reason,
                effective_start,
            });
        }
    }

    /// The manifest id of the in-flight download, stamped onto progress events.
    fn current_manifest_id(&self) -> String {
        self.current_manifest_id
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }
}

fn failover_reason(error: &Error) -> String {
    let reason = match error {
        Error::Transport { kind, .. } => match kind {
            TransportErrorKind::Connect(_) => "route could not connect",
            TransportErrorKind::Timeout => "route timed out",
            TransportErrorKind::Status { status, .. } if *status == 404 => {
                "route did not have the file"
            }
            TransportErrorKind::Status { status, .. } if *status >= 500 => "route server failed",
            TransportErrorKind::Status { .. } => "route returned an error",
            TransportErrorKind::NotFound => "route did not have the file",
            TransportErrorKind::Unsupported(_) => "route cannot serve this file",
            TransportErrorKind::Integrity(_) => "route served bytes that failed verification",
            TransportErrorKind::Unauthorized => "route needs authorization",
            TransportErrorKind::Other(_) => "route stalled",
        },
        Error::AuthRequired(_) => "route needs authorization",
        Error::HashMismatch { .. } | Error::SizeMismatch { .. } => {
            "route served bytes that failed verification"
        }
        Error::FormatInvalid { .. } => "route served an unexpected file format",
        _ => "route failed",
    };
    cap_reason(reason)
}

fn cap_reason(s: &str) -> String {
    const MAX: usize = 180;
    if s.len() <= MAX {
        s.to_string()
    } else {
        format!("{}...", &s[..MAX])
    }
}

fn remove_install_dest(dest_path: &str) -> Result<()> {
    let path = Path::new(dest_path);
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::fs(path, e)),
    };
    if meta.file_type().is_dir() {
        std::fs::remove_dir_all(path).map_err(|e| Error::fs(path, e))?;
    } else {
        // Committed CAS blobs are made read-only; clear that bit before deleting
        // so eviction works (notably on Windows, where the read-only attribute
        // blocks removal). This is intentional, not the clippy footgun.
        #[allow(clippy::permissions_set_readonly_false)]
        if meta.permissions().readonly() {
            let mut perms = meta.permissions();
            perms.set_readonly(false);
            let _ = std::fs::set_permissions(path, perms);
        }
        std::fs::remove_file(path).map_err(|e| Error::fs(path, e))?;
    }
    Ok(())
}

/// Collect every cached blob that may be shared (policy-eligible OR opted-in),
/// each as its on-disk path + the catalog metadata to announce. Used by both the
/// background announce loop and the on-demand `SeederHandle` refresh.
#[cfg(feature = "http")]
fn build_share_items(db: &Db, cas: &Cas) -> Vec<ShareItem> {
    let mut out = Vec::new();
    let Ok(blobs) = db.list_cache_blobs() else {
        return out;
    };
    for b in blobs {
        if !db.is_blob_shareable(&b.blake3).unwrap_or(false) {
            continue;
        }
        let Ok(path) = cas.blob_path(&b.blake3) else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        let (name, license, quant) = db
            .blob_catalog_meta(&b.blake3, &b.sha256)
            .ok()
            .flatten()
            .unwrap_or_default();
        out.push((
            path,
            crate::tracker::AnnounceItem {
                blake3: b.blake3,
                sha256: b.sha256,
                name,
                size: b.size_bytes,
                quant,
                license,
                // Everything we share is public by default — the point is that
                // anyone can find it in Explore. It already passed the
                // is_blob_shareable gate (non-gated or opted in).
                listable: true,
            },
        ));
    }
    out
}

/// Build a local manifest for an imported file with no HF match, titled from the
/// file's own header + filename (see [`crate::inspect`]) with the user's
/// overrides already applied to `parsed`. The license the user declared drives
/// the redistribution class (an open one makes the model reseedable once shared);
/// an unknown license stays download-only. The model is NOT auto-shared — a
/// `local` publisher has no public provenance — until the user opts it in.
fn titled_manifest(
    parsed: &crate::inspect::ParsedModel,
    filename: &str,
    description: Option<&str>,
    hashes: &crate::hash::Hashes,
    size: u64,
) -> Manifest {
    use crate::manifest::*;
    // Full blake3 in the id (not a 12-hex prefix) so two distinct imports can
    // never collide and silently overwrite each other's metadata.
    let manifest_id = format!("mdl_local_{}", hashes.blake3);
    let name = if parsed.title.trim().is_empty() {
        sanitize_local_name(filename)
    } else {
        parsed.title.trim().to_string()
    };
    let format = parsed
        .format
        .clone()
        .or_else(|| crate::inspect::format_from_name(filename));
    let license_tag = parsed
        .license
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    Manifest {
        schema_version: SCHEMA_VERSION.to_string(),
        manifest_id,
        publisher: Publisher {
            id: "local".to_string(),
            display_name: Some("Imported locally".to_string()),
            public_keys: vec![],
        },
        model: Model {
            name,
            family: parsed.family.clone().filter(|s| !s.trim().is_empty()),
            architecture: parsed.architecture.clone().filter(|s| !s.trim().is_empty()),
            revision: None,
            format: format.clone(),
            quantization: parsed.quant.clone().filter(|s| !s.trim().is_empty()),
        },
        license: License {
            spdx: license_tag.unwrap_or("unknown").to_string(),
            license_url: None,
            // An open license the user declared makes this reseedable once shared;
            // unknown stays download-only.
            redistribution: RedistributionClass::for_license(license_tag),
        },
        access: Access {
            gated: false,
            require_signed_manifest: false,
            // Open on every P2P transport so failover can use whatever's up.
            allowed_source_classes: vec![
                SourceClass::Iroh,
                SourceClass::Ipfs,
                SourceClass::HttpsMirror,
                SourceClass::LocalFile,
            ],
        },
        artifacts: vec![Artifact {
            path: sanitize_local_name(filename),
            role: "weights".to_string(),
            size_bytes: size,
            hashes: hashes.clone(),
            chunking: None,
            format,
            sources: vec![],
        }],
        provenance: Some(Provenance {
            origin: Some("local-import".to_string()),
            model_card_ref: parsed.source_url.clone().filter(|s| !s.trim().is_empty()),
            note: description.map(|s| s.to_string()),
            malware_badges_observed: None,
            generated_at: Some(crate::util::now_rfc3339()),
        }),
        signatures: vec![],
    }
}

/// Build a verifiable, download-only manifest for a model identified only by a
/// content id / share link (no HF page needed). The worldwide tracker resolves
/// the content id to peers at download time; bytes are verified against the id.
#[cfg(feature = "http")]
fn content_manifest(target: &crate::share::ShareTarget) -> Manifest {
    use crate::manifest::*;
    let seed = if target.sha256.len() == 64 {
        &target.sha256
    } else {
        &target.blake3
    };
    let manifest_id = format!("mdl_p2p_{}", &seed[..12.min(seed.len())]);
    let file_name = sanitize_local_name(&target.name);
    let display_name = {
        let t = target.display_title();
        if t.is_empty() {
            file_name.clone()
        } else {
            t.to_string()
        }
    };
    let format = crate::inspect::format_from_name(&file_name);
    let opt = |s: &str| {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    Manifest {
        schema_version: SCHEMA_VERSION.to_string(),
        manifest_id,
        publisher: Publisher {
            id: "p2p".to_string(),
            display_name: Some("Shared peer-to-peer".to_string()),
            public_keys: vec![],
        },
        model: Model {
            name: display_name,
            family: opt(&target.family),
            architecture: None,
            revision: None,
            format: format.clone(),
            quantization: opt(&target.quant),
        },
        license: License {
            spdx: if target.license.trim().is_empty() {
                "unknown".to_string()
            } else {
                target.license.trim().to_string()
            },
            license_url: None,
            // Derive the reseed policy from the carried license: an open license
            // makes this auto-shareable; an unknown one stays download-only (and
            // so is NOT auto-reseeded — see `Manifest::has_public_provenance`).
            redistribution: RedistributionClass::for_license(
                Some(target.license.trim()).filter(|s| !s.is_empty()),
            ),
        },
        access: Access {
            gated: false,
            require_signed_manifest: false,
            // Open on every P2P transport so failover can use whatever's up.
            allowed_source_classes: vec![
                SourceClass::Iroh,
                SourceClass::Ipfs,
                SourceClass::HttpsMirror,
            ],
        },
        artifacts: vec![Artifact {
            path: file_name,
            role: "weights".to_string(),
            size_bytes: target.size,
            hashes: crate::hash::Hashes::new(target.blake3.clone(), target.sha256.clone()),
            chunking: None,
            format,
            sources: vec![],
        }],
        provenance: Some(Provenance {
            origin: Some("p2p-content-id".to_string()),
            model_card_ref: opt(&target.origin),
            note: opt(&target.desc),
            malware_badges_observed: None,
            generated_at: Some(crate::util::now_rfc3339()),
        }),
        signatures: vec![],
    }
}

fn sanitize_local_name(filename: &str) -> String {
    filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename)
        .to_string()
}

fn artifact_download_id(manifest_id: &str, path: &str) -> String {
    let key = format!("{manifest_id}::{path}");
    let h = blake3::hash(key.as_bytes());
    hex::encode(&h.as_bytes()[..8])
}

fn short(s: &str) -> String {
    s.chars().take(12).collect()
}

fn transport_kind(e: &Error) -> Option<&TransportErrorKind> {
    match e {
        Error::Transport { kind, .. } => Some(kind),
        _ => None,
    }
}

/// HashMismatch / SizeMismatch surfaced by the verifier are integrity failures.
fn is_integrity(e: &Error) -> bool {
    matches!(e, Error::HashMismatch { .. } | Error::SizeMismatch { .. })
}

async fn truncate_to(path: &Path, len: u64) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .await
        .map_err(|e| Error::fs(path, e))?;
    file.set_len(len).await.map_err(|e| Error::fs(path, e))?;
    Ok(())
}

fn artifact_warnings(decision: &PolicyDecision, artifact_path: &str) -> Vec<String> {
    decision
        .warnings
        .iter()
        .filter(|w| w.starts_with(artifact_path))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hashes;
    use crate::manifest::RedistributionClass;

    fn hashes() -> Hashes {
        Hashes::new("6a4f".repeat(16), "c2de".repeat(16))
    }

    #[test]
    fn titled_manifest_fills_structured_fields() {
        let parsed = crate::inspect::ParsedModel {
            title: "Mistral-7B-Instruct-v0.3".into(),
            family: Some("Mistral".into()),
            quant: Some("Q4_K_M".into()),
            architecture: Some("llama".into()),
            format: Some("gguf".into()),
            license: Some("apache-2.0".into()),
            source_url: Some("https://huggingface.co/gone".into()),
            ..Default::default()
        };
        let m = titled_manifest(
            &parsed,
            "ggml-model-q4_k_m.gguf",
            Some("Rescued reupload."),
            &hashes(),
            4_000_000,
        );
        m.validate().unwrap();
        // The human title is the model name, not the raw filename.
        assert_eq!(m.model.name, "Mistral-7B-Instruct-v0.3");
        assert_eq!(m.model.family.as_deref(), Some("Mistral"));
        assert_eq!(m.model.quantization.as_deref(), Some("Q4_K_M"));
        assert_eq!(m.model.architecture.as_deref(), Some("llama"));
        // A declared open license makes it reseedable (once shared).
        assert_eq!(m.license.spdx, "apache-2.0");
        assert_eq!(
            m.license.redistribution,
            RedistributionClass::PublicP2pAllowed
        );
        // Full-blake3 id (collision-proof), and on-disk path keeps the filename.
        assert!(m.manifest_id.ends_with(&"6a4f".repeat(16)));
        assert_eq!(m.artifacts[0].path, "ggml-model-q4_k_m.gguf");
        // A `local` import is never auto-shared, even with an open license and
        // even with the gated-sharing opt-in on — it stays private until the user
        // opts in (no public provenance).
        assert!(!m.auto_shareable(false));
        assert!(!m.auto_shareable(true));
        let p = m.provenance.unwrap();
        assert_eq!(p.note.as_deref(), Some("Rescued reupload."));
        assert_eq!(
            p.model_card_ref.as_deref(),
            Some("https://huggingface.co/gone")
        );
    }

    #[test]
    fn titled_manifest_unknown_license_is_download_only() {
        let parsed = crate::inspect::ParsedModel {
            title: "Some Model".into(),
            format: Some("gguf".into()),
            ..Default::default()
        };
        let m = titled_manifest(&parsed, "some-model.gguf", None, &hashes(), 10);
        assert_eq!(m.license.spdx, "unknown");
        assert_eq!(
            m.license.redistribution,
            RedistributionClass::PublicDownloadOnly
        );
    }

    #[test]
    fn meta_overrides_parsed() {
        let mut parsed = crate::inspect::ParsedModel {
            title: "auto-title".into(),
            license: Some("unknown".into()),
            ..Default::default()
        };
        let meta = LocalShareMeta {
            title: Some("  My Real Title  ".into()),
            license: Some("mit".into()),
            quant: Some("".into()), // empty => ignored
            ..Default::default()
        };
        meta.merge_into(&mut parsed);
        assert_eq!(parsed.title, "My Real Title");
        assert_eq!(parsed.license.as_deref(), Some("mit"));
        assert_eq!(parsed.quant, None);
    }

    /// End-to-end: import a real (tiny) GGUF with explicit metadata + publish,
    /// confirm the Library row carries the title/license/quant and is shared, then
    /// retitle/relicense it in place.
    #[tokio::test]
    async fn import_with_meta_titles_and_shares_then_renames() {
        let dir = tempfile::tempdir().unwrap();
        // A minimal valid GGUF v3 (magic + version + zero counts) + some bytes.
        let model = dir.path().join("ggml-model-q4_k_m.gguf");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"GGUF");
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
        bytes.extend_from_slice(&0u64.to_le_bytes()); // kv_count
        bytes.extend_from_slice(&[7u8; 128]);
        std::fs::write(&model, &bytes).unwrap();

        let engine = Engine::open(EngineConfig::new(dir.path().join("atlas"))).unwrap();
        let meta = LocalShareMeta {
            title: Some("Mistral 7B Instruct".into()),
            license: Some("apache-2.0".into()),
            quant: Some("Q4_K_M".into()),
            skip_hf_match: true,
            publish: true,
            ..Default::default()
        };
        let out = engine
            .import_local_file_with_meta(&model, meta)
            .await
            .unwrap();
        assert!(!out.matched);
        assert_eq!(out.model_name, "Mistral 7B Instruct");
        assert!(
            out.shareable,
            "publish=true must opt the model into the mesh"
        );

        let installed = engine.installed_models().unwrap();
        let m = installed
            .iter()
            .find(|m| m.name == "Mistral 7B Instruct")
            .expect("imported model in library");
        assert_eq!(m.license, "apache-2.0");
        assert_eq!(m.quant.as_deref(), Some("Q4_K_M"));
        assert!(m.shareable);

        // Retitle + relicense in place.
        engine
            .rename_model(
                &m.manifest_id,
                &LocalShareMeta {
                    title: Some("Renamed Model".into()),
                    license: Some("mit".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let installed = engine.installed_models().unwrap();
        assert!(installed
            .iter()
            .any(|m| m.name == "Renamed Model" && m.license == "mit"));
    }
}
