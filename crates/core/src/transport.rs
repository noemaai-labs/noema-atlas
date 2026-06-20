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
        Source::Ipfs { .. } => "ipfs".to_string(),
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
    pub struct IpfsGatewayAdapter {
        pub http: HttpClient,
        pub gateways: Vec<String>, // e.g. ["https://ipfs.io", "https://cloudflare-ipfs.com"]
    }

    impl IpfsGatewayAdapter {
        fn url(gateway: &str, cid: &str) -> String {
            format!("{}/ipfs/{}", gateway.trim_end_matches('/'), cid)
        }
    }

    #[async_trait::async_trait]
    impl TransportAdapter for IpfsGatewayAdapter {
        fn class(&self) -> SourceClass {
            SourceClass::Ipfs
        }
        async fn probe(
            &self,
            source: &Source,
            _a: &Artifact,
            ctx: &FetchCtx,
        ) -> Result<SourceCaps> {
            let Source::Ipfs { cid, .. } = source else {
                return Err(adapter_mismatch());
            };
            let gw = self.gateways.first().ok_or_else(|| {
                Error::transport(
                    source.source_id(),
                    TransportErrorKind::Unsupported("no ipfs gateway configured".into()),
                )
            })?;
            self.http
                .probe_url(
                    &Self::url(gw, cid),
                    ctx.token.as_deref(),
                    &source.source_id(),
                )
                .await
        }
        async fn open(
            &self,
            source: &Source,
            _a: &Artifact,
            range: Option<ByteRange>,
            ctx: &FetchCtx,
        ) -> Result<Opened> {
            let Source::Ipfs { cid, .. } = source else {
                return Err(adapter_mismatch());
            };
            // Try each configured gateway in turn (client-side verification by
            // the engine makes an untrusted gateway safe — corruption is caught).
            let mut last = Error::transport(source.source_id(), TransportErrorKind::NotFound);
            for gw in &self.gateways {
                match self
                    .http
                    .open_url(
                        &Self::url(gw, cid),
                        ctx.token.as_deref(),
                        range,
                        &source.source_id(),
                    )
                    .await
                {
                    Ok(o) => return Ok(o),
                    Err(e) => last = e,
                }
            }
            Err(last)
        }
    }

    // LAN peering was removed (Atlas is a worldwide service). The `LanPeer`
    // source variant is retained only for back-compat deserialization; it is
    // never planned or fetched, so it has no adapter.
}

#[cfg(feature = "http")]
pub use net::{HttpClient, HttpMirrorAdapter, HuggingFaceAdapter, IpfsGatewayAdapter};
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
            _artifact: &Artifact,
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
                node.fetch_from_providers(blob_hash, tickets, &tmp, cancel, on_bytes)
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
/// Holds one adapter per supported source class and dispatches by class.
pub struct Transports {
    local: LocalFileAdapter,
    #[cfg(feature = "http")]
    mirror: net::HttpMirrorAdapter,
    #[cfg(feature = "http")]
    hf: net::HuggingFaceAdapter,
    #[cfg(feature = "http")]
    ipfs: net::IpfsGatewayAdapter,
    #[cfg(feature = "iroh")]
    iroh: iroh_adapter::IrohAdapter,
}

/// Construction options for the transport registry.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub request_timeout: Duration,
    /// Hugging Face Hub origin used for *downloads* (resolve URLs). Set this to an
    /// HF mirror (e.g. `https://hf-mirror.com`) to fetch weights through it.
    pub hf_endpoint: String,
    pub ipfs_gateways: Vec<String>,
    /// Directory for the Iroh blob store (used by the iroh fetch adapter).
    pub iroh_store_dir: std::path::PathBuf,
    /// Optional proxy ("VPN tunnel") for all HTTP-family transports. Accepts
    /// `http://`, `https://`, or `socks5://` / `socks5h://`. `None` (or empty)
    /// means a direct connection (reqwest still honors system proxy env vars).
    pub proxy: Option<String>,
}

impl Default for TransportConfig {
    fn default() -> Self {
        TransportConfig {
            request_timeout: Duration::from_secs(300),
            hf_endpoint: "https://huggingface.co".to_string(),
            ipfs_gateways: vec![
                "https://ipfs.io".to_string(),
                "https://cloudflare-ipfs.com".to_string(),
                "https://dweb.link".to_string(),
            ],
            iroh_store_dir: std::env::temp_dir().join("noema-iroh-store"),
            proxy: None,
        }
    }
}

impl Transports {
    pub fn new(cfg: &TransportConfig) -> Result<Self> {
        // Internet-facing transports (HF, mirror, IPFS) ride the optional proxy
        #[cfg(feature = "http")]
        let http = net::HttpClient::new(cfg.request_timeout, cfg.proxy.as_deref())?;
        Ok(Transports {
            local: LocalFileAdapter,
            #[cfg(feature = "http")]
            mirror: net::HttpMirrorAdapter { http: http.clone() },
            #[cfg(feature = "http")]
            hf: net::HuggingFaceAdapter {
                http: http.clone(),
                endpoint: cfg.hf_endpoint.clone(),
            },
            #[cfg(feature = "http")]
            ipfs: net::IpfsGatewayAdapter {
                http,
                gateways: cfg.ipfs_gateways.clone(),
            },
            #[cfg(feature = "iroh")]
            iroh: iroh_adapter::IrohAdapter::new(cfg.iroh_store_dir.clone()),
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
            #[cfg(feature = "http")]
            SourceClass::Ipfs => Ok(&self.ipfs),
            // LAN peering removed; the variant only exists for back-compat deser
            // and is never planned (see `PlatformProfile::fetch_enabled`).
            SourceClass::LanPeer => Err(Error::Unsupported(
                "LAN peering has been removed — Atlas is worldwide-only".into(),
            )),
            #[cfg(not(feature = "http"))]
            SourceClass::HttpsMirror | SourceClass::Huggingface | SourceClass::Ipfs => {
                Err(Error::Unsupported(
                    "HTTP transports disabled (build without `http` feature)".into(),
                ))
            }
            #[cfg(feature = "iroh")]
            SourceClass::Iroh => Ok(&self.iroh),
            #[cfg(not(feature = "iroh"))]
            SourceClass::Iroh => Err(Error::Unsupported(
                "iroh adapter not enabled (build with `--features iroh`)".into(),
            )),
            // BitTorrent has been retired; the variant only exists for back-compat
            // deser and is never planned (see `PlatformProfile::fetch_enabled`).
            SourceClass::BittorrentV2 => Err(Error::Unsupported(
                "BitTorrent support has been removed".into(),
            )),
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
}
