use crate::error::{Error, Result, TransportErrorKind};
use crate::manifest::{Artifact, AuthPolicy, Source, SourceClass};
use bytes::Bytes;
use futures_util::Stream;
use std::pin::Pin;
use std::time::Duration;

/// A stream of byte chunks, each fallible.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;

/// Half-open byte range `[start, end)`; `end == None` means "to EOF".
#[derive(Debug, Clone, Copy)]
pub struct ByteRange {
    pub start: u64,
    pub end: Option<u64>,
}

/// What an adapter can do for a given source.
#[derive(Debug, Clone, Default)]
pub struct SourceCaps {
    pub supports_range: bool,
    pub size: Option<u64>,
}

/// Whether a source needs authentication, and under which service namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthRequirement {
    None,
    Token { service: String },
}

/// A byte-progress sink an adapter calls during an in-`open()` transfer.
///
/// Iroh downloads the whole blob before it returns a stream, so the engine's
/// per-chunk progress would otherwise show nothing until the network transfer
/// had already finished. The closure receives `(bytes_done, bytes_total)`.
#[derive(Clone)]
pub struct ProgressSink(pub std::sync::Arc<dyn Fn(u64, u64) + Send + Sync>);

impl ProgressSink {
    pub fn report(&self, done: u64, total: u64) {
        (self.0)(done, total);
    }
}

impl std::fmt::Debug for ProgressSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProgressSink")
    }
}

/// Rich live transfer stats from a swarm transport (BitTorrent), for the UI:
/// beyond byte progress, the connected-peer count and cumulative upload (so the UI
/// can show peers + a seed ratio). Down/up speeds are derived UI-side from deltas.
#[derive(Debug, Clone, Default)]
pub struct LiveStats {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub peers: u32,
    pub uploaded_bytes: u64,
}

/// A rich-stats sink an adapter calls during an in-`open()` swarm transfer
/// (BitTorrent). Separate from [`ProgressSink`] so byte-only transports (Iroh,
/// HTTP) are unaffected.
#[derive(Clone)]
pub struct StatsSink(pub std::sync::Arc<dyn Fn(LiveStats) + Send + Sync>);

impl StatsSink {
    pub fn report(&self, stats: LiveStats) {
        (self.0)(stats);
    }
}

impl std::fmt::Debug for StatsSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StatsSink")
    }
}

/// Persistence hook for a blob's *lifetime* BitTorrent upload, so the stop-at-ratio
/// cap survives restarts (librqbit's own upload counter resets each session).
/// Implemented by the engine's `Db`; kept as a narrow trait so the transport layer
/// doesn't depend on the database directly.
pub trait BtUploadStore: Send + Sync {
    /// Cumulative bytes uploaded for `blake3` across all prior runs (0 if none).
    fn load_uploaded(&self, blake3: &str) -> u64;
    /// Persist the new lifetime cumulative upload for `blake3`.
    fn store_uploaded(&self, blake3: &str, uploaded_bytes: u64);
}

/// Per-fetch context handed to adapters (resolved token, timeouts).
#[derive(Debug, Clone, Default)]
pub struct FetchCtx {
    pub token: Option<String>,
    pub timeout: Option<Duration>,
    /// Cooperative cancel flag for a user Pause/Stop. Adapters whose download
    /// happens inside `open()` (Iroh — the whole blob lands before the engine
    /// stream loop runs) must poll this and abort promptly; without it Pause/Stop
    /// would only register after the multi-GB transfer already finished.
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Whether the active interruption is a Stop (discard the partial) rather than
    /// a Pause (keep it for resume). Adapters that hold their partial somewhere the
    /// engine can't see (Iroh keeps an incomplete blob in its own store, not the
    /// engine's `.part`) must, on a discard-cancel, drop that partial themselves so
    /// a Stop truly starts clean. Ignored by adapters whose partial *is* the
    /// engine's `.part` (the engine deletes that one).
    pub discard_partial: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Live byte-progress sink for those same in-`open()` transports, so the UI
    /// reflects the real transfer instead of a frozen label. `None` for adapters
    /// whose `open()` returns immediately and streams incrementally (the engine's
    /// own stream loop already reports those).
    pub on_bytes: Option<ProgressSink>,
    /// Rich live-stats sink (peers, upload) for swarm transports (BitTorrent).
    /// `None` for transports that only report bytes via `on_bytes`.
    pub on_stats: Option<StatsSink>,
}

/// An opened transfer: a byte stream plus where it actually starts (servers may
/// ignore a range request and restart from 0) and the total size if known.
pub struct Opened {
    pub stream: ByteStream,
    pub effective_start: u64,
    pub total_size: Option<u64>,
    /// The whole blob was already fetched — and live-progress-reported via
    /// `ctx.on_bytes` — during `open()` (e.g. Iroh). The returned `stream` just
    /// re-reads that local file, so the engine's read loop is a local *verify*
    /// pass, not a network download: it must not be reported (or counted) as a
    /// second download. `false` for incrementally-streaming transports (HTTP,
    /// local file), whose read loop *is* the transfer.
    pub prefetched: bool,
}

/// The uniform adapter interface.
#[async_trait::async_trait]
pub trait TransportAdapter: Send + Sync {
    fn class(&self) -> SourceClass;

    /// Auth requirement for a source (derived from its `auth` policy + class).
    fn auth_requirements(&self, source: &Source) -> AuthRequirement {
        match source.auth() {
            AuthPolicy::None => AuthRequirement::None,
            AuthPolicy::Token => AuthRequirement::Token {
                service: service_for_source(source),
            },
        }
    }

    /// Best-effort capability probe (size, range support).
    async fn probe(
        &self,
        source: &Source,
        artifact: &Artifact,
        ctx: &FetchCtx,
    ) -> Result<SourceCaps>;

    /// Open a transfer, optionally from a byte offset (for resume).
    async fn open(
        &self,
        source: &Source,
        artifact: &Artifact,
        range: Option<ByteRange>,
        ctx: &FetchCtx,
    ) -> Result<Opened>;
}

/// The keystore service namespace for a source's credentials.
pub fn service_for_source(source: &Source) -> String {
    match source {
        Source::Huggingface { .. } => "huggingface".to_string(),
        Source::HttpsMirror { url, .. } => format!("https:{}", host_of(url)),
        Source::Iroh { .. } => "iroh".to_string(),
        Source::BittorrentV2 { .. } => "bittorrent".to_string(),
        Source::LanPeer { url, .. } => format!("lan:{}", host_of(url)),
        Source::LocalFile { .. } => "local".to_string(),
    }
}

fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url)
        .to_string()
}

/// Apply an optional proxy (the app's "VPN tunnel") to a reqwest client builder.
///
/// `proxy` accepts any scheme reqwest understands — `http://`, `https://`, or
/// `socks5://` (also `socks5h://` to resolve DNS through the proxy, which is the
/// safe choice for tunneling). An empty / whitespace value means "no proxy", so
/// callers can pass a config field straight through. The proxy applies to all
/// schemes (`Proxy::all`) so both plaintext probes and TLS downloads tunnel.
///
/// Returns an error on a malformed proxy URL rather than silently ignoring it —
/// a misconfigured tunnel should be visible, not leak traffic around the VPN.
#[cfg(feature = "http")]
pub fn apply_proxy(
    builder: reqwest::ClientBuilder,
    proxy: Option<&str>,
) -> Result<reqwest::ClientBuilder> {
    let raw = match proxy {
        Some(p) => p.trim(),
        None => return Ok(builder),
    };
    if raw.is_empty() {
        return Ok(builder);
    }
    let p = reqwest::Proxy::all(raw)
        .map_err(|e| Error::other(format!("invalid proxy url {raw:?}: {e}")))?;
    Ok(builder.proxy(p))
}
/// Imports a file that already exists on the local filesystem.
pub struct LocalFileAdapter;

#[async_trait::async_trait]
impl TransportAdapter for LocalFileAdapter {
    fn class(&self) -> SourceClass {
        SourceClass::LocalFile
    }

    async fn probe(
        &self,
        source: &Source,
        _artifact: &Artifact,
        _ctx: &FetchCtx,
    ) -> Result<SourceCaps> {
        let Source::LocalFile { path } = source else {
            return Err(adapter_mismatch());
        };
        let md = tokio::fs::metadata(path)
            .await
            .map_err(|e| transport(source, TransportErrorKind::Other(e.to_string())))?;
        Ok(SourceCaps {
            supports_range: true,
            size: Some(md.len()),
        })
    }

    async fn open(
        &self,
        source: &Source,
        _artifact: &Artifact,
        range: Option<ByteRange>,
        _ctx: &FetchCtx,
    ) -> Result<Opened> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let Source::LocalFile { path } = source else {
            return Err(adapter_mismatch());
        };
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|e| transport(source, TransportErrorKind::NotFound).context_io(e))?;
        let total = file.metadata().await.ok().map(|m| m.len());
        let start = range.map(|r| r.start).unwrap_or(0);
        if start > 0 {
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|e| transport(source, TransportErrorKind::Other(e.to_string())))?;
        }
        let stream = futures_util::stream::unfold((file, false), |(mut f, done)| async move {
            if done {
                return None;
            }
            let mut buf = vec![0u8; 256 * 1024];
            match f.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok(Bytes::from(buf)), (f, false)))
                }
                Err(e) => Some((
                    Err(Error::transport(
                        "local",
                        TransportErrorKind::Other(e.to_string()),
                    )),
                    (f, true),
                )),
            }
        });
        Ok(Opened {
            stream: Box::pin(stream),
            effective_start: start,
            total_size: total,
            prefetched: false,
        })
    }
}
#[cfg(feature = "http")]
mod net {
    use super::*;
    use futures_util::StreamExt;
    use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
    use reqwest::{Client, StatusCode};

    /// Shared HTTP client used by all HTTP-family adapters.
    #[derive(Clone)]
    pub struct HttpClient {
        client: Client,
    }

    impl HttpClient {
        pub fn new(timeout: Duration, proxy: Option<&str>) -> Result<Self> {
            let builder = Client::builder()
                .user_agent(concat!("noema-atlas/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(15))
                .timeout(timeout);
            let client = super::apply_proxy(builder, proxy)?
                .build()
                .map_err(|e| Error::other(format!("http client: {e}")))?;
            Ok(HttpClient { client })
        }

        async fn probe_url(
            &self,
            url: &str,
            token: Option<&str>,
            source_id: &str,
        ) -> Result<SourceCaps> {
            let mut req = self.client.head(url);
            if let Some(t) = token {
                req = req.bearer_auth(t);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => {
                    let size = resp
                        .headers()
                        .get(CONTENT_LENGTH)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok());
                    let supports_range = resp
                        .headers()
                        .get(ACCEPT_RANGES)
                        .and_then(|v| v.to_str().ok())
                        .map(|v| v.contains("bytes"))
                        .unwrap_or(false);
                    Ok(SourceCaps {
                        supports_range,
                        size,
                    })
                }
                // HEAD often unsupported; assume range works and discover at GET.
                _ => Ok(SourceCaps {
                    supports_range: true,
                    size: None,
                }),
            }
            .map_err(|e: Error| {
                Error::transport(source_id, TransportErrorKind::Other(e.to_string()))
            })
        }

        pub async fn open_url(
            &self,
            url: &str,
            token: Option<&str>,
            range: Option<ByteRange>,
            source_id: &str,
        ) -> Result<Opened> {
            let requested_start = range.map(|r| r.start).unwrap_or(0);
            // Half-open `[start, end)` → HTTP's inclusive `bytes=start-(end-1)`.
            // A bounded end enables segmented/multi-connection downloads; with no
            // end we request an open-ended range (resume-to-EOF).
            let requested_end = range.and_then(|r| r.end);
            let mut req = self.client.get(url);
            if let Some(t) = token {
                req = req.bearer_auth(t);
            }
            if requested_start > 0 || requested_end.is_some() {
                let header = match requested_end {
                    Some(end) if end > requested_start => {
                        format!("bytes={requested_start}-{}", end - 1)
                    }
                    _ => format!("bytes={requested_start}-"),
                };
                req = req.header(RANGE, header);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| map_reqwest_err(source_id, e))?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                return Err(Error::transport(
                    source_id,
                    TransportErrorKind::Unauthorized,
                ));
            }
            if status == StatusCode::NOT_FOUND {
                return Err(Error::transport(source_id, TransportErrorKind::NotFound));
            }
            if !status.is_success() {
                return Err(Error::transport(
                    source_id,
                    TransportErrorKind::Status {
                        status: status.as_u16(),
                        message: status.canonical_reason().unwrap_or("error").to_string(),
                    },
                ));
            }
            // Did the server honor the range?
            let effective_start = if requested_start > 0 && status == StatusCode::PARTIAL_CONTENT {
                requested_start
            } else if requested_start > 0 {
                // Range ignored (200 OK): stream restarts at 0.
                0
            } else {
                0
            };
            let total_size = parse_total_size(&resp);

            let sid = source_id.to_string();
            let stream = resp.bytes_stream().map(move |item| {
                item.map_err(|e| Error::transport(&sid, TransportErrorKind::Other(e.to_string())))
            });
            Ok(Opened {
                stream: Box::pin(stream),
                effective_start,
                total_size,
                prefetched: false,
            })
        }
    }

    fn parse_total_size(resp: &reqwest::Response) -> Option<u64> {
        // Prefer Content-Range total when present (range responses), else Content-Length.
        if let Some(cr) = resp
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
        {
            if let Some(total) = cr.rsplit('/').next() {
                if let Ok(n) = total.trim().parse::<u64>() {
                    return Some(n);
                }
            }
        }
        resp.headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
    }

    fn map_reqwest_err(source_id: &str, e: reqwest::Error) -> Error {
        let kind = if e.is_timeout() {
            TransportErrorKind::Timeout
        } else if e.is_connect() {
            TransportErrorKind::Connect(e.to_string())
        } else {
            TransportErrorKind::Other(e.to_string())
        };
        Error::transport(source_id, kind)
    }
    pub struct HttpMirrorAdapter {
        pub http: HttpClient,
    }

    #[async_trait::async_trait]
    impl TransportAdapter for HttpMirrorAdapter {
        fn class(&self) -> SourceClass {
            SourceClass::HttpsMirror
        }
        async fn probe(
            &self,
            source: &Source,
            _a: &Artifact,
            ctx: &FetchCtx,
        ) -> Result<SourceCaps> {
            let Source::HttpsMirror { url, .. } = source else {
                return Err(adapter_mismatch());
            };
            self.http
                .probe_url(url, ctx.token.as_deref(), &source.source_id())
                .await
        }
        async fn open(
            &self,
            source: &Source,
            _a: &Artifact,
            range: Option<ByteRange>,
            ctx: &FetchCtx,
        ) -> Result<Opened> {
            let Source::HttpsMirror { url, .. } = source else {
                return Err(adapter_mismatch());
            };
            self.http
                .open_url(url, ctx.token.as_deref(), range, &source.source_id())
                .await
        }
    }
    pub struct HuggingFaceAdapter {
        pub http: HttpClient,
        pub endpoint: String, // e.g. https://huggingface.co
    }

    impl HuggingFaceAdapter {
        fn resolve_url(&self, repo_id: &str, revision: &str, path: &str) -> String {
            format!(
                "{}/{}/resolve/{}/{}",
                self.endpoint.trim_end_matches('/'),
                repo_id.trim_matches('/'),
                revision,
                path.trim_start_matches('/')
            )
        }
    }

    #[async_trait::async_trait]
    impl TransportAdapter for HuggingFaceAdapter {
        fn class(&self) -> SourceClass {
            SourceClass::Huggingface
        }
        async fn probe(
            &self,
            source: &Source,
            _a: &Artifact,
            ctx: &FetchCtx,
        ) -> Result<SourceCaps> {
            let Source::Huggingface {
                repo_id,
                revision,
                path,
                ..
            } = source
            else {
                return Err(adapter_mismatch());
            };
            let url = self.resolve_url(repo_id, revision, path);
            self.http
                .probe_url(&url, ctx.token.as_deref(), &source.source_id())
                .await
        }
        async fn open(
            &self,
            source: &Source,
            _a: &Artifact,
            range: Option<ByteRange>,
            ctx: &FetchCtx,
        ) -> Result<Opened> {
            let Source::Huggingface {
                repo_id,
                revision,
                path,
                ..
            } = source
            else {
                return Err(adapter_mismatch());
            };
            let url = self.resolve_url(repo_id, revision, path);
            self.http
                .open_url(&url, ctx.token.as_deref(), range, &source.source_id())
                .await
        }
    }
    // LAN peering was removed (Atlas is a worldwide service). The `LanPeer`
    // source variant is retained only for back-compat deserialization; it is
    // never planned or fetched, so it has no adapter.
}

#[cfg(feature = "http")]
pub use net::{HttpClient, HttpMirrorAdapter, HuggingFaceAdapter};
#[cfg(feature = "iroh")]
mod iroh_adapter {
    use super::*;
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    use tokio::sync::Mutex;

    /// Fetches a blob over Iroh from the provider node tickets carried by an
    /// `iroh` source (as returned by the worldwide tracker). The node is spawned
    /// lazily on first use (binding a QUIC socket is async) and can be dropped and
    /// re-spawned via [`reset_node`](Self::reset_node) after an interrupted fetch.
    pub struct IrohAdapter {
        node: Mutex<Option<Arc<crate::iroh_node::IrohNode>>>,
        store_dir: std::path::PathBuf,
    }

    impl IrohAdapter {
        pub fn new(store_dir: std::path::PathBuf) -> Self {
            IrohAdapter {
                node: Mutex::new(None),
                store_dir,
            }
        }

        async fn node(&self) -> Result<Arc<crate::iroh_node::IrohNode>> {
            let mut guard = self.node.lock().await;
            if guard.is_none() {
                *guard = Some(Arc::new(
                    crate::iroh_node::IrohNode::spawn(&self.store_dir).await?,
                ));
            }
            Ok(guard.as_ref().unwrap().clone())
        }

        /// Take the cached fetch node out so the next fetch spawns a fresh one.
        /// After an interrupted fetch (Pause/Stop) the node's pooled QUIC
        /// connection to the peer can be left half-open; reusing it makes the next
        /// attempt sit on "connecting" forever, while a fresh node redials cleanly.
        /// The peer's blob store is on disk (`store_dir`), so a kept partial still
        /// resumes. Returns the old node so the caller can tear it down off-path.
        async fn take_node(&self) -> Option<Arc<crate::iroh_node::IrohNode>> {
            self.node.lock().await.take()
        }
    }

    #[async_trait::async_trait]
    impl TransportAdapter for IrohAdapter {
        fn class(&self) -> SourceClass {
            SourceClass::Iroh
        }

        async fn probe(
            &self,
            _s: &Source,
            artifact: &Artifact,
            _c: &FetchCtx,
        ) -> Result<SourceCaps> {
            Ok(SourceCaps {
                supports_range: false,
                size: Some(artifact.size_bytes),
            })
        }

        async fn open(
            &self,
            source: &Source,
            artifact: &Artifact,
            _range: Option<ByteRange>,
            ctx: &FetchCtx,
        ) -> Result<Opened> {
            let Source::Iroh {
                blob_hash, tickets, ..
            } = source
            else {
                return Err(adapter_mismatch());
            };
            if tickets.is_empty() {
                return Err(Error::transport(
                    source.source_id(),
                    TransportErrorKind::Unsupported("iroh source has no provider tickets".into()),
                ));
            }
            let node = self.node().await?;
            let scratch = self.store_dir.join("scratch");
            std::fs::create_dir_all(&scratch).ok();
            let tmp = scratch.join(format!(
                "iroh-{}.tmp",
                &blob_hash[..16.min(blob_hash.len())]
            ));
            // Clear any leftover from a prior aborted attempt: the export step
            // refuses to overwrite, so a stale scratch file is what surfaced as
            // "iroh transfer: export: File exists (os error 17)" on retry.
            if tokio::fs::metadata(&tmp).await.is_ok() {
                let _ = tokio::fs::remove_file(&tmp).await;
            }
            let cancel = ctx.cancel.clone();
            // Forward iroh's live download progress to the UI: the whole blob
            // lands here inside `open()`, so without this the transfer would show
            // no movement until it had already finished.
            let on_bytes = ctx.on_bytes.as_ref().map(|s| s.0.clone());
            // A full BlobTicket string also works; otherwise treat entries as
            // node tickets and fetch the blob by its blake3 hash.
            let result = if blob_hash.len() == 64 {
                node.fetch_from_providers(
                    blob_hash,
                    tickets,
                    &tmp,
                    artifact.size_bytes,
                    cancel,
                    on_bytes,
                )
                .await
            } else {
                node.fetch_to_file(&tickets[0], &tmp, cancel, on_bytes)
                    .await
            };
            if matches!(result, Err(Error::Cancelled)) {
                // Make Stop/Pause near-instant: hand *all* cleanup to a background
                // task and return the moment the fetch aborts. Two steps would
                // otherwise stall the path the user is waiting on — deleting a
                // multi-GB partial blob, and tearing the fetch node down (closing
                // the QUIC endpoint and joining its local pool, which can take
                // seconds or wedge). Taking the node out of the cache here also
                // means the next fetch spawns a clean one, the half-open-connection
                // reset that keeps a later pull from sitting on "connecting".
                //
                // On a Stop (discard) the partial blob in the node's own store is
                // dropped too — the engine's `.part` cleanup can't reach it, so
                // without this Stop would leave the (multi-GB) partial behind.
                let discard = ctx
                    .discard_partial
                    .as_ref()
                    .map(|d| d.load(std::sync::atomic::Ordering::SeqCst))
                    .unwrap_or(false);
                drop(node); // release this call's handle; the cached one is taken next
                let cached = self.take_node().await;
                let tmp = tmp.clone();
                let blob_hash = blob_hash.clone();
                let tickets = tickets.clone();
                tokio::spawn(async move {
                    if let Some(node) = &cached {
                        if discard {
                            let hex = if blob_hash.len() == 64 {
                                Some(blob_hash)
                            } else {
                                crate::iroh_node::IrohNode::ticket_hash(tickets[0].as_str()).ok()
                            };
                            if let Some(hex) = hex {
                                let _ = node.unseed(&hex).await;
                            }
                        }
                        // Bounded close so a wedged endpoint can't pin this task.
                        node.shutdown_handle().await;
                    }
                    if discard {
                        let _ = tokio::fs::remove_file(&tmp).await;
                    }
                    drop(cached);
                });
            }
            // Preserve a user-cancel as `Cancelled` so the engine stops the whole
            // download instead of failing over to the next source. Also pass through
            // an already-classified transport error untouched: the fetch watchdog
            // returns `NotFound` (non-retriable) when providers deliver no data, and
            // re-wrapping it as `Other` would make it retriable — wasting another
            // full connect window on the same dead peers before failing over.
            result.map_err(|e| match e {
                Error::Cancelled => Error::Cancelled,
                e @ Error::Transport { .. } => e,
                other => Error::transport(
                    source.source_id(),
                    TransportErrorKind::Other(other.to_string()),
                ),
            })?;

            let total = tokio::fs::metadata(&tmp).await.ok().map(|m| m.len());
            let file = tokio::fs::File::open(&tmp)
                .await
                .map_err(|e| Error::fs(&tmp, e))?;
            let cleanup = tmp.clone();
            let stream = futures_util::stream::unfold((file, false), move |(mut f, done)| {
                let cleanup = cleanup.clone();
                async move {
                    if done {
                        return None;
                    }
                    let mut buf = vec![0u8; 256 * 1024];
                    match f.read(&mut buf).await {
                        Ok(0) => {
                            let _ = tokio::fs::remove_file(&cleanup).await;
                            None
                        }
                        Ok(n) => {
                            buf.truncate(n);
                            Some((Ok(Bytes::from(buf)), (f, false)))
                        }
                        Err(e) => Some((
                            Err(Error::transport(
                                "iroh",
                                TransportErrorKind::Other(e.to_string()),
                            )),
                            (f, true),
                        )),
                    }
                }
            });
            Ok(Opened {
                stream: Box::pin(stream),
                effective_start: 0,
                total_size: total,
                // Iroh fetches the whole blob during `open()` (above) and
                // reports it live via `on_bytes`; this stream just re-reads the
                // local scratch file for verification.
                prefetched: true,
            })
        }
    }
}

#[cfg(feature = "iroh")]
pub use iroh_adapter::IrohAdapter;

// ===========================================================================
// BitTorrent adapter (librqbit) — magnet → one verified file → CAS
// ===========================================================================

/// Choose which file in a (possibly multi-file) torrent matches the artifact we
/// want. Exact basename+size first; then a size-only fallback *only* when exactly
/// one file has that size (else ambiguous → refuse and let the engine fail over);
/// finally the single-file shortcut. Pure (no I/O, no librqbit types) so it's
/// unit-testable without a swarm.
#[cfg(feature = "bittorrent")]
fn pick_torrent_file(files: &[(String, u64)], want_name: &str, want_size: u64) -> Option<usize> {
    if let Some(i) = files
        .iter()
        .position(|(name, len)| *len == want_size && basename(name).eq_ignore_ascii_case(want_name))
    {
        return Some(i);
    }
    let size_matches: Vec<usize> = files
        .iter()
        .enumerate()
        .filter(|(_, (_, len))| *len == want_size)
        .map(|(i, _)| i)
        .collect();
    if size_matches.len() == 1 {
        return Some(size_matches[0]);
    }
    if files.len() == 1 {
        return Some(0);
    }
    None
}

#[cfg(feature = "bittorrent")]
fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

#[cfg(feature = "bittorrent")]
mod bittorrent_adapter {
    use super::*;
    use librqbit::api::Api;
    use librqbit::{
        AddTorrent, AddTorrentOptions, ConnectionOptions, ListenerMode, ListenerOptions, Session,
        SessionOptions, SessionPersistenceConfig,
    };
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncReadExt;
    use tokio::sync::OnceCell;

    /// librqbit's per-torrent id (a `usize`; not re-exported at the crate root).
    type TorrentId = usize;

    /// Well-known public BitTorrent trackers, layered on top of DHT so a magnet that
    /// only carries an info-hash still finds peers fast. Attached to both the created
    /// torrent's announce-list (seed path) and a magnet-only add (leech path), gated
    /// by `TransportConfig::bittorrent_use_public_trackers`.
    pub const PUBLIC_TRACKERS: &[&str] = &[
        "udp://tracker.opentrackr.org:1337/announce",
        "udp://open.tracker.cl:1337/announce",
        "udp://tracker.openbittorrent.com:6969/announce",
        "udp://exodus.desync.com:6969/announce",
        "udp://tracker.torrent.eu.org:451/announce",
        "udp://open.demonii.com:1337/announce",
        "udp://tracker.dler.org:6969/announce",
        "https://tracker.tamersunion.org:443/announce",
    ];

    /// The public-tracker list to attach, given whether the app proxy is set.
    ///
    /// PRIVACY: librqbit routes only peer/DHT traffic (and `https://` tracker
    /// announces) through a `socks5` proxy — UDP tracker announces go out direct.
    /// So with a proxy ("VPN tunnel") configured, every `udp://` tracker would leak
    /// the user's real IP *and* the model's info-hash to the tracker operator. When
    /// a proxy is set we therefore drop the `udp://` trackers and keep only the
    /// `https://` one (which IS proxied); with no proxy we use the full list.
    fn public_trackers(proxy_set: bool) -> Vec<String> {
        PUBLIC_TRACKERS
            .iter()
            .filter(|t| !proxy_set || t.starts_with("https://"))
            .map(|t| t.to_string())
            .collect()
    }

    /// How long to wait for a magnet's metadata (file list) to resolve from the
    /// swarm/DHT before failing over. A dead magnet never resolves.
    const METADATA_TIMEOUT: Duration = Duration::from_secs(60);
    /// Abort if downloaded bytes don't advance for this long — kills a genuinely
    /// stalled swarm without punishing a slow-but-healthy transfer.
    const STALL_TIMEOUT: Duration = Duration::from_secs(120);

    /// Map a bytes/sec cap to librqbit's `Option<NonZeroU32>` limiter: `0` (or a
    /// value past `u32::MAX`, clamped) means unlimited → `None`.
    fn bps_to_limit(bps: u64) -> Option<std::num::NonZeroU32> {
        std::num::NonZeroU32::new(bps.min(u32::MAX as u64) as u32)
    }

    /// Normalize librqbit's `ConnectionKind` (not nameable outside the crate, but it
    /// `Display`s as `tcp`/`utp`/`socks`) to the UI's `"TCP"`/`"uTP"`/`"unknown"`.
    /// `None` (peer not yet live) maps to `"unknown"`.
    fn conn_kind_label<T: std::fmt::Display>(kind: Option<T>) -> String {
        match kind.map(|k| k.to_string()) {
            Some(s) if s.eq_ignore_ascii_case("tcp") => "TCP".to_string(),
            Some(s) if s.eq_ignore_ascii_case("utp") => "uTP".to_string(),
            Some(other) => other,
            None => "unknown".to_string(),
        }
    }

    /// Fetches ONE file from a BitTorrent swarm by magnet into a scratch dir, then
    /// streams it back for the engine to verify (mirroring the Iroh adapter's
    /// download-to-tempfile-then-stream-back shape; `prefetched: true`). The torrent
    /// is **never trusted**: the engine re-verifies every byte against the signed
    /// manifest, so a v1/hybrid swarm is fine.
    ///
    /// The librqbit `Session` is **persistent** (resume-data + fastresume), so a
    /// paused or interrupted download resumes from its on-disk pieces rather than
    /// restarting. Seeding completed blobs is wired separately (engine-driven, from
    /// a reflink of the CAS blob). The session lazily binds on first use.
    /// A live peer of a managed torrent (seeding or actively leeching), mapped from
    /// librqbit's per-peer stats snapshot for the UI's peer list.
    #[derive(Debug, Clone)]
    pub struct BtPeer {
        pub addr: String,
        /// Transport carrying this connection: `"TCP"`, `"uTP"`, or `"unknown"`.
        pub conn_kind: String,
        /// librqbit's peer state name (e.g. `"live"`).
        pub state: String,
        /// Bytes fetched *from* this peer (we downloaded).
        pub downloaded: u64,
        /// Bytes uploaded *to* this peer (we served).
        pub uploaded: u64,
    }

    pub struct BittorrentAdapter {
        session: OnceCell<Arc<Session>>,
        store_dir: std::path::PathBuf,
        /// App proxy ("VPN tunnel"); only `socks5(h)://` can carry peer/DHT traffic,
        /// so http(s) proxies are ignored here (BT goes direct).
        proxy: Option<String>,
        /// Whether the session binds an inbound listener (so it can seed). Even when
        /// off, downloads work outbound + DHT.
        seed: bool,
        /// Inbound listen-port range (best-effort) when `seed`.
        listen_ports: Option<std::ops::Range<u16>>,
        /// Whether to attempt UPnP/NAT-PMP port mapping when `seed`.
        enable_upnp: bool,
        /// Per-direction rate caps in bytes/sec applied to the librqbit session
        /// (`0` = unlimited). Bytes/sec, clamped to `u32` for librqbit's limiter.
        up_bps: u64,
        down_bps: u64,
        /// Attach the public-tracker list to created torrents and magnet-only adds.
        use_public_trackers: bool,
        /// Seed/upload ratio at which a seeded torrent is paused (`0.0` = unlimited).
        /// Stored as f64 *bits* in an atomic so the UI can raise/lower/clear it at
        /// runtime; lowering or clearing it resumes blobs the watcher had paused.
        max_ratio: Arc<AtomicU64>,
        /// Blake3s the ratio watcher has paused after reaching the lifetime cap. Kept
        /// distinct from a user Pause so `is_seeding` can report them as NOT seeding
        /// (the pill mustn't lie) and so a later cap raise/clear can resume exactly
        /// these — without un-pausing a torrent the user paused by hand.
        ratio_paused: Arc<Mutex<HashSet<String>>>,
        /// Lifetime-upload persistence (engine's `Db`), so the ratio cap is a
        /// lifetime ratio across runs rather than per-session. `None` in builds with
        /// no store (the cap then degrades to per-session, as before).
        upload_store: Option<Arc<dyn BtUploadStore>>,
        /// In-flight + active seeds, keyed by blob blake3. Lets `unseed` find the
        /// added torrent (to delete it + its reflinked seed file) and abort a
        /// still-promoting `seed_promote` task before it adds the torrent.
        seeds: Arc<Mutex<HashMap<String, SeedEntry>>>,
        /// Torrent id of each in-flight *leech* download, keyed by blob blake3, so
        /// `peers_for` can read a downloading swarm's live peers. Inserted when the
        /// download torrent is added and removed when `open` returns.
        active: Arc<Mutex<HashMap<String, TorrentId>>>,
    }

    /// Per-blob seeding state. A single blob can be seeded under more than one file
    /// name (e.g. a post-import rename), each producing a *distinct* torrent
    /// (info-hash includes the file name). We therefore track **all** of them:
    /// `promotions` are the in-flight `seed_promote` tasks (each with its own
    /// `wanted` abort flag), and `torrent_ids` accumulates every added torrent — so
    /// `unseed` tears down ALL of a blob's torrents, not just the most recent (the
    /// leak when this was keyed to a single id).
    #[derive(Default)]
    struct SeedEntry {
        promotions: Vec<Promotion>,
        torrent_ids: Vec<TorrentId>,
    }

    /// One in-flight seed promotion: the `wanted` flag it polls (cleared to abort)
    /// and its task handle.
    struct Promotion {
        wanted: Arc<AtomicBool>,
        task: tokio::task::JoinHandle<()>,
    }

    impl BittorrentAdapter {
        #[allow(clippy::too_many_arguments)]
        pub fn new(
            store_dir: std::path::PathBuf,
            proxy: Option<String>,
            seed: bool,
            listen_ports: Option<std::ops::Range<u16>>,
            enable_upnp: bool,
            up_bps: u64,
            down_bps: u64,
            use_public_trackers: bool,
            max_ratio: f64,
            upload_store: Option<Arc<dyn BtUploadStore>>,
        ) -> Self {
            BittorrentAdapter {
                session: OnceCell::new(),
                store_dir,
                proxy,
                seed,
                listen_ports,
                enable_upnp,
                up_bps,
                down_bps,
                use_public_trackers,
                max_ratio: Arc::new(AtomicU64::new(max_ratio.to_bits())),
                ratio_paused: Arc::new(Mutex::new(HashSet::new())),
                upload_store,
                seeds: Arc::new(Mutex::new(HashMap::new())),
                active: Arc::new(Mutex::new(HashMap::new())),
            }
        }

        /// Whether the app proxy ("VPN tunnel") is configured (non-empty). Drives the
        /// proxy-aware public-tracker filter ([`public_trackers`]): with a proxy set,
        /// `udp://` trackers (which librqbit can't proxy) are dropped to avoid leaking
        /// the user's real IP + the model info-hash around the tunnel.
        fn proxy_set(&self) -> bool {
            // Only a socks5 proxy actually tunnels peer/DHT traffic (see `session`),
            // so only then would a udp tracker leak around it. With any other proxy
            // (or none) dropping udp trackers buys no privacy, just fewer peers.
            self.proxy
                .as_deref()
                .map(str::trim)
                .is_some_and(|p| p.starts_with("socks5"))
        }

        /// Lazily build the persistent session (binds sockets + starts DHT) on first
        /// use. Seeding wants an inbound listener + UPnP; if binding fails (ports
        /// taken) it retries leech-only so downloads still work this run.
        async fn session(&self) -> Result<&Arc<Session>> {
            self.session
                .get_or_try_init(|| async {
                    let scratch = self.store_dir.join("scratch");
                    std::fs::create_dir_all(&scratch).ok();
                    // Only a socks5 proxy can carry peer/DHT traffic.
                    let socks_proxy_url = self
                        .proxy
                        .as_deref()
                        .map(str::trim)
                        .filter(|p| p.starts_with("socks5"))
                        .map(|p| p.to_string());
                    let session_dir = self.store_dir.join("session");
                    // A TCP+uTP listener (BEP-29): uTP lets us reach peers that only
                    // speak uTP (and accept inbound), TCP covers the rest. UPnP port
                    // mapping only when we intend to seed. Honor a configured port.
                    let mut listener = ListenerOptions {
                        mode: ListenerMode::TcpAndUtp,
                        enable_upnp_port_forwarding: self.seed && self.enable_upnp,
                        ..Default::default()
                    };
                    if let Some(range) = &self.listen_ports {
                        listener.listen_addr =
                            (std::net::Ipv6Addr::UNSPECIFIED, range.start).into();
                    }
                    // User-configured speed caps (bytes/sec; `0` = unlimited). librqbit's
                    // limiter is a `NonZeroU32`, so clamp to u32 and map 0 → None.
                    let ratelimits = librqbit::limits::LimitsConfig {
                        upload_bps: bps_to_limit(self.up_bps),
                        download_bps: bps_to_limit(self.down_bps),
                    };
                    let mk_opts = |listen: Option<ListenerOptions>| SessionOptions {
                        listen,
                        // Only a socks5 proxy can carry peer traffic (filtered above).
                        connect: socks_proxy_url.clone().map(|p| ConnectionOptions {
                            proxy_url: Some(p),
                            ..Default::default()
                        }),
                        ratelimits,
                        // Persistent: re-attach in-progress torrents + restore piece
                        // state quickly, so pause/restart resumes rather than restarts.
                        persistence: Some(SessionPersistenceConfig::Json {
                            folder: Some(session_dir.clone()),
                        }),
                        fastresume: true,
                        ..Default::default()
                    };
                    // Best-effort: if binding the listener fails (e.g. the port is
                    // taken), retry with none — DHT + outbound TCP/uTP still work.
                    let mut sess =
                        Session::new_with_opts(scratch.clone(), mk_opts(Some(listener))).await;
                    if sess.is_err() {
                        sess = Session::new_with_opts(scratch.clone(), mk_opts(None)).await;
                    }
                    let sess = sess.map_err(|e| {
                        Error::transport(
                            "bittorrent",
                            TransportErrorKind::Other(format!("session init: {e}")),
                        )
                    })?;
                    // Sweep orphaned per-fetch scratch dirs left by a crash or an
                    // early-dropped stream from a *prior* run. Only delete a `dl-*`
                    // dir that no current torrent points at — a paused/in-progress BT
                    // download keeps its (deterministic) dir for resume, and the
                    // session has just re-attached those persisted torrents, so their
                    // output folders are live here and are preserved.
                    sweep_orphan_scratch(&scratch);
                    // Stop-at-ratio: a lightweight periodic task pauses any seeded
                    // torrent whose *lifetime* upload/size ratio has reached the cap,
                    // and resumes ones it paused if the cap is later lowered/cleared.
                    // Runs whenever we seed (the cap is read live, so it can start at
                    // 0/unlimited and be raised at runtime); the watcher itself no-ops
                    // each tick while the cap is unlimited.
                    if self.seed {
                        tokio::spawn(ratio_watch(
                            sess.clone(),
                            self.seeds.clone(),
                            self.max_ratio.clone(),
                            self.ratio_paused.clone(),
                            self.upload_store.clone(),
                        ));
                    }
                    Ok(sess)
                })
                .await
        }

        /// Seed a verified CAS blob to the swarm: reflink it under its model name
        /// (CoW where supported), build a v1 torrent over it, and add it to the
        /// persistent session (which announces to the DHT). Returns immediately — the
        /// (one-time, cold) session init AND the heavy piece-hashing both run on the
        /// spawned task, so a launch re-seed never stalls the first transfer.
        pub async fn seed_blob(
            self: &Arc<Self>,
            cas_blob: std::path::PathBuf,
            name: String,
            blake3: String,
            on_magnet: Option<Arc<dyn Fn(String) + Send + Sync>>,
        ) -> Result<()> {
            let seed_root = self.store_dir.join("seed");
            // A "still wanted" flag the promotion polls so an `unseed` that races a
            // freshly-spawned `seed_blob` aborts it before it adds the torrent.
            let wanted = Arc::new(AtomicBool::new(true));
            let trackers = if self.use_public_trackers {
                public_trackers(self.proxy_set())
            } else {
                Vec::new()
            };
            let me = self.clone();
            let b3 = blake3.clone();
            let wanted_task = wanted.clone();
            // Resolve the (lazily-bound) session *inside* the task so its cold init
            // (binding sockets, starting DHT/UPnP) is off the caller's path, then run
            // the promotion. A session-init failure just means "don't seed this one".
            let task = tokio::spawn(async move {
                let session = match me.session().await {
                    Ok(s) => s.clone(),
                    Err(e) => {
                        tracing::warn!(error = %e, "bittorrent: seed session init failed");
                        return;
                    }
                };
                seed_promote(
                    session,
                    cas_blob,
                    name,
                    b3,
                    seed_root,
                    on_magnet,
                    me.seeds.clone(),
                    wanted_task,
                    trackers,
                    me.max_ratio.clone(),
                    me.ratio_paused.clone(),
                    me.upload_store.clone(),
                )
                .await;
            });
            // Append this promotion to the blob's entry rather than replacing it: a
            // blob can be seeded under more than one file name (distinct torrents),
            // and dropping the prior entry would orphan its already-added torrent.
            self.seeds
                .lock()
                .unwrap()
                .entry(blake3)
                .or_default()
                .promotions
                .push(Promotion { wanted, task });
            Ok(())
        }

        /// Whether this blob is *managed* by the live session (a seed torrent or an
        /// in-flight promotion exists) — including one the ratio watcher has paused.
        /// Used to skip a redundant re-seed so a duplicate promotion isn't spawned;
        /// the UI-facing signal is [`is_actively_seeding`](Self::is_actively_seeding).
        /// Note a persisted magnet is NOT the signal — after a restart the in-memory
        /// map is empty even for blobs we should re-add (the launch sweep's job).
        pub fn is_seeding(&self, blake3: &str) -> bool {
            self.seeds.lock().unwrap().contains_key(blake3)
        }

        /// Whether this blob is *actively* seeding right now — managed AND not paused
        /// by the stop-at-ratio watcher. The UI's "Seeding" pill reads this so a
        /// ratio-paused seed isn't shown as seeding (it's uploading nothing).
        pub fn is_actively_seeding(&self, blake3: &str) -> bool {
            self.seeds.lock().unwrap().contains_key(blake3)
                && !self.ratio_paused.lock().unwrap().contains(blake3)
        }

        /// Set the stop-at-ratio cap at runtime (`0.0` = unlimited). Lowering or
        /// clearing it lets the watcher resume blobs it had paused on the next tick;
        /// raising it lets newly-over-cap blobs pause. The cap is read live by the
        /// watcher (started whenever we seed), so this needs no session rebuild.
        pub fn set_max_ratio(&self, max_ratio: f64) {
            self.max_ratio.store(max_ratio.to_bits(), Ordering::Relaxed);
        }

        /// Live per-peer snapshot for a blob's managed torrent — whichever of its
        /// seeded torrents or its in-flight leech is live. Empty when the blob isn't
        /// managed here, the session isn't up yet, or the torrent isn't live (no
        /// peers). Reads through librqbit's public `Api`, filtered to live peers.
        pub fn peers_for(&self, blake3: &str) -> Vec<BtPeer> {
            let Some(session) = self.session.get().cloned() else {
                return Vec::new();
            };
            // Seeders can have several torrents per blob (one per file name); a
            // leecher has at most one. Try every candidate id and merge their peers.
            let mut ids: Vec<TorrentId> = self
                .seeds
                .lock()
                .unwrap()
                .get(blake3)
                .map(|e| e.torrent_ids.clone())
                .unwrap_or_default();
            if let Some(id) = self.active.lock().unwrap().get(blake3).copied() {
                ids.push(id);
            }
            if ids.is_empty() {
                return Vec::new();
            }
            let api = Api::new(session, None);
            let mut out = Vec::new();
            for id in ids {
                // Default filter == live peers only. A non-live torrent yields an
                // error here (TorrentIsNotLive); treat that as "no peers".
                let Ok(snap) = api.api_peer_stats(id.into(), Default::default()) else {
                    continue;
                };
                out.extend(snap.peers.into_iter().map(|(addr, st)| BtPeer {
                    addr,
                    conn_kind: conn_kind_label(st.conn_kind),
                    state: st.state.to_string(),
                    downloaded: st.counters.fetched_bytes,
                    uploaded: st.counters.uploaded_bytes,
                }));
            }
            out
        }

        /// Stop BitTorrent-seeding a blob: abort any in-flight promotion, then
        /// remove the seeded torrent and its reflinked seed file from the session.
        /// No-op if the blob isn't being seeded.
        pub async fn unseed(&self, blake3: &str) {
            let entry = self.seeds.lock().unwrap().remove(blake3);
            self.ratio_paused.lock().unwrap().remove(blake3);
            let Some(entry) = entry else { return };
            // Tell every still-running promotion to abort (each polls this after its
            // steps and won't add a torrent), then abort the tasks outright.
            for p in entry.promotions {
                p.wanted.store(false, Ordering::SeqCst);
                p.task.abort();
            }
            // Delete every torrent already added for this blob (+ its reflinked file).
            if !entry.torrent_ids.is_empty() {
                if let Ok(session) = self.session().await {
                    for id in entry.torrent_ids {
                        let _ = session.delete(id.into(), true).await;
                    }
                }
            }
        }

        /// Synchronous unseed for sync engine paths (evict/delete): drop the entry
        /// and abort any in-flight promotions now, and — if any torrent was already
        /// added and the session is live — spawn their deletion onto the current
        /// runtime. Best-effort; the persisted magnet is cleared by the caller.
        pub fn unseed_detached(&self, blake3: &str) {
            let entry = self.seeds.lock().unwrap().remove(blake3);
            self.ratio_paused.lock().unwrap().remove(blake3);
            let Some(entry) = entry else { return };
            for p in entry.promotions {
                p.wanted.store(false, Ordering::SeqCst);
                p.task.abort();
            }
            if !entry.torrent_ids.is_empty() {
                if let Some(session) = self.session.get().cloned() {
                    if let Ok(handle) = tokio::runtime::Handle::try_current() {
                        handle.spawn(async move {
                            for id in entry.torrent_ids {
                                let _ = session.delete(id.into(), true).await;
                            }
                        });
                    }
                }
            }
        }
    }

    /// Outcome of waiting for a torrent: finished, user-cancelled, or failed.
    enum Awaited {
        Done,
        Cancelled,
        Failed(TransportErrorKind),
    }

    /// Removes a per-fetch scratch download dir when dropped — i.e. whenever the
    /// verify-back stream ends, including the engine dropping it early on a verify
    /// failure or error (which the old EOF-only cleanup leaked). A `None` payload
    /// (e.g. after the dir was already removed) is a no-op.
    struct ScratchGuard(Option<std::path::PathBuf>);
    impl Drop for ScratchGuard {
        fn drop(&mut self) {
            if let Some(dir) = self.0.take() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    /// Removes a blob's in-flight-leech entry from the `active` registry when the
    /// `open` call ends, so `peers_for` never reports a stale (finished or aborted)
    /// download's torrent.
    struct ActiveGuard {
        active: Arc<Mutex<HashMap<String, TorrentId>>>,
        blake3: String,
    }
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.active.lock().unwrap().remove(&self.blake3);
        }
    }

    /// Remove leaked, *empty* `dl-*` per-fetch scratch dirs under `scratch` at
    /// session init. An empty `dl-*` dir can only be a shell left by a crash or an
    /// early-dropped stream (the `create_dir_all` ran but no data file remains) — a
    /// paused/in-progress BT download always has its partial file inside, so it's
    /// never empty and is preserved for resume. librqbit doesn't expose a live
    /// torrent's output path publicly, so "is it empty" is the conservative,
    /// resume-safe orphan test; the in-process `ScratchGuard` handles the common
    /// case (verify-fail / early drop) within a run. The librqbit persistence dir
    /// lives outside `scratch`, so it's never considered here.
    fn sweep_orphan_scratch(scratch: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir(scratch) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_dl = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("dl-"))
                .unwrap_or(false);
            if !is_dl || !path.is_dir() {
                continue;
            }
            // Empty (no partial inside) ⇒ a leaked shell, safe to remove. A dir that
            // still holds a (possibly partial) download file is left for resume.
            let empty = std::fs::read_dir(&path)
                .map(|mut e| e.next().is_none())
                .unwrap_or(false);
            if empty {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }

    /// Periodically enforce the stop-at-ratio cap against each blob's **lifetime**
    /// upload/size ratio: pause a blob's seed torrents once over the cap, and resume
    /// the ones it paused if the cap is later lowered or cleared. Cheap: one stats
    /// read per seeded torrent every 30s. Runs for the life of the session, reading
    /// the cap live so a runtime change takes effect on the next tick.
    ///
    /// Lifetime ratio = `persisted_baseline + this_session_upload` over the blob
    /// size, so the cap survives restarts (librqbit's upload counter is per-session).
    /// The persisted total is refreshed each tick; `baseline` is read once per blob
    /// and cached here so the in-session refresh never double-counts.
    async fn ratio_watch(
        session: Arc<Session>,
        seeds: Arc<Mutex<HashMap<String, SeedEntry>>>,
        max_ratio: Arc<AtomicU64>,
        ratio_paused: Arc<Mutex<HashSet<String>>>,
        upload_store: Option<Arc<dyn BtUploadStore>>,
    ) {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        // Per-blob persisted-upload baseline, read once and held for the session.
        let mut baselines: HashMap<String, u64> = HashMap::new();
        loop {
            tick.tick().await;
            let cap = f64::from_bits(max_ratio.load(Ordering::Relaxed));
            // Snapshot (blake3, ids) under the lock, then act without holding it.
            let blobs: Vec<(String, Vec<TorrentId>)> = {
                let guard = seeds.lock().unwrap();
                guard
                    .iter()
                    .filter(|(_, e)| !e.torrent_ids.is_empty())
                    .map(|(b3, e)| (b3.clone(), e.torrent_ids.clone()))
                    .collect()
            };
            for (blake3, ids) in blobs {
                // Sum this session's upload across the blob's torrents; size is the
                // (shared) blob size. Skip a blob with no live/known torrents.
                let mut session_uploaded = 0u64;
                let mut size = 0u64;
                let mut handles = Vec::new();
                for id in &ids {
                    let Some(handle) = session.get((*id).into()) else {
                        continue;
                    };
                    let stats = handle.stats();
                    session_uploaded += stats.uploaded_bytes;
                    size = size.max(stats.total_bytes);
                    handles.push((*id, handle, stats.live.is_some()));
                }
                if handles.is_empty() || size == 0 {
                    continue;
                }
                // Lifetime upload = persisted baseline (read once) + this session.
                let baseline = match upload_store.as_ref() {
                    Some(store) => *baselines
                        .entry(blake3.clone())
                        .or_insert_with(|| store.load_uploaded(&blake3)),
                    None => 0,
                };
                let lifetime = baseline + session_uploaded;
                if let Some(store) = upload_store.as_ref() {
                    store.store_uploaded(&blake3, lifetime);
                }
                let ratio = lifetime as f64 / size as f64;
                let over_cap = cap > 0.0 && ratio >= cap;
                if over_cap {
                    let mut paused_any = false;
                    for (id, handle, live) in &handles {
                        if *live && session.pause(handle).await.is_ok() {
                            paused_any = true;
                            tracing::info!(
                                torrent_id = id,
                                ratio = format!("{ratio:.2}"),
                                max_ratio = cap,
                                "bittorrent: stop-at-ratio reached, pausing seed"
                            );
                        }
                    }
                    if paused_any {
                        ratio_paused.lock().unwrap().insert(blake3.clone());
                    }
                } else if ratio_paused.lock().unwrap().contains(&blake3) {
                    // Cap was lowered/cleared (or never reached now): resume exactly
                    // the torrents the watcher paused, and forget the paused mark.
                    for (id, handle, _) in &handles {
                        if session.unpause(handle).await.is_ok() {
                            tracing::info!(
                                torrent_id = id,
                                "bittorrent: ratio cap relaxed, resuming seed"
                            );
                        }
                    }
                    ratio_paused.lock().unwrap().remove(&blake3);
                }
            }
        }
    }

    #[async_trait::async_trait]
    impl TransportAdapter for BittorrentAdapter {
        fn class(&self) -> SourceClass {
            SourceClass::BittorrentV2
        }

        async fn probe(
            &self,
            _s: &Source,
            artifact: &Artifact,
            _c: &FetchCtx,
        ) -> Result<SourceCaps> {
            // Swarms fetch whole-file (out of order); no HTTP-style ranges.
            Ok(SourceCaps {
                supports_range: false,
                size: Some(artifact.size_bytes),
            })
        }

        async fn open(
            &self,
            source: &Source,
            artifact: &Artifact,
            _range: Option<ByteRange>,
            ctx: &FetchCtx,
        ) -> Result<Opened> {
            let Source::BittorrentV2 { magnet_uri, .. } = source else {
                return Err(adapter_mismatch());
            };
            let session = self.session().await?;
            // Layer the public trackers onto a magnet-only add (gated), so a magnet
            // that carries no announce-list still joins them beyond DHT. With a proxy
            // set, only the proxied https:// tracker is attached (udp:// would leak).
            let trackers = if self.use_public_trackers {
                Some(public_trackers(self.proxy_set()))
            } else {
                None
            };
            // Cap the metadata wait at METADATA_TIMEOUT: `ctx.timeout` is the whole
            // download budget (~5 min), so without the `.min` a dead magnet that
            // never resolves would tie this source up for the full budget instead
            // of failing over after a minute.
            let meta_timeout = ctx
                .timeout
                .map(|t| t.min(METADATA_TIMEOUT))
                .unwrap_or(METADATA_TIMEOUT);

            // --- 1. Resolve metadata (list_only) to choose the right file. -------
            let listed = tokio::time::timeout(
                meta_timeout,
                session.add_torrent(
                    AddTorrent::from_url(magnet_uri.as_str()),
                    Some(AddTorrentOptions {
                        list_only: true,
                        trackers: trackers.clone(),
                        ..Default::default()
                    }),
                ),
            )
            .await
            .map_err(|_| transport(source, TransportErrorKind::Timeout))?
            .map_err(|e| transport(source, TransportErrorKind::Other(e.to_string())))?;
            let info = match listed {
                librqbit::AddTorrentResponse::ListOnly(r) => r.info,
                _ => {
                    return Err(transport(
                        source,
                        TransportErrorKind::Other("expected a file listing".into()),
                    ))
                }
            };
            let files: Vec<(String, u64)> = info
                .iter_file_details()
                .map(|fd| {
                    let name = fd.filename.to_pathbuf().to_string_lossy().into_owned();
                    (name, fd.len)
                })
                .collect();
            let want_name = basename(&artifact.path);
            let Some(file_idx) = pick_torrent_file(&files, want_name, artifact.size_bytes) else {
                return Err(transport(source, TransportErrorKind::NotFound));
            };

            // --- 2. Download just that file into a deterministic per-magnet folder
            //        (stable across runs so a resume reuses on-disk pieces). -------
            let out_dir = self.store_dir.join("scratch").join(format!(
                "dl-{}-{}",
                hex::encode(&blake3::hash(magnet_uri.as_bytes()).as_bytes()[..8]),
                file_idx
            ));
            std::fs::create_dir_all(&out_dir).map_err(|e| Error::fs(&out_dir, e))?;
            let added = session
                .add_torrent(
                    AddTorrent::from_url(magnet_uri.as_str()),
                    Some(AddTorrentOptions {
                        only_files: Some(vec![file_idx]),
                        output_folder: Some(out_dir.to_string_lossy().into_owned()),
                        overwrite: true,
                        trackers: trackers.clone(),
                        ..Default::default()
                    }),
                )
                .await
                .map_err(|e| transport(source, TransportErrorKind::Other(e.to_string())))?;
            let handle = added.into_handle().ok_or_else(|| {
                transport(
                    source,
                    TransportErrorKind::Other("torrent produced no handle".into()),
                )
            })?;
            // A re-added torrent (librqbit returns the existing handle for a known
            // info-hash) or one reloaded from persistence can come back PAUSED from a
            // prior user Pause; the add does NOT unpause it. Resume it before awaiting,
            // or it sits paused, makes no progress, trips the stall watchdog, and the
            // stall handler deletes the partial — i.e. Pause→Resume and restart-resume
            // would destroy the very data they're meant to keep. No-op for a fresh add.
            let _ = session.unpause(&handle).await;

            // Register this leech under its blob blake3 so `peers_for` can read its
            // live swarm while it downloads. The guard removes the entry on every
            // exit path (done, cancel, fail, early-drop). No-op when the manifest
            // carries no blake3 for this artifact (e.g. sha256-only sources).
            let _active = (!artifact.hashes.blake3.is_empty()).then(|| {
                self.active
                    .lock()
                    .unwrap()
                    .insert(artifact.hashes.blake3.clone(), handle.id());
                ActiveGuard {
                    active: self.active.clone(),
                    blake3: artifact.hashes.blake3.clone(),
                }
            });

            // --- 3. Await completion: stall watchdog + live progress + cancel. ----
            match await_completion(&handle, STALL_TIMEOUT, ctx, artifact.size_bytes).await {
                Awaited::Done => {}
                Awaited::Cancelled => {
                    // Stop discards (delete torrent + files); Pause keeps the pieces
                    // (pause the torrent in the persistent session) for instant resume.
                    let stop = ctx
                        .discard_partial
                        .as_ref()
                        .map(|d| d.load(Ordering::SeqCst))
                        .unwrap_or(false);
                    if stop {
                        let _ = session.delete(handle.id().into(), true).await;
                        let _ = tokio::fs::remove_dir_all(&out_dir).await;
                    } else {
                        let _ = session.pause(&handle).await;
                    }
                    return Err(Error::Cancelled);
                }
                Awaited::Failed(kind) => {
                    // Genuine failure (dead swarm / stall / error): keep the partial and
                    // pause the torrent so a retry resumes from its pieces — unless the
                    // user asked to discard (Stop). The engine re-verifies whatever
                    // finally completes, so a kept partial is always safe.
                    let stop = ctx
                        .discard_partial
                        .as_ref()
                        .map(|d| d.load(Ordering::SeqCst))
                        .unwrap_or(false);
                    if stop {
                        let _ = session.delete(handle.id().into(), true).await;
                        let _ = tokio::fs::remove_dir_all(&out_dir).await;
                    } else {
                        let _ = session.pause(&handle).await;
                    }
                    return Err(transport(source, kind));
                }
            }

            // --- 4. Resolve the on-disk path & sanity-check the size. ------------
            let relative = handle
                .with_metadata(|m| m.file_infos[file_idx].relative_filename.clone())
                .map_err(|e| transport(source, TransportErrorKind::Other(e.to_string())))?;
            let tmp = out_dir.join(&relative);
            let total = tokio::fs::metadata(&tmp).await.ok().map(|m| m.len());
            if total != Some(artifact.size_bytes) {
                let _ = session.delete(handle.id().into(), true).await;
                let _ = tokio::fs::remove_dir_all(&out_dir).await;
                return Err(transport(
                    source,
                    TransportErrorKind::Other(format!(
                        "downloaded file size {} != expected {}",
                        total.unwrap_or(0),
                        artifact.size_bytes
                    )),
                ));
            }

            // --- 5. Leech-only for now (Phase 4 adds CAS-reflink seeding): drop the
            //        torrent and delete the scratch tree once streamed back. -------
            let _ = session.delete(handle.id().into(), false).await;
            let file = tokio::fs::File::open(&tmp)
                .await
                .map_err(|e| Error::fs(&tmp, e))?;
            // A Drop guard tied to the stream state removes the scratch dir whenever
            // the stream ends — EOF, a read error, OR the engine dropping it early
            // (a verify failure or any error after step 5). The previous EOF-only
            // removal leaked the dir on every early drop.
            let guard = ScratchGuard(Some(out_dir.clone()));
            let stream =
                futures_util::stream::unfold((file, false, guard), move |(mut f, done, guard)| {
                    async move {
                        if done {
                            return None;
                        }
                        let mut buf = vec![0u8; 256 * 1024];
                        match f.read(&mut buf).await {
                            Ok(0) => None, // guard drops here → scratch dir removed
                            Ok(n) => {
                                buf.truncate(n);
                                Some((Ok(Bytes::from(buf)), (f, false, guard)))
                            }
                            Err(e) => Some((
                                Err(Error::transport(
                                    "bittorrent",
                                    TransportErrorKind::Other(e.to_string()),
                                )),
                                (f, true, guard),
                            )),
                        }
                    }
                });
            Ok(Opened {
                stream: Box::pin(stream),
                effective_start: 0,
                total_size: total,
                // Whole file fetched here; the engine's read loop is a verify pass.
                prefetched: true,
            })
        }
    }

    /// Wait for the torrent to finish, forwarding live byte progress to the UI and
    /// honoring a user Pause/Stop (`ctx.cancel`). Aborts if downloaded bytes don't
    /// advance for `stall` (distinguishes "slow but alive" from "dead swarm").
    async fn await_completion(
        handle: &Arc<librqbit::ManagedTorrent>,
        stall: Duration,
        ctx: &FetchCtx,
        total: u64,
    ) -> Awaited {
        let done = handle.wait_until_completed();
        tokio::pin!(done);
        let mut last_bytes = 0u64;
        let mut last_advance = tokio::time::Instant::now();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.tick().await; // consume the immediate first tick
        loop {
            tokio::select! {
                r = &mut done => {
                    return match r {
                        Ok(_) => Awaited::Done,
                        Err(e) => Awaited::Failed(TransportErrorKind::Other(e.to_string())),
                    };
                }
                _ = tick.tick() => {
                    if ctx.cancel.as_ref().map(|c| c.load(Ordering::SeqCst)).unwrap_or(false) {
                        return Awaited::Cancelled;
                    }
                    let stats = handle.stats();
                    if let Some(err) = stats.error {
                        return Awaited::Failed(TransportErrorKind::Other(err));
                    }
                    if let Some(sink) = &ctx.on_stats {
                        let peers = stats
                            .live
                            .as_ref()
                            .map(|l| l.snapshot.peer_stats.live)
                            .unwrap_or(0);
                        sink.report(LiveStats {
                            bytes_done: stats.progress_bytes.min(total),
                            bytes_total: total,
                            peers,
                            uploaded_bytes: stats.uploaded_bytes,
                        });
                    }
                    if stats.finished {
                        return Awaited::Done;
                    }
                    if stats.progress_bytes > last_bytes {
                        last_bytes = stats.progress_bytes;
                        last_advance = tokio::time::Instant::now();
                    } else if last_advance.elapsed() >= stall {
                        return Awaited::Failed(TransportErrorKind::Timeout);
                    }
                }
            }
        }
    }

    /// Background seed promotion: reflink the CAS blob under its model name, create a
    /// v1 torrent over it, and add it to the session to seed (DHT-announced). Every
    /// step degrades gracefully — a failure just means "don't seed this one".
    ///
    /// `wanted` is polled after each long step; a concurrent [`unseed`] clears it to
    /// abort the promotion before the torrent is added (a race the detached spawn
    /// would otherwise lose, leaking a seed for a model the user just stopped). If we
    /// do add it, its id is recorded in `seeds` so a *later* unseed can delete it.
    #[allow(clippy::too_many_arguments)]
    async fn seed_promote(
        session: Arc<Session>,
        cas_blob: std::path::PathBuf,
        name: String,
        blake3: String,
        seed_root: std::path::PathBuf,
        on_magnet: Option<Arc<dyn Fn(String) + Send + Sync>>,
        seeds: Arc<Mutex<HashMap<String, SeedEntry>>>,
        wanted: Arc<AtomicBool>,
        trackers: Vec<String>,
        max_ratio: Arc<AtomicU64>,
        ratio_paused: Arc<Mutex<HashSet<String>>>,
        upload_store: Option<Arc<dyn BtUploadStore>>,
    ) {
        let fname = {
            let b = super::basename(&name);
            if b.is_empty() {
                "model.bin"
            } else {
                b
            }
        };
        let seed_dir = seed_root.join(&blake3[..16.min(blake3.len())]);
        let seed_file = seed_dir.join(fname);
        if std::fs::create_dir_all(&seed_dir).is_err() {
            return;
        }
        // Reflink the verified blob under its model name (librqbit seeds by the
        // torrent's file name, not the CAS `<blake3>.blob`). CoW where supported;
        // make it writable since librqbit opens the seed file read+write.
        let (src, dst) = (cas_blob, seed_file.clone());
        let linked = matches!(
            tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                // Skip the reflink only if a prior seed file is present *and* the
                // right length: a truncated/partial leftover from an aborted seed
                // would otherwise be trusted and seeded as a corrupt file.
                let src_len = std::fs::metadata(&src).map(|m| m.len()).unwrap_or(0);
                if std::fs::metadata(&dst).map(|m| m.len()).ok() == Some(src_len) && src_len > 0 {
                    return Ok(()); // already reflinked (idempotent re-seed)
                }
                let _ = std::fs::remove_file(&dst);
                reflink_copy::reflink_or_copy(&src, &dst)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o644));
                }
                Ok(())
            })
            .await,
            Ok(Ok(()))
        );
        if !linked || !wanted.load(Ordering::SeqCst) {
            return;
        }
        let created = match librqbit::create_torrent(
            &seed_file,
            librqbit::CreateTorrentOptions {
                name: Some(fname),
                // Carries into the torrent's announce-list AND, mirrored below, into
                // the generated magnet — public trackers beyond DHT.
                trackers: trackers.clone(),
                ..Default::default()
            },
            &librqbit::spawn_utils::BlockingSpawner::new(1),
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "bittorrent: create_torrent failed");
                return;
            }
        };
        let bytes = match created.as_bytes() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "bittorrent: serialize torrent failed");
                return;
            }
        };
        if !wanted.load(Ordering::SeqCst) {
            return;
        }
        // DHT-discoverable magnet, carrying the same public trackers as the torrent's
        // announce-list (when enabled) so a leecher joins them from the magnet alone.
        let magnet =
            librqbit::Magnet::from_id20(created.info_hash(), trackers.clone(), None).to_string();
        if let Some(cb) = on_magnet {
            cb(magnet.clone());
        }
        // Last check before the irreversible add: if unseed fired during torrent
        // creation, don't add (it can't be aborted by the JoinHandle once added).
        if !wanted.load(Ordering::SeqCst) {
            return;
        }
        match session
            .add_torrent(
                AddTorrent::from_bytes(bytes),
                Some(AddTorrentOptions {
                    output_folder: Some(seed_dir.to_string_lossy().into_owned()),
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await
        {
            Ok(added) => {
                tracing::info!(%magnet, blake3 = %blake3, "bittorrent: seeding blob");
                // Record the id so a later unseed can delete it; but if unseed
                // already removed our entry (raced past the final check), undo the
                // add ourselves so we don't leak a seed. Decide while holding the
                // lock, drop it, *then* await the rollback delete (the guard isn't
                // Send so it can't cross the await). Match this promotion by its own
                // `wanted` Arc and drop it from `promotions` (it's done) while
                // appending the torrent id to the blob's accumulated list.
                let handle = added.into_handle();
                let id = handle.as_ref().map(|h| h.id());
                let kept = {
                    let mut guard = seeds.lock().unwrap();
                    match guard.get_mut(&blake3) {
                        Some(entry) if wanted.load(Ordering::SeqCst) => {
                            entry
                                .promotions
                                .retain(|p| !Arc::ptr_eq(&p.wanted, &wanted));
                            if let Some(id) = id {
                                entry.torrent_ids.push(id);
                            }
                            true
                        }
                        _ => false,
                    }
                };
                if !kept {
                    if let Some(id) = id {
                        let _ = session.delete(id.into(), true).await;
                    }
                    return;
                }
                // A re-added persisted torrent can come back PAUSED from a prior
                // session's stop-at-ratio. Resume it UNLESS its *lifetime* ratio is
                // still at/over the current cap — otherwise a once-capped seed would
                // stay dead forever (no resume path), and a cleared/raised cap would
                // never take effect until the next watcher tick. Mark it ratio-paused
                // when we leave it paused so the UI and the watcher agree.
                if let Some(handle) = handle {
                    let cap = f64::from_bits(max_ratio.load(Ordering::Relaxed));
                    let stats = handle.stats();
                    let size = stats.total_bytes.max(1);
                    let baseline = upload_store
                        .as_ref()
                        .map(|s| s.load_uploaded(&blake3))
                        .unwrap_or(0);
                    let ratio = (baseline + stats.uploaded_bytes) as f64 / size as f64;
                    if cap > 0.0 && ratio >= cap {
                        let _ = session.pause(&handle).await;
                        ratio_paused.lock().unwrap().insert(blake3.clone());
                    } else {
                        let _ = session.unpause(&handle).await;
                        ratio_paused.lock().unwrap().remove(&blake3);
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "bittorrent: seed add_torrent failed"),
        }
    }
}

#[cfg(feature = "bittorrent")]
pub use bittorrent_adapter::{BittorrentAdapter, BtPeer};

/// A live peer of a managed BitTorrent torrent (stub when the adapter isn't built
/// in, so `bt_peers` and its callers compile without the `bittorrent` feature).
#[cfg(not(feature = "bittorrent"))]
#[derive(Debug, Clone)]
pub struct BtPeer {
    pub addr: String,
    pub conn_kind: String,
    pub state: String,
    pub downloaded: u64,
    pub uploaded: u64,
}

/// Holds one adapter per supported source class and dispatches by class.
pub struct Transports {
    local: LocalFileAdapter,
    #[cfg(feature = "http")]
    mirror: net::HttpMirrorAdapter,
    #[cfg(feature = "http")]
    hf: net::HuggingFaceAdapter,
    #[cfg(feature = "iroh")]
    iroh: iroh_adapter::IrohAdapter,
    // `Arc` so the seed path can detach session init off the caller's critical path
    // (the launch re-seed must not stall the first transfer on a cold session).
    #[cfg(feature = "bittorrent")]
    bittorrent: std::sync::Arc<bittorrent_adapter::BittorrentAdapter>,
    /// Master switch: when false, the BitTorrent transport is treated as
    /// unsupported for fetching, so the planner fails over to Iroh/HTTP and a
    /// manifest-carried magnet source is skipped (it never joins a swarm).
    #[cfg(feature = "bittorrent")]
    bittorrent_enabled: bool,
}

/// Construction options for the transport registry.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub request_timeout: Duration,
    /// Hugging Face Hub origin used for *downloads* (resolve URLs). Set this to an
    /// HF mirror (e.g. `https://hf-mirror.com`) to fetch weights through it.
    pub hf_endpoint: String,
    /// Directory for the Iroh blob store (used by the iroh fetch adapter).
    pub iroh_store_dir: std::path::PathBuf,
    /// Optional proxy ("VPN tunnel") for all HTTP-family transports. Accepts
    /// `http://`, `https://`, or `socks5://` / `socks5h://`. `None` (or empty)
    /// means a direct connection (reqwest still honors system proxy env vars).
    pub proxy: Option<String>,
    /// Directory for the BitTorrent session: librqbit's own persistence (piece
    /// resume-data) plus per-fetch scratch download folders.
    pub bittorrent_store_dir: std::path::PathBuf,
    /// CAS `blake3` blob directory, so completed blobs can be reflink-seeded over
    /// BitTorrent without a second on-disk copy. Set by the engine to `<root>/cas/blake3`.
    pub bittorrent_cas_dir: std::path::PathBuf,
    /// Master switch for the BitTorrent transport (download + seed). On by default.
    pub bittorrent_enabled: bool,
    /// Whether to seed completed, publicly-redistributable blobs over BitTorrent.
    pub bittorrent_seed: bool,
    /// Inbound listen-port range (best-effort; falls back to outbound + DHT only
    /// when none are free). `None` disables inbound listening.
    pub bittorrent_listen_port_range: Option<std::ops::Range<u16>>,
    /// Enable UPnP / NAT-PMP port mapping for inbound BitTorrent connectivity.
    pub bittorrent_enable_upnp: bool,
    /// Per-direction BitTorrent rate caps in bytes/sec (`0` = unlimited).
    pub bittorrent_max_up_bps: u64,
    pub bittorrent_max_down_bps: u64,
    /// Attach the well-known public-tracker list to created torrents (seed path) and
    /// magnet-only adds (leech path), for peer discovery beyond DHT. On by default.
    pub bittorrent_use_public_trackers: bool,
    /// Stop seeding a torrent once its upload/size ratio reaches this value
    /// (`0.0` = unlimited / never stop on ratio).
    pub bittorrent_max_ratio: f64,
}

impl Default for TransportConfig {
    fn default() -> Self {
        TransportConfig {
            request_timeout: Duration::from_secs(300),
            hf_endpoint: "https://huggingface.co".to_string(),
            iroh_store_dir: std::env::temp_dir().join("noema-iroh-store"),
            proxy: None,
            bittorrent_store_dir: std::env::temp_dir().join("noema-bittorrent-store"),
            bittorrent_cas_dir: std::env::temp_dir().join("noema-cas-blake3"),
            bittorrent_enabled: true,
            bittorrent_seed: true,
            bittorrent_listen_port_range: Some(6881..6892),
            bittorrent_enable_upnp: true,
            bittorrent_max_up_bps: 0,
            bittorrent_max_down_bps: 0,
            bittorrent_use_public_trackers: true,
            bittorrent_max_ratio: 0.0,
        }
    }
}

impl Transports {
    /// Build the transport registry. `bt_upload_store` persists each blob's lifetime
    /// BitTorrent upload (for the stop-at-ratio cap); pass `None` to keep the cap
    /// per-session. Ignored unless the `bittorrent` feature is built.
    pub fn new(
        cfg: &TransportConfig,
        bt_upload_store: Option<std::sync::Arc<dyn BtUploadStore>>,
    ) -> Result<Self> {
        // Internet-facing transports (HF, mirror) ride the optional proxy
        #[cfg(feature = "http")]
        let http = net::HttpClient::new(cfg.request_timeout, cfg.proxy.as_deref())?;
        #[cfg(not(feature = "bittorrent"))]
        let _ = bt_upload_store;
        Ok(Transports {
            local: LocalFileAdapter,
            #[cfg(feature = "http")]
            mirror: net::HttpMirrorAdapter { http: http.clone() },
            #[cfg(feature = "http")]
            hf: net::HuggingFaceAdapter {
                http,
                endpoint: cfg.hf_endpoint.clone(),
            },
            #[cfg(feature = "iroh")]
            iroh: iroh_adapter::IrohAdapter::new(cfg.iroh_store_dir.clone()),
            #[cfg(feature = "bittorrent")]
            bittorrent: std::sync::Arc::new(bittorrent_adapter::BittorrentAdapter::new(
                cfg.bittorrent_store_dir.clone(),
                cfg.proxy.clone(),
                cfg.bittorrent_seed,
                cfg.bittorrent_listen_port_range.clone(),
                cfg.bittorrent_enable_upnp,
                cfg.bittorrent_max_up_bps,
                cfg.bittorrent_max_down_bps,
                cfg.bittorrent_use_public_trackers,
                cfg.bittorrent_max_ratio,
                bt_upload_store,
            )),
            #[cfg(feature = "bittorrent")]
            bittorrent_enabled: cfg.bittorrent_enabled,
        })
    }

    /// Return the adapter for a source class, or an `Unsupported` error.
    pub fn for_class(&self, class: SourceClass) -> Result<&dyn TransportAdapter> {
        match class {
            SourceClass::LocalFile => Ok(&self.local),
            #[cfg(feature = "http")]
            SourceClass::HttpsMirror => Ok(&self.mirror),
            #[cfg(feature = "http")]
            SourceClass::Huggingface => Ok(&self.hf),
            // LAN peering removed; the variant only exists for back-compat deser
            // and is never planned (see `PlatformProfile::fetch_enabled`).
            SourceClass::LanPeer => Err(Error::Unsupported(
                "LAN peering has been removed — Atlas is worldwide-only".into(),
            )),
            #[cfg(not(feature = "http"))]
            SourceClass::HttpsMirror | SourceClass::Huggingface => Err(Error::Unsupported(
                "HTTP transports disabled (build without `http` feature)".into(),
            )),
            #[cfg(feature = "iroh")]
            SourceClass::Iroh => Ok(&self.iroh),
            #[cfg(not(feature = "iroh"))]
            SourceClass::Iroh => Err(Error::Unsupported(
                "iroh adapter not enabled (build with `--features iroh`)".into(),
            )),
            // The master switch gates the FETCH side too: with BitTorrent off, a
            // BT source is unsupported, so the planner fails over to another route
            // and a manifest-carried magnet never joins a swarm.
            #[cfg(feature = "bittorrent")]
            SourceClass::BittorrentV2 if !self.bittorrent_enabled => {
                Err(Error::Unsupported("BitTorrent is disabled".into()))
            }
            #[cfg(feature = "bittorrent")]
            SourceClass::BittorrentV2 => Ok(self.bittorrent.as_ref()),
            #[cfg(not(feature = "bittorrent"))]
            SourceClass::BittorrentV2 => Err(Error::Unsupported(
                "bittorrent adapter not enabled (build with `--features bittorrent`)".into(),
            )),
        }
    }

    /// Seed a verified CAS blob over BitTorrent (no-op when the adapter isn't built
    /// in). Returns quickly; the actual seeding proceeds in the background.
    pub async fn seed_blob(
        &self,
        cas_blob: std::path::PathBuf,
        name: String,
        blake3: String,
        on_magnet: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>>,
    ) -> Result<()> {
        #[cfg(feature = "bittorrent")]
        self.bittorrent
            .seed_blob(cas_blob, name, blake3, on_magnet)
            .await?;
        #[cfg(not(feature = "bittorrent"))]
        {
            let _ = (cas_blob, name, blake3, on_magnet);
        }
        Ok(())
    }

    /// Stop seeding a blob over BitTorrent (no-op when the adapter isn't built in,
    /// or when the blob wasn't being seeded). Aborts an in-flight seed promotion
    /// and removes the seeded torrent + its reflinked file.
    pub async fn unseed_blob(&self, blake3: &str) -> Result<()> {
        #[cfg(feature = "bittorrent")]
        self.bittorrent.unseed(blake3).await;
        #[cfg(not(feature = "bittorrent"))]
        {
            let _ = blake3;
        }
        Ok(())
    }

    /// Whether a blob is *managed* over BitTorrent in the live session — including a
    /// torrent the ratio watcher has paused (always false when the adapter isn't
    /// built in). Lets the engine skip a redundant re-seed (avoiding a duplicate
    /// promotion); for the UI's "Seeding" state use [`is_actively_seeding_blob`].
    pub fn is_seeding_blob(&self, blake3: &str) -> bool {
        #[cfg(feature = "bittorrent")]
        {
            self.bittorrent.is_seeding(blake3)
        }
        #[cfg(not(feature = "bittorrent"))]
        {
            let _ = blake3;
            false
        }
    }

    /// Whether a blob is *actively* seeding over BitTorrent right now — managed AND
    /// not paused by the stop-at-ratio watcher. The UI reads this so a ratio-paused
    /// seed isn't shown as seeding. Always false without the `bittorrent` feature.
    pub fn is_actively_seeding_blob(&self, blake3: &str) -> bool {
        #[cfg(feature = "bittorrent")]
        {
            self.bittorrent.is_actively_seeding(blake3)
        }
        #[cfg(not(feature = "bittorrent"))]
        {
            let _ = blake3;
            false
        }
    }

    /// Set the BitTorrent stop-at-ratio cap at runtime (`0.0` = unlimited). Lowering
    /// or clearing it resumes seeds the watcher had paused; raising it lets
    /// newly-over-cap seeds pause. No-op without the `bittorrent` feature.
    pub fn set_bittorrent_max_ratio(&self, max_ratio: f64) {
        #[cfg(feature = "bittorrent")]
        {
            self.bittorrent.set_max_ratio(max_ratio);
        }
        #[cfg(not(feature = "bittorrent"))]
        {
            let _ = max_ratio;
        }
    }

    /// Live BitTorrent peers for a blob's managed torrent — its seeded torrent(s) or
    /// its in-flight leech, whichever is live. Empty when BitTorrent is disabled or
    /// not built in, or the blob isn't managed here.
    #[cfg(feature = "bittorrent")]
    pub fn bt_peers(&self, blake3: &str) -> Vec<BtPeer> {
        if !self.bittorrent_enabled {
            return Vec::new();
        }
        self.bittorrent.peers_for(blake3)
    }

    /// Live BitTorrent peers for a blob — always empty without the `bittorrent`
    /// feature, so callers compile unconditionally.
    #[cfg(not(feature = "bittorrent"))]
    pub fn bt_peers(&self, blake3: &str) -> Vec<BtPeer> {
        let _ = blake3;
        Vec::new()
    }

    /// Synchronous unseed for sync engine paths (evict/delete): aborts an in-flight
    /// promotion now and spawns the torrent teardown onto the current runtime.
    pub fn unseed_blob_detached(&self, blake3: &str) {
        #[cfg(feature = "bittorrent")]
        self.bittorrent.unseed_detached(blake3);
        #[cfg(not(feature = "bittorrent"))]
        {
            let _ = blake3;
        }
    }
}

fn adapter_mismatch() -> Error {
    Error::other("adapter invoked with a mismatched source variant")
}

fn transport(source: &Source, kind: TransportErrorKind) -> Error {
    Error::transport(source.source_id(), kind)
}

/// Small extension so we can attach an io error context to a transport error.
trait ErrCtx {
    fn context_io(self, e: std::io::Error) -> Error;
}
impl ErrCtx for Error {
    fn context_io(self, e: std::io::Error) -> Error {
        match self {
            Error::Transport { source_id, kind } => Error::Transport {
                source_id,
                kind: match kind {
                    TransportErrorKind::NotFound => {
                        TransportErrorKind::Other(format!("not found: {e}"))
                    }
                    other => other,
                },
            },
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::tests_support::sample_manifest;
    use futures_util::StreamExt;

    #[tokio::test]
    async fn local_adapter_streams_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.gguf");
        let data = vec![9u8; 100_000];
        tokio::fs::write(&p, &data).await.unwrap();

        let adapter = LocalFileAdapter;
        let source = Source::LocalFile {
            path: p.to_string_lossy().to_string(),
        };
        let artifact = sample_manifest().artifacts[0].clone();
        let caps = adapter
            .probe(&source, &artifact, &FetchCtx::default())
            .await
            .unwrap();
        assert_eq!(caps.size, Some(100_000));

        let opened = adapter
            .open(&source, &artifact, None, &FetchCtx::default())
            .await
            .unwrap();
        let mut got = Vec::new();
        let mut s = opened.stream;
        while let Some(chunk) = s.next().await {
            got.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn local_adapter_resumes_from_offset() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.bin");
        let data = (0..50_000u32).map(|i| i as u8).collect::<Vec<u8>>();
        tokio::fs::write(&p, &data).await.unwrap();
        let adapter = LocalFileAdapter;
        let source = Source::LocalFile {
            path: p.to_string_lossy().to_string(),
        };
        let artifact = sample_manifest().artifacts[0].clone();
        let opened = adapter
            .open(
                &source,
                &artifact,
                Some(ByteRange {
                    start: 20_000,
                    end: None,
                }),
                &FetchCtx::default(),
            )
            .await
            .unwrap();
        assert_eq!(opened.effective_start, 20_000);
        let mut got = Vec::new();
        let mut s = opened.stream;
        while let Some(chunk) = s.next().await {
            got.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(got, data[20_000..]);
    }

    #[cfg(feature = "bittorrent")]
    #[test]
    fn bittorrent_picks_file_by_name_and_size() {
        let files = vec![
            ("readme.txt".to_string(), 100u64),
            ("dir/model-q4_k_m.gguf".to_string(), 4_000_000u64),
            ("dir/model-q8.gguf".to_string(), 8_000_000u64),
        ];
        // Exact basename + size wins, even nested in a torrent sub-directory.
        assert_eq!(
            pick_torrent_file(&files, "model-q4_k_m.gguf", 4_000_000),
            Some(1)
        );
        // Wrong size with the right name is NOT accepted (the bytes must match).
        assert_eq!(pick_torrent_file(&files, "model-q4_k_m.gguf", 123), None);
    }

    #[cfg(feature = "bittorrent")]
    #[test]
    fn bittorrent_size_only_match_must_be_unambiguous() {
        // Unique size, mismatched name (a packer renamed it) → size-only match.
        let unique = vec![("a.bin".to_string(), 10u64), ("b.bin".to_string(), 20u64)];
        assert_eq!(pick_torrent_file(&unique, "weights.gguf", 20), Some(1));
        // Two files share the target size → ambiguous → refuse (fail over).
        let ambiguous = vec![("a.bin".to_string(), 20u64), ("b.bin".to_string(), 20u64)];
        assert_eq!(pick_torrent_file(&ambiguous, "weights.gguf", 20), None);
    }

    #[cfg(feature = "bittorrent")]
    #[test]
    fn bittorrent_single_file_torrent_is_taken() {
        // A single-file torrent: take it regardless of name; the engine verifies.
        let one = vec![("whatever.gguf".to_string(), 555u64)];
        assert_eq!(pick_torrent_file(&one, "model.gguf", 999), Some(0));
    }
}
