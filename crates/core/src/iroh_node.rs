#![allow(deprecated)]

use crate::error::{Error, Result, TransportErrorKind};
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use iroh::base::ticket::{BlobTicket, NodeTicket};
use iroh::net::endpoint::{Connecting, Connection, TransportConfig, VarInt};
use iroh::net::key::SecretKey;
use iroh::net::Endpoint;
use iroh::router::{ProtocolHandler, Router};
use iroh_blobs::downloader::Downloader;
use iroh_blobs::get::db::DownloadProgress;
use iroh_blobs::net_protocol::{BlobDownloadRequest, Blobs, DownloadMode};
use iroh_blobs::provider::{handle_connection, CustomEventSender, Event, EventSender};
use iroh_blobs::store::fs::Store as FsStore;
use iroh_blobs::store::{ExportMode, ImportMode, ReadableStore as _, Store as _};
use iroh_blobs::util::local_pool::{LocalPool, LocalPoolHandle};
use iroh_blobs::util::progress::AsyncChannelProgressSender;
use iroh_blobs::util::SetTagOption;
use iroh_blobs::{BlobFormat, Hash};

fn ierr<E: std::fmt::Display>(ctx: &str, e: E) -> Error {
    Error::other(format!("iroh {ctx}: {e}"))
}

/// Per-stream QUIC receive window sized for relayed desktop transfers.
const STREAM_RECEIVE_WINDOW: u32 = 16 * 1024 * 1024;

/// Byte-progress sink `(bytes_done, bytes_total)` for an in-flight fetch. The
/// whole blob downloads (and exports) before the engine's stream loop runs, so
/// without forwarding iroh's own progress the transfer would appear frozen.
type BytesProgress = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// How long an iroh fetch waits for the *first* bytes from any provider before
/// giving up; on timeout it fails over to the next source (e.g. Hugging Face).
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Once bytes are flowing, the maximum silence between progress events before the
/// transfer is treated as stalled (the peer dropped mid-transfer).
const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// Upper bound on resolving this node's reachable address, so a discovery stall
/// can't wedge worldwide-share startup (the UI thread `block_on`s it).
const NODE_ADDR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Upper bound on each step of a hard share-stop, so an unresponsive
/// router/endpoint shutdown can't freeze the UI thread that `block_on`s it.
const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How often the seeder sweeps for upload slots whose peer vanished without a
/// clean end-of-transfer event.
const UPLOAD_REAP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// An upload slot with no provider event for this long is treated as a vanished
/// peer and reclaimed. Kept well under the downloader's [`STALL_TIMEOUT`].
const UPLOAD_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Watchdog decision for an in-flight iroh fetch: abort if still connecting past
/// [`CONNECT_TIMEOUT`], or a connected transfer has gone silent past [`STALL_TIMEOUT`].
fn fetch_stalled(
    elapsed_since_start: std::time::Duration,
    received_bytes: bool,
    idle_since_last_progress: std::time::Duration,
) -> bool {
    if received_bytes {
        idle_since_last_progress >= STALL_TIMEOUT
    } else {
        elapsed_since_start >= CONNECT_TIMEOUT
    }
}

/// Stripe piece size. A whole multiple of the 16 KiB bao chunk group so a stripe
/// boundary never splits a group (which would make peers ship overlapping spine).
const STRIPE_PIECE_BYTES: u64 = 4 * 1024 * 1024;

/// Below this, with one peer, striping isn't worth its per-connection overhead.
const MULTIPEER_MIN_BYTES: u64 = 2 * STRIPE_PIECE_BYTES;

const MAX_STRIPE_PEERS: usize = 16;

/// Pieces one peer keeps in flight at once, each on its own QUIC stream over the
/// peer's one connection. Pipelining keeps the link busy across piece boundaries.
const PIPELINE_PER_PEER: usize = 3;

/// Consecutive failed pieces before a peer is dropped from the swarm (one bad
/// stream shouldn't evict a healthy peer — its piece just requeues elsewhere).
const PEER_FAIL_BUDGET: u64 = 3;

/// How often the striped driver re-asks the caller for providers, so peers that
/// come online mid-download join the swarm and dropped ones get a fresh chance.
const PEER_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Callback handing the striped fetch a fresh provider-ticket list mid-transfer
/// (typically a tracker `/providers` re-query). Returns raw node tickets; the
/// fetch parses, de-duplicates, and self-filters them.
pub type MoreNodes =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Vec<String>> + Send>> + Send + Sync>;

/// blake3 leaf chunk size; `ChunkNum`/`RangeSpec` count in these units.
const BLAKE3_CHUNK: u64 = 1024;

/// Scratch file a fetch writes a blob into before the engine verifies it. One
/// place derives the name so the adapter and engine agree. `blob_hash` is not
/// guaranteed ASCII, so take chars, never byte-index (panics mid-codepoint).
pub(crate) fn scratch_path(store_dir: &Path, blob_hash: &str) -> std::path::PathBuf {
    store_dir.join("scratch").join(format!(
        "iroh-{}.tmp",
        blob_hash.chars().take(16).collect::<String>()
    ))
}

/// Sidecar journal recording a striped fetch's completed pieces, so Pause/Stop or
/// a crash resumes where it left off instead of re-fetching from byte 0. Its
/// presence also tells the adapter a scratch leftover is a resumable partial.
pub(crate) fn stripe_journal_path(dest: &Path) -> std::path::PathBuf {
    let mut os = dest.as_os_str().to_os_string();
    os.push(".stripes");
    std::path::PathBuf::from(os)
}

/// Load the completed-piece set from a stripe journal. Empty (fresh start) unless
/// the header matches this exact blob + size and the scratch file still exists at
/// its full pre-sized length. A torn trailing line just drops that piece.
fn load_stripe_journal(
    journal: &Path,
    hash: &Hash,
    size: u64,
    dest: &Path,
) -> std::collections::HashSet<(u64, u64)> {
    let empty = std::collections::HashSet::new;
    match std::fs::metadata(dest) {
        Ok(m) if m.len() == size => {}
        _ => return empty(),
    }
    let Ok(text) = std::fs::read_to_string(journal) else {
        return empty();
    };
    let mut lines = text.lines();
    let expected = format!("v1 {size} {}", hex::encode(hash.as_bytes()));
    if lines.next() != Some(expected.as_str()) {
        return empty();
    }
    lines
        .filter_map(|l| {
            let (s, e) = l.split_once(' ')?;
            Some((s.parse().ok()?, e.parse().ok()?))
        })
        .collect()
}

/// Split a blob into contiguous `[start_chunk, end_chunk)` stripe pieces.
fn stripe_pieces(size: u64) -> std::collections::VecDeque<(u64, u64)> {
    let total_chunks = size.div_ceil(BLAKE3_CHUNK);
    let piece_chunks = STRIPE_PIECE_BYTES / BLAKE3_CHUNK;
    let mut pieces = std::collections::VecDeque::new();
    let mut c = 0u64;
    while c < total_chunks {
        let end = (c + piece_chunks).min(total_chunks);
        pieces.push_back((c, end));
        c = end;
    }
    pieces
}

/// Fetch one chunk range of `hash` over `conn`, bao-verified against the root and
/// written to `file` at the leaves' absolute offsets. A bad peer fails before any
/// byte is written.
async fn get_blob_range(
    conn: &Connection,
    hash: Hash,
    start_chunk: u64,
    end_chunk: u64,
    file: &mut iroh_io::File,
) -> Result<()> {
    use bao_tree::{ChunkNum, ChunkRanges};
    use iroh_blobs::get::fsm::{ConnectedNext, EndBlobNext};
    use iroh_blobs::protocol::{GetRequest, RangeSpecSeq};

    let ranges = ChunkRanges::from(ChunkNum(start_chunk)..ChunkNum(end_chunk));
    let request = GetRequest::new(hash, RangeSpecSeq::from_ranges([ranges]));
    let connected = iroh_blobs::get::fsm::start(conn.clone(), request)
        .next()
        .await
        .map_err(|e| ierr("stripe connect", e))?;
    let ConnectedNext::StartRoot(start_root) =
        connected.next().await.map_err(|e| ierr("stripe next", e))?
    else {
        return Err(ierr("stripe", "expected StartRoot"));
    };
    let at_end = start_root
        .next()
        .write_all(&mut *file)
        .await
        .map_err(|e| ierr("stripe decode/write", e))?;
    let EndBlobNext::Closing(closing) = at_end.next() else {
        return Err(ierr("stripe", "expected closing"));
    };
    closing.next().await.map_err(|e| ierr("stripe close", e))?;
    Ok(())
}

/// State shared by every lane of a striped fetch.
struct StripeShared {
    endpoint: Endpoint,
    hash: Hash,
    dest: std::path::PathBuf,
    size: u64,
    /// (start_chunk, end_chunk, attempts)
    queue: Mutex<std::collections::VecDeque<(u64, u64, u64)>>,
    pieces_left: AtomicU64,
    bytes_done: AtomicU64,
    last_emit: AtomicU64,
    /// Set when a piece has failed too many times: striping can't finish.
    fatal: AtomicBool,
    /// Attempt ceiling per piece; grows as peers join so a late-arriving healthy
    /// peer still gets a shot at a piece the early swarm kept dropping.
    max_attempts: AtomicU64,
    cancel: Option<Arc<AtomicBool>>,
    on_bytes: Option<BytesProgress>,
    /// NodeIds currently in the swarm, so a provider refresh doesn't re-dial a
    /// connected peer (a dropped peer leaves the set and may be re-added later).
    active: Mutex<std::collections::HashSet<iroh::net::NodeId>>,
    /// Resume journal (append-only): one `start end` line per completed piece.
    journal: Option<std::sync::Mutex<std::fs::File>>,
}

impl StripeShared {
    fn stopped(&self) -> bool {
        self.cancel
            .as_ref()
            .map(|f| f.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    fn complete_piece(&self, start: u64, end: u64) {
        let nbytes = (end * BLAKE3_CHUNK).min(self.size) - (start * BLAKE3_CHUNK).min(self.size);
        let done = self.bytes_done.fetch_add(nbytes, Ordering::SeqCst) + nbytes;
        self.pieces_left.fetch_sub(1, Ordering::SeqCst);
        if let Some(j) = self.journal.as_ref() {
            use std::io::Write;
            let _ = writeln!(j.lock().unwrap(), "{start} {end}");
        }
        if let Some(sink) = self.on_bytes.as_deref() {
            let prev = self.last_emit.load(Ordering::SeqCst);
            if done.saturating_sub(prev) >= (1 << 20) || done >= self.size {
                self.last_emit.store(done, Ordering::SeqCst);
                sink(done.min(self.size), self.size);
            }
        }
    }

    fn requeue_piece(&self, start: u64, end: u64, attempts: u64) {
        if attempts >= self.max_attempts.load(Ordering::SeqCst) {
            self.fatal.store(true, Ordering::SeqCst);
        } else {
            self.queue.lock().unwrap().push_back((start, end, attempts));
        }
    }
}

/// Add one peer to a striped fetch: dial it once, then run [`PIPELINE_PER_PEER`]
/// lanes over that connection, each pulling pieces from the shared queue on its own
/// QUIC stream. A lane failure requeues its piece; [`PEER_FAIL_BUDGET`] consecutive
/// failures retire the whole peer (transient errors don't).
fn spawn_stripe_peer(
    workers: &mut tokio::task::JoinSet<()>,
    shared: Arc<StripeShared>,
    node: iroh::net::NodeAddr,
) {
    let id = node.node_id;
    if !shared.active.lock().unwrap().insert(id) {
        return;
    }
    workers.spawn(async move {
        // Remove this peer from the swarm set on any exit (including abort), so a
        // later provider refresh may re-admit it with a fresh connection.
        struct ActiveGuard(Arc<StripeShared>, iroh::net::NodeId);
        impl Drop for ActiveGuard {
            fn drop(&mut self) {
                self.0.active.lock().unwrap().remove(&self.1);
            }
        }
        let _guard = ActiveGuard(shared.clone(), id);
        let conn = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            shared.endpoint.connect(node, iroh_blobs::protocol::ALPN),
        )
        .await
        {
            Ok(Ok(c)) => c,
            _ => return,
        };
        let fails = Arc::new(AtomicU64::new(0));
        let mut lanes: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        for _ in 0..PIPELINE_PER_PEER {
            let shared = shared.clone();
            let conn = conn.clone();
            let fails = fails.clone();
            lanes.spawn(async move {
                // Per-lane handle: positioned writes to disjoint offsets are safe
                // across independent handles to the same file.
                let mut file = match std::fs::OpenOptions::new().write(true).open(&shared.dest) {
                    Ok(f) => iroh_io::File::from_std(f),
                    Err(_) => return,
                };
                let mut idle_polls = 0u32;
                loop {
                    if shared.fatal.load(Ordering::SeqCst)
                        || shared.stopped()
                        || fails.load(Ordering::SeqCst) >= PEER_FAIL_BUDGET
                    {
                        return;
                    }
                    let next = { shared.queue.lock().unwrap().pop_front() };
                    let Some((start, end, attempts)) = next else {
                        if shared.pieces_left.load(Ordering::SeqCst) == 0 {
                            return;
                        }
                        // Empty for now but not done: another lane may re-queue a
                        // failed piece. Back off and re-check, bounded so a wedged
                        // swarm can't spin forever.
                        idle_polls += 1;
                        if idle_polls > 200 {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    };
                    idle_polls = 0;
                    let outcome = tokio::time::timeout(
                        STALL_TIMEOUT,
                        get_blob_range(&conn, shared.hash, start, end, &mut file),
                    )
                    .await;
                    match outcome {
                        Ok(Ok(())) => {
                            fails.store(0, Ordering::SeqCst);
                            shared.complete_piece(start, end);
                        }
                        _ => {
                            fails.fetch_add(1, Ordering::SeqCst);
                            shared.requeue_piece(start, end, attempts + 1);
                        }
                    }
                }
            });
        }
        while lanes.join_next().await.is_some() {}
    });
}

/// Live provider-side counters for the worldwide Iroh seeder. Small and cloneable
/// so the desktop UI can sample them the same way it samples the LAN HTTP server.
#[derive(Debug, Clone, Default)]
pub struct IrohMetrics {
    uploaded_bytes: Arc<AtomicU64>,
    active_uploads: Arc<AtomicU64>,
    /// Active upload count keyed by blob hash for per-model share-off warnings.
    active_by_hash: Arc<Mutex<HashMap<Hash, u64>>>,
}

impl IrohMetrics {
    pub fn uploaded(&self) -> u64 {
        self.uploaded_bytes.load(Ordering::Relaxed)
    }

    pub fn active_uploads(&self) -> u64 {
        self.active_uploads.load(Ordering::Relaxed)
    }

    /// How many peers are pulling the blob with this blake3 (hex) right now.
    /// Returns 0 for an unparseable hash or one nobody is fetching.
    pub fn active_uploads_for_hex(&self, blake3_hex: &str) -> u64 {
        let Ok(bytes) = crate::hash::parse_hex32(blake3_hex) else {
            return 0;
        };
        let hash = Hash::from(bytes);
        self.active_by_hash
            .lock()
            .map(|m| m.get(&hash).copied().unwrap_or(0))
            .unwrap_or(0)
    }

    fn add_uploaded(&self, bytes: u64) {
        if bytes > 0 {
            self.uploaded_bytes.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    fn start_upload(&self, hash: Hash) {
        self.active_uploads.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut m) = self.active_by_hash.lock() {
            *m.entry(hash).or_insert(0) += 1;
        }
    }

    fn finish_upload(&self, hash: Hash) {
        let _ = self
            .active_uploads
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
        if let Ok(mut m) = self.active_by_hash.lock() {
            if let Some(c) = m.get_mut(&hash) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    m.remove(&hash);
                }
            }
        }
    }
}

/// Per-request upload accounting for `ProviderEventSink`.
#[derive(Debug, Clone)]
struct Upload {
    /// The blob this request is serving. Stashed so the completed/aborted event
    /// (which carries only the connection/request ids) can decrement the right
    /// per-hash counter and find which connections to sever on this blob's share-off.
    hash: Hash,
    /// Bytes counted as uploaded for this request so far. Relative to *this*
    /// request: only the ranges we actually send, never a prefix a resuming peer
    /// already holds.
    sent: u64,
    /// The last absolute blob offset a progress event reported, or `None` until the
    /// first event sets the baseline. A resumed transfer's first event lands where
    /// the peer left off, so we baseline to it rather than counting it.
    last_offset: Option<u64>,
    /// When we last saw any provider event for this request. The reaper reclaims a
    /// slot idle past [`UPLOAD_STALL_TIMEOUT`], so a peer that quit mid-pull without
    /// a final event can't pin the "pulling now" count up (see `reap_stalled`).
    last_event_at: std::time::Instant,
}

impl Upload {
    fn new(hash: Hash) -> Self {
        Self {
            hash,
            sent: 0,
            last_offset: None,
            last_event_at: std::time::Instant::now(),
        }
    }
}

#[derive(Debug, Clone)]
struct ProviderEventSink {
    metrics: IrohMetrics,
    // (connection_id, request_id) -> per-request upload accounting.
    progress: Arc<Mutex<std::collections::HashMap<(u64, u64), Upload>>>,
}

impl ProviderEventSink {
    fn new(metrics: IrohMetrics) -> Self {
        Self {
            metrics,
            progress: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    fn note_started(&self, key: (u64, u64), hash: Hash) {
        if let Ok(mut progress) = self.progress.lock() {
            if let std::collections::hash_map::Entry::Vacant(slot) = progress.entry(key) {
                slot.insert(Upload::new(hash));
                self.metrics.start_upload(hash);
            }
        }
    }

    fn note_progress(&self, key: (u64, u64), hash: Hash, end_offset: u64) {
        if let Ok(mut progress) = self.progress.lock() {
            let up = progress.entry(key).or_insert_with(|| {
                self.metrics.start_upload(hash);
                Upload::new(hash)
            });
            up.last_event_at = std::time::Instant::now();
            match up.last_offset {
                // First progress event: `end_offset` is the *absolute* blob offset,
                // so a resumed transfer jumps straight to where it left off. Baseline
                // to it rather than counting it (the phantom "instant upload" spike).
                None => up.last_offset = Some(end_offset),
                Some(prev) => {
                    if end_offset > prev {
                        let delta = end_offset - prev;
                        self.metrics.add_uploaded(delta);
                        up.sent += delta;
                        up.last_offset = Some(end_offset);
                    }
                }
            }
        }
    }

    fn note_finished(&self, key: (u64, u64), final_bytes: Option<u64>) {
        let up = self
            .progress
            .lock()
            .ok()
            .and_then(|mut progress| progress.remove(&key));
        if let Some(up) = up {
            if let Some(final_bytes) = final_bytes {
                // `final_bytes` is the actual wire total sent (net of any resumed
                // prefix). Reconcile the remainder forward-delta counting missed —
                // notably the baseline first chunk.
                self.metrics
                    .add_uploaded(final_bytes.saturating_sub(up.sent));
            }
            self.metrics.finish_upload(up.hash);
        }
    }

    /// Reclaim upload slots whose peer stopped pulling without a final event (iroh
    /// pools the connection, so no `TransferAborted` arrives and the slot would
    /// otherwise pin the "pulling now" counts up). Returns the number reclaimed.
    fn reap_stalled(&self, max_idle: std::time::Duration) -> usize {
        let now = std::time::Instant::now();
        let stale: Vec<(u64, u64)> = {
            let Ok(progress) = self.progress.lock() else {
                return 0;
            };
            progress
                .iter()
                .filter(|(_, up)| now.duration_since(up.last_event_at) >= max_idle)
                .map(|(k, _)| *k)
                .collect()
        };
        for key in &stale {
            // No final byte count: the peer left mid-stream, so we want only the
            // decrement, not a reconciliation of the wire total.
            self.note_finished(*key, None);
        }
        stale.len()
    }

    /// Reconcile when a serving connection's loop ends: decrement any requests still
    /// counted on it, so a closed connection clears at once rather than waiting for
    /// the idle sweep.
    fn connection_closed(&self, conn_id: u64) {
        let keys: Vec<(u64, u64)> = {
            let Ok(progress) = self.progress.lock() else {
                return;
            };
            progress
                .keys()
                .filter(|(c, _)| *c == conn_id)
                .copied()
                .collect()
        };
        for key in keys {
            self.note_finished(key, None);
        }
    }

    /// The QUIC connection ids currently transferring `hash` to a peer. Used to
    /// hard-disconnect exactly the peers pulling one blob when its share is turned
    /// off, without tearing down connections serving other (still-shared) files.
    fn connection_ids_for_hash(&self, hash: Hash) -> Vec<u64> {
        let Ok(progress) = self.progress.lock() else {
            return Vec::new();
        };
        let mut ids: Vec<u64> = progress
            .iter()
            .filter(|(_, up)| up.hash == hash)
            .map(|((conn_id, _), _)| *conn_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    fn handle_event(&self, event: Event) {
        match event {
            Event::GetRequestReceived {
                connection_id,
                request_id,
                hash,
            } => self.note_started((connection_id, request_id), hash),
            Event::TransferProgress {
                connection_id,
                request_id,
                hash,
                end_offset,
            } => self.note_progress((connection_id, request_id), hash, end_offset),
            Event::TransferCompleted {
                connection_id,
                request_id,
                stats,
            } => self.note_finished((connection_id, request_id), Some(stats.send.total().size)),
            Event::TransferAborted {
                connection_id,
                request_id,
                stats,
            } => self.note_finished(
                (connection_id, request_id),
                stats.map(|s| s.send.total().size),
            ),
            _ => {}
        }
    }
}

impl CustomEventSender for ProviderEventSink {
    fn send(
        &self,
        event: Event,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
        self.handle_event(event);
        Box::pin(async {})
    }

    fn try_send(&self, event: Event) {
        self.handle_event(event);
    }
}

/// Live QUIC connections we're serving blobs over, keyed by their stable id (the
/// `connection_id` provider events carry). Lets us `close()` exactly the
/// connections pulling a given blob when its share is turned off.
#[derive(Clone, Default)]
struct ConnRegistry {
    conns: Arc<Mutex<HashMap<u64, Connection>>>,
}

impl ConnRegistry {
    fn insert(&self, id: u64, conn: Connection) {
        if let Ok(mut m) = self.conns.lock() {
            m.insert(id, conn);
        }
    }

    fn remove(&self, id: u64) {
        if let Ok(mut m) = self.conns.lock() {
            m.remove(&id);
        }
    }

    /// Hard-close the listed connections (those mid-transfer of a blob whose share
    /// just stopped), severing every stream at once instead of letting the pull time
    /// out. iroh re-accepts a fresh connection if that peer later wants something else.
    fn close(&self, ids: &[u64]) {
        if let Ok(m) = self.conns.lock() {
            for id in ids {
                if let Some(conn) = m.get(id) {
                    conn.close(0u32.into(), b"share stopped");
                }
            }
        }
    }
}

/// Our own blobs accept-handler, a thin wrapper over [`handle_connection`] that also
/// records each live connection in a [`ConnRegistry`] — which is what lets a
/// per-model share-off forcibly disconnect the peers pulling that model.
#[derive(Clone)]
struct BlobServer {
    store: FsStore,
    events: EventSender,
    /// The same sink the events feed, kept so the accept-handler can reconcile
    /// upload counters when a connection's serving loop ends (a peer can drop
    /// without a final transfer event).
    sink: ProviderEventSink,
    rt: LocalPoolHandle,
    registry: ConnRegistry,
}

impl std::fmt::Debug for BlobServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobServer").finish_non_exhaustive()
    }
}

impl ProtocolHandler for BlobServer {
    fn accept(
        self: Arc<Self>,
        conn: Connecting,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        Box::pin(async move {
            let connection = conn.await?;
            let id = connection.stable_id() as u64;
            self.registry.insert(id, connection.clone());
            handle_connection(
                connection,
                self.store.clone(),
                self.events.clone(),
                self.rt.clone(),
            )
            .await;
            // The connection's serving loop has ended: reclaim any upload slots
            // still counted on it (a peer can drop without a final transfer
            // event), then drop our handle to it.
            self.sink.connection_closed(id);
            self.registry.remove(id);
            Ok(())
        })
    }

    fn shutdown(self: Arc<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            self.store.shutdown().await;
        })
    }
}

/// Load this node's persistent Ed25519 secret key from `store_dir/node.key`,
/// generating and persisting a fresh one on first run. A stable key yields a stable
/// NodeId across restarts, so the tracker sees one device instead of one per launch.
fn load_or_create_secret_key(store_dir: &Path) -> Result<SecretKey> {
    let path = store_dir.join("node.key");
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) {
            return Ok(SecretKey::from_bytes(&arr));
        }
        // A corrupt/legacy key file: fall through and mint a fresh identity.
    }
    let key = SecretKey::generate();
    let bytes = key.to_bytes();
    // The key is a credential. On unix, create it atomically owner-only (mode 0600
    // via OpenOptions) so there's no world-readable window between write and chmod.
    // Windows lacks unix mode bits; the file inherits the parent dir's ACL (the
    // per-user app data dir), which is the platform's owner-private default.
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| Error::fs(&path, e))?;
        f.write_all(&bytes).map_err(|e| Error::fs(&path, e))?;
        // `.mode(0o600)` only applies when the file is created; if we just overwrote
        // a pre-existing corrupt/legacy key with looser permissions, re-tighten it.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| Error::fs(&path, e))?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, bytes).map_err(|e| Error::fs(&path, e))?;
    Ok(key)
}

/// This device's stable NodeId (hex) derived from the persisted node key, without
/// spawning an endpoint. `None` if no key exists yet (worldwide sharing has never
/// run). Identical to a live [`IrohNode::node_id`], so the engine can exclude and
/// withdraw its own prior-session announces even when the seeder isn't running.
pub fn node_id_from_store(store_dir: &Path) -> Option<String> {
    let path = store_dir.join("node.key");
    let bytes = std::fs::read(&path).ok()?;
    let arr = <[u8; 32]>::try_from(bytes.as_slice()).ok()?;
    let key = SecretKey::from_bytes(&arr);
    Some(key.public().to_string())
}

/// Sign a tracker announce/withdraw payload with this device's node SECRET key,
/// proving ownership of the claimed NodeId. Returns `(node_id, ts_ms, sig_b64)`; the
/// registry rebuilds the same canonical payload and verifies it (see
/// [`crate::announce_auth`]). The payload also binds `ticket` and `audience` so a
/// MITM can't rewrite the address and a captured request can't replay against a
/// different registry. `None` when no key exists yet or an item id isn't 64-hex.
pub fn sign_announce(
    store_dir: &Path,
    method: &str,
    ticket: &str,
    audience: &str,
    item_ids: &[String],
) -> Option<(String, i64, String)> {
    use base64::Engine as _;
    let path = store_dir.join("node.key");
    let bytes = std::fs::read(&path).ok()?;
    let arr = <[u8; 32]>::try_from(bytes.as_slice()).ok()?;
    let key = SecretKey::from_bytes(&arr);
    let node_id = key.public().to_string();
    let ts = crate::util::now_unix_millis();
    let payload =
        crate::announce_auth::canonical_payload(method, &node_id, ts, ticket, audience, item_ids)?;
    let sig = key.sign(&payload);
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
    Some((node_id, ts, sig_b64))
}

/// A running iroh node: a QUIC endpoint, a disk-backed blob store, and a router
/// serving the blobs protocol so peers worldwide can fetch from us.
pub struct IrohNode {
    endpoint: Endpoint,
    blobs: Arc<Blobs<FsStore>>,
    store: FsStore,
    router: Router,
    pool: LocalPool,
    metrics: IrohMetrics,
    /// The provider event sink, kept so we can ask which connections are pulling a
    /// given blob right now (to sever exactly them on a per-file share-off).
    sink: ProviderEventSink,
    /// Live serving connections, so a per-file share-off can hard-close them.
    registry: ConnRegistry,
    // Keep import temp tags alive so seeded blobs aren't garbage-collected.
    tags: std::sync::Mutex<Vec<iroh_blobs::TempTag>>,
    /// Background sweep that reclaims upload slots whose peer vanished without a
    /// clean end-of-transfer event. Aborted on drop (see the `Drop` impl).
    reaper: tokio::task::JoinHandle<()>,
}

impl Drop for IrohNode {
    /// Stop the upload-reaper sweep when the node goes away (worldwide share off,
    /// or app quit). The task holds a clone of the metrics/progress state, so
    /// without this it would outlive the node it exists to reconcile.
    fn drop(&mut self) {
        self.reaper.abort();
    }
}

impl IrohNode {
    /// Spawn a node with its blob store under `store_dir` (created if needed).
    pub async fn spawn(store_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(store_dir).map_err(|e| Error::fs(store_dir, e))?;
        let pool = LocalPool::single();
        // Persist the node identity so this device keeps a *stable* NodeId across
        // restarts; otherwise every launch mints a fresh NodeId and shows up as a
        // new peer while the previous announcement is still live.
        let secret_key = load_or_create_secret_key(store_dir)?;
        // Lift QUIC flow control above quinn's conservative 1.25 MB per-stream
        // default so a high-latency / relayed path isn't pinned to ~10 MB/s by the
        // window alone. Applied endpoint-wide, so fetch and serve both benefit.
        let mut transport = TransportConfig::default();
        transport
            .stream_receive_window(VarInt::from_u32(STREAM_RECEIVE_WINDOW)) // how fast a peer may send to us
            .send_window(4 * STREAM_RECEIVE_WINDOW as u64) // serve side: cover several concurrent streams
            // Connection-level window must cover the per-stream windows of every
            // pipelined lane (PIPELINE_PER_PEER streams per peer), or it becomes
            // the new ~10 MB/s-style ceiling the stream window fix lifted.
            .receive_window(VarInt::from_u32(4 * STREAM_RECEIVE_WINDOW));
        let endpoint = Endpoint::builder()
            .secret_key(secret_key)
            .transport_config(transport)
            // n0 DNS + relays reach worldwide peers; local-network (mDNS) discovery
            // lets two devices on the same LAN dial directly without a relay
            // round-trip. Both run together; a failed mDNS setup is silently skipped.
            .discovery_n0()
            .discovery_local_network()
            .bind()
            .await
            .map_err(|e| ierr("endpoint bind", e))?;
        // Retry the store load briefly: on a quick stop→start a previous node's redb
        // lock may take a moment to release as its store Arc drops, so the immediate
        // reopen can transiently conflict.
        let store = {
            let mut attempt = FsStore::load(store_dir).await;
            for _ in 0..5 {
                if attempt.is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                attempt = FsStore::load(store_dir).await;
            }
            attempt.map_err(|e| ierr("store load", e))?
        };
        let downloader = Downloader::new(store.clone(), endpoint.clone(), pool.handle().clone());
        let metrics = IrohMetrics::default();
        // One sink backs both halves: `Blobs` keeps it for events its own paths emit
        // and our `BlobServer` feeds it the serving events, so the per-blob counters
        // and progress map stay unified.
        let sink = ProviderEventSink::new(metrics.clone());
        let registry = ConnRegistry::default();
        let blobs = Arc::new(Blobs::new_with_events(
            store.clone(),
            pool.handle().clone(),
            EventSender::from(sink.clone()),
            downloader,
        ));
        // Serve via our own handler (not `blobs`) so we hold a handle to each live
        // connection and can sever exactly the peers pulling a blob when its share
        // is turned off. `blobs` is still used for the client/download side.
        let server = Arc::new(BlobServer {
            store: store.clone(),
            events: EventSender::from(sink.clone()),
            sink: sink.clone(),
            rt: pool.handle().clone(),
            registry: registry.clone(),
        });
        let router = Router::builder(endpoint.clone())
            .accept(iroh_blobs::protocol::ALPN.to_vec(), server)
            .spawn()
            .await
            .map_err(|e| ierr("router spawn", e))?;
        // Self-healing sweep: reclaim upload slots whose peer vanished without a
        // clean end-of-transfer event, so the "peers pulling" counts can't get
        // pinned up by a downloader that quit mid-pull (see `reap_stalled`).
        let reaper = {
            let sink = sink.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(UPLOAD_REAP_INTERVAL);
                loop {
                    tick.tick().await;
                    sink.reap_stalled(UPLOAD_STALL_TIMEOUT);
                }
            })
        };
        Ok(IrohNode {
            endpoint,
            blobs,
            store,
            router,
            pool,
            metrics,
            sink,
            registry,
            tags: std::sync::Mutex::new(Vec::new()),
            reaper,
        })
    }

    pub fn metrics(&self) -> IrohMetrics {
        self.metrics.clone()
    }

    /// This node's stable NodeId (hex). Unlike the full node *ticket* (whose
    /// relay/direct addresses shift as the network moves), this is the durable device
    /// identity — the correct key for de-duplicating peers on the tracker.
    pub fn node_id(&self) -> String {
        self.endpoint.node_id().to_string()
    }

    /// This node's current address (relay + direct candidates), bounded by
    /// [`NODE_ADDR_TIMEOUT`] so a transient discovery stall can't wedge a caller —
    /// notably worldwide-share startup, which the UI thread `block_on`s.
    async fn node_addr_bounded(&self) -> Result<iroh::net::NodeAddr> {
        tokio::time::timeout(NODE_ADDR_TIMEOUT, self.endpoint.node_addr())
            .await
            .map_err(|_| ierr("node_addr", "timed out resolving a reachable address"))?
            .map_err(|e| ierr("node_addr", e))
    }

    /// This node's ticket (node id + relay/direct addrs) for announcing — lets
    /// peers reach us from anywhere. Share this with the tracker.
    pub async fn node_ticket(&self) -> Result<String> {
        let addr = self.node_addr_bounded().await?;
        Ok(NodeTicket::new(addr).to_string())
    }

    /// Seed a file by *reference* (no copy): returns its blake3 hex. Keep the
    /// node alive to keep serving it.
    pub async fn seed_file(&self, path: &Path) -> Result<String> {
        let (tx, _keep) = async_channel::unbounded();
        let progress = AsyncChannelProgressSender::new(tx);
        let (tag, _size) = self
            .store
            .import_file(
                path.to_path_buf(),
                ImportMode::TryReference,
                BlobFormat::Raw,
                progress,
            )
            .await
            .map_err(|e| ierr("import", e))?;
        drop(_keep);
        let hash = *tag.hash();
        self.tags.lock().unwrap().push(tag);
        Ok(hex::encode(hash.as_bytes()))
    }

    /// Stop serving a previously-seeded blob: drop the temp tag and delete it from
    /// the store so peers can no longer pull it over Iroh. Seeded **by reference**,
    /// so the underlying CAS file is untouched. Withdrawing from the tracker only
    /// hides it from discovery; a peer that knows the hash could still fetch until now.
    pub async fn unseed(&self, blake3_hex: &str) -> Result<()> {
        let hash = Hash::from(crate::hash::parse_hex32(blake3_hex)?);
        if let Ok(mut tags) = self.tags.lock() {
            tags.retain(|t| *t.hash() != hash);
        }
        self.store
            .delete(vec![hash])
            .await
            .map_err(|e| ierr("unseed", e))?;
        Ok(())
    }

    /// Stop serving a blob **and** hard-disconnect any peer currently pulling it.
    /// [`unseed`](Self::unseed) alone only stops *new* reads; this also closes the
    /// QUIC connections actively transferring this blob, scoped to the one blob so
    /// pulls of other still-shared files are left alone.
    pub async fn unseed_and_disconnect(&self, blake3_hex: &str) -> Result<()> {
        let hash = Hash::from(crate::hash::parse_hex32(blake3_hex)?);
        // Snapshot which connections are pulling this blob *before* unseeding —
        // deleting it aborts those transfers and clears their progress entries.
        let conn_ids = self.sink.connection_ids_for_hash(hash);
        self.unseed(blake3_hex).await?;
        self.registry.close(&conn_ids);
        Ok(())
    }

    /// Seed a file and return `(blob_ticket, blake3_hex)` for direct sharing.
    pub async fn provide(&self, path: &Path) -> Result<(String, String)> {
        let blake3 = self.seed_file(path).await?;
        let hash = Hash::from(crate::hash::parse_hex32(&blake3)?);
        let addr = self.node_addr_bounded().await?;
        let ticket = BlobTicket::new(addr, hash, BlobFormat::Raw).map_err(|e| ierr("ticket", e))?;
        Ok((ticket.to_string(), blake3))
    }

    /// Fetch a blob named by a full `BlobTicket` string, exporting it to `dest`.
    /// `cancel`, when set, lets a user Stop abort the transfer promptly.
    pub async fn fetch_to_file(
        &self,
        ticket_str: &str,
        dest: &Path,
        cancel: Option<Arc<AtomicBool>>,
        on_bytes: Option<BytesProgress>,
    ) -> Result<()> {
        let ticket: BlobTicket = ticket_str
            .trim()
            .parse()
            .map_err(|e| ierr("ticket parse", e))?;
        let hash = ticket.hash();
        let format = ticket.format();
        let node = ticket.node_addr().clone();
        self.fetch_hash(hash, format, vec![node], dest, cancel, on_bytes)
            .await
    }

    /// Fetch a blob by its blake3 hash from the given provider node tickets (as
    /// returned by the tracker), exporting it to `dest`. With several peers and a
    /// large file this stripes the blob across all of them ([`fetch_striped`]);
    /// otherwise, or on failure, it falls back to a single-peer download.
    #[allow(clippy::too_many_arguments)]
    pub async fn fetch_from_providers(
        &self,
        blake3_hex: &str,
        node_tickets: &[String],
        dest: &Path,
        size: u64,
        cancel: Option<Arc<AtomicBool>>,
        on_bytes: Option<BytesProgress>,
        more_nodes: Option<MoreNodes>,
    ) -> Result<()> {
        let hash = Hash::from(crate::hash::parse_hex32(blake3_hex)?);
        let nodes =
            self.parse_provider_tickets(node_tickets, &mut std::collections::HashSet::new());
        if nodes.is_empty() {
            return Err(Error::other(
                "no reachable peers are online for this file right now",
            ));
        }
        if size >= MULTIPEER_MIN_BYTES {
            match self
                .fetch_striped(
                    hash,
                    nodes.clone(),
                    dest,
                    size,
                    cancel.clone(),
                    on_bytes.clone(),
                    more_nodes,
                )
                .await
            {
                Ok(()) => return Ok(()),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "striped multi-peer fetch failed; falling back to single-peer download"
                    );
                }
            }
        }
        self.fetch_hash(hash, BlobFormat::Raw, nodes, dest, cancel, on_bytes)
            .await
    }

    /// Parse tracker tickets into dialable addresses, de-duplicating by stable NodeId
    /// and dropping this node itself. An older deployed registry ignores the
    /// exclude-querier request, and a stale self-entry would make the fetch dial its
    /// own store and look "stuck resolving".
    fn parse_provider_tickets(
        &self,
        node_tickets: &[String],
        seen: &mut std::collections::HashSet<iroh::net::NodeId>,
    ) -> Vec<iroh::net::NodeAddr> {
        let me = self.endpoint.node_id();
        let mut nodes = Vec::new();
        for nt in node_tickets {
            if let Ok(node) = nt.trim().parse::<NodeTicket>() {
                let addr = node.node_addr().clone();
                if addr.node_id != me && seen.insert(addr.node_id) {
                    nodes.push(addr);
                }
            }
        }
        nodes
    }

    /// Pull one blob from many peers at once: split into pieces in a shared queue,
    /// each peer running [`PIPELINE_PER_PEER`] work-stealing lanes that write
    /// bao-verified ranges straight to `dest` at their absolute offset. `more_nodes`,
    /// when given, is polled every [`PEER_REFRESH_INTERVAL`] so providers appearing
    /// mid-transfer join the swarm.
    #[allow(clippy::too_many_arguments)]
    pub async fn fetch_striped(
        &self,
        hash: Hash,
        nodes: Vec<iroh::net::NodeAddr>,
        dest: &Path,
        size: u64,
        cancel: Option<Arc<AtomicBool>>,
        on_bytes: Option<BytesProgress>,
        more_nodes: Option<MoreNodes>,
    ) -> Result<()> {
        // Resume: the journal names the pieces already bao-verified into `dest` by a
        // previous attempt of this exact blob+size — only the rest is fetched.
        // Anything stale falls back to a fresh start.
        let journal_path = stripe_journal_path(dest);
        let all = stripe_pieces(size);
        let mut done = load_stripe_journal(&journal_path, &hash, size, dest);
        if !done.is_empty() {
            let grid: std::collections::HashSet<(u64, u64)> = all.iter().copied().collect();
            if !done.iter().all(|p| grid.contains(p)) {
                done.clear();
            }
        }
        if done.is_empty() {
            // Pre-size so lanes can write their stripes at absolute offsets.
            let f = std::fs::File::create(dest).map_err(|e| Error::fs(dest, e))?;
            f.set_len(size).map_err(|e| Error::fs(dest, e))?;
            let _ = std::fs::write(
                &journal_path,
                format!("v1 {size} {}\n", hex::encode(hash.as_bytes())),
            );
        }
        let resumed: u64 = done
            .iter()
            .map(|&(s, e)| (e * BLAKE3_CHUNK).min(size) - (s * BLAKE3_CHUNK).min(size))
            .sum();
        let pieces: std::collections::VecDeque<(u64, u64, u64)> = all
            .into_iter()
            .filter(|p| !done.contains(p))
            .map(|(s, e)| (s, e, 0))
            .collect();
        if pieces.is_empty() {
            let _ = std::fs::remove_file(&journal_path);
            if let Some(sink) = on_bytes.as_deref() {
                sink(size, size);
            }
            return Ok(());
        }
        if resumed > 0 {
            tracing::info!(resumed, size, "striped fetch: resuming from piece journal");
            if let Some(sink) = on_bytes.as_deref() {
                sink(resumed, size);
            }
        }
        let journal = std::fs::OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .ok()
            .map(std::sync::Mutex::new);

        let shared = Arc::new(StripeShared {
            endpoint: self.endpoint.clone(),
            hash,
            dest: dest.to_path_buf(),
            size,
            pieces_left: AtomicU64::new(pieces.len() as u64),
            queue: Mutex::new(pieces),
            bytes_done: AtomicU64::new(resumed),
            last_emit: AtomicU64::new(resumed),
            fatal: AtomicBool::new(false),
            max_attempts: AtomicU64::new((nodes.len() as u64 + 2).max(6)),
            cancel,
            on_bytes,
            active: Mutex::new(std::collections::HashSet::new()),
            journal,
        });

        let mut workers: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        for node in nodes {
            spawn_stripe_peer(&mut workers, shared.clone(), node);
        }

        // Drive the workers: cancel + aggregate connect/stall watchdog every tick,
        // and a periodic provider re-query feeding new peers into the swarm.
        let (fresh_tx, mut fresh_rx) = tokio::sync::mpsc::channel::<Vec<String>>(2);
        let mut last_refresh = std::time::Instant::now();
        let mut refresh_inflight = false;
        let started = std::time::Instant::now();
        // The connect window restarts (bounded) when new peers join a still-silent
        // swarm, so a refresh that surfaces a live peer isn't killed by a deadline
        // the dead starters already burned.
        let mut connect_anchor = started;
        let mut connect_extensions = 0u32;
        // Resumed bytes don't count as live progress: the connect window / stall
        // watchdog and the refresh extension must judge THIS attempt's peers.
        let baseline = resumed;
        let mut last_bytes = baseline;
        let mut last_advance = started;
        loop {
            tokio::select! {
                done = workers.join_next() => {
                    if done.is_none() {
                        break;
                    }
                }
                Some(tickets) = fresh_rx.recv() => {
                    refresh_inflight = false;
                    let mut seen: std::collections::HashSet<iroh::net::NodeId> =
                        shared.active.lock().unwrap().clone();
                    let fresh = self.parse_provider_tickets(&tickets, &mut seen);
                    if !fresh.is_empty() {
                        let active = shared.active.lock().unwrap().len() as u64;
                        shared
                            .max_attempts
                            .fetch_max(active + fresh.len() as u64 + 2, Ordering::SeqCst);
                        if last_bytes == baseline && connect_extensions < 2 {
                            connect_anchor = std::time::Instant::now();
                            connect_extensions += 1;
                        }
                        for node in fresh {
                            if shared.active.lock().unwrap().len() >= MAX_STRIPE_PEERS {
                                break;
                            }
                            tracing::info!(node = %node.node_id, "provider joined mid-transfer");
                            spawn_stripe_peer(&mut workers, shared.clone(), node);
                        }
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                    if shared.stopped() {
                        workers.abort_all();
                        return Err(Error::Cancelled);
                    }
                    let now = shared.bytes_done.load(Ordering::SeqCst);
                    if now > last_bytes {
                        last_bytes = now;
                        last_advance = std::time::Instant::now();
                    }
                    if fetch_stalled(connect_anchor.elapsed(), last_bytes > baseline, last_advance.elapsed()) {
                        tracing::warn!("striped fetch watchdog: no aggregate progress, aborting");
                        shared.fatal.store(true, Ordering::SeqCst);
                        workers.abort_all();
                        return Err(Error::transport("iroh", TransportErrorKind::NotFound));
                    }
                    if let Some(refresh) = more_nodes.as_ref() {
                        if !refresh_inflight
                            && last_refresh.elapsed() >= PEER_REFRESH_INTERVAL
                            && shared.pieces_left.load(Ordering::SeqCst) > 0
                        {
                            refresh_inflight = true;
                            last_refresh = std::time::Instant::now();
                            let fut = refresh();
                            let tx = fresh_tx.clone();
                            tokio::spawn(async move {
                                let _ = tx.send(fut.await).await;
                            });
                        }
                    }
                }
            }
        }

        if shared.pieces_left.load(Ordering::SeqCst) == 0 {
            let _ = std::fs::remove_file(&journal_path);
            if let Some(sink) = shared.on_bytes.as_deref() {
                sink(size, size);
            }
            Ok(())
        } else {
            // Keep the journal: a retry (or an explicit Resume) picks up the
            // completed pieces instead of starting over.
            Err(Error::transport("iroh", TransportErrorKind::NotFound))
        }
    }

    /// Run the (`!Send`) download + export on the blob store's local pool, from every
    /// candidate node concurrently. Raced against `cancel` so a user Stop aborts
    /// promptly, keeping any partial blob in the store for a later resume.
    async fn fetch_hash(
        &self,
        hash: Hash,
        format: BlobFormat,
        nodes: Vec<iroh::net::NodeAddr>,
        dest: &Path,
        cancel: Option<Arc<AtomicBool>>,
        on_bytes: Option<BytesProgress>,
    ) -> Result<()> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::fs(parent, e))?;
        }
        let blobs = self.blobs.clone();
        let store = self.store.clone();
        let endpoint = self.endpoint.clone();
        let dest_pb = dest.to_path_buf();

        // The progress channel is created out here (not inside the pool task) so this
        // `select!` loop can drain iroh's download-progress events and forward live
        // byte counts to the UI; the sender moves into the (`!Send`) pool task.
        let (tx, rx) = async_channel::unbounded::<DownloadProgress>();
        let run = self.pool.handle().spawn(move || async move {
            let progress = AsyncChannelProgressSender::new(tx);
            let req = BlobDownloadRequest {
                hash,
                format,
                nodes,
                tag: SetTagOption::Auto,
                mode: DownloadMode::Queued,
            };
            blobs
                .download(endpoint, req, progress)
                .await
                .map_err(|e| format!("download: {e}"))?;
            // A leftover from a previous/aborted attempt would make the export fail
            // (a `rename` export fails on Windows if the dest exists) — remove it
            // first so a retry/redownload always succeeds.
            if tokio::fs::metadata(&dest_pb).await.is_ok() {
                let _ = tokio::fs::remove_file(&dest_pb).await;
            }
            // Export by *reference*, not by copy: `TryReference` moves the downloaded
            // blob out of the iroh store (a free `rename` on the same filesystem) and
            // re-points the entry at the scratch file, so the model is written to disk
            // twice (download + CAS) rather than three times.
            if let Err(e) = store
                .export(
                    hash,
                    dest_pb.clone(),
                    ExportMode::TryReference,
                    Box::new(|_| Ok(())),
                )
                .await
            {
                // The remove above can race a leftover being recreated, which
                // surfaces as "export: File exists (os error 17)". Clear the
                // destination and try once more before giving up.
                let _ = tokio::fs::remove_file(&dest_pb).await;
                store
                    .export(
                        hash,
                        dest_pb,
                        ExportMode::TryReference,
                        Box::new(|_| Ok(())),
                    )
                    .await
                    .map_err(|e2| format!("export: {e2} (first attempt: {e})"))?;
            }
            // Drop the now-redundant store entry: after the move it references the
            // scratch file, so left in place the iroh store would keep a *second*
            // permanent copy of every model. Deleting an `External` entry is a no-op
            // on the underlying file, so the scratch the `select!` loop reads is safe.
            let _ = store.delete(vec![hash]).await;
            Ok::<(), String>(())
        });
        tokio::pin!(run);
        // Forward `(done, total)` to the UI, throttled to ~1 MiB steps so a fast
        // transfer doesn't flood the progress channel. `total` is learned from the
        // first `Found`; `done` tracks the latest `Progress` offset.
        let mut total: u64 = 0;
        let mut last_emit: u64 = 0;
        // Always drain the progress channel — even with no UI sink — so the
        // connect/stall watchdog below can observe whether bytes are actually
        // flowing.
        let mut rx_open = true;
        // Connect/stall watchdog state: `started` anchors the pre-first-byte connect
        // window (an absolute ceiling); `last_progress` anchors the post-first-byte
        // stall window and resets on *every* `Progress` event.
        let started = std::time::Instant::now();
        let mut received_bytes = false;
        let mut last_progress = started;
        loop {
            tokio::select! {
                r = &mut run => {
                    // Land exactly on 100% so the bar doesn't freeze just short of
                    // done while the (network-silent) export finishes.
                    if let Some(sink) = on_bytes.as_deref() {
                        if total > 0 {
                            sink(total, total);
                        }
                    }
                    return r
                        .map_err(|e| ierr("pool join", e))?
                        .map_err(|e| ierr("transfer", e));
                }
                ev = rx.recv(), if rx_open => {
                    match ev {
                        Ok(DownloadProgress::Found { size, .. }) => total = total.max(size),
                        Ok(DownloadProgress::Progress { offset, .. }) => {
                            // Real bytes are flowing: switch the watchdog from the
                            // connect window to the stall window, resetting on every
                            // event (a re-emitted stale offset mustn't keep it alive).
                            received_bytes = true;
                            last_progress = std::time::Instant::now();
                            if let Some(sink) = on_bytes.as_deref() {
                                if total > 0 && offset.saturating_sub(last_emit) >= (1 << 20) {
                                    last_emit = offset;
                                    sink(offset.min(total), total);
                                }
                            }
                        }
                        Ok(_) => {}
                        // Sender dropped (task ended) — stop polling so the arm
                        // doesn't busy-spin on immediate errors. The tick arm below
                        // still runs, so cancel + the watchdog keep working.
                        Err(_) => rx_open = false,
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(150)) => {
                    if cancel.as_ref().map(|c| c.load(Ordering::SeqCst)).unwrap_or(false) {
                        // Returning here drops `run`, which aborts the pool task and
                        // cancels the in-flight download/export.
                        return Err(Error::Cancelled);
                    }
                    // Watchdog: if no size header arrived within the connect window,
                    // or a connected transfer has gone silent past the stall window,
                    // give up. Returning drops `run` (keeping any partial blob for a
                    // later resume) and surfaces `NotFound` so the engine fails over.
                    if fetch_stalled(started.elapsed(), received_bytes, last_progress.elapsed()) {
                        tracing::warn!(
                            connected = received_bytes,
                            "iroh fetch watchdog: providers delivered no data, aborting for failover"
                        );
                        return Err(Error::transport("iroh", TransportErrorKind::NotFound));
                    }
                }
            }
        }
    }

    /// The blake3 hash referenced by a blob ticket (hex), without fetching.
    pub fn ticket_hash(ticket_str: &str) -> Result<String> {
        let ticket: BlobTicket = ticket_str
            .trim()
            .parse()
            .map_err(|e| ierr("ticket parse", e))?;
        Ok(hex::encode(ticket.hash().as_bytes()))
    }

    pub async fn shutdown(self) {
        // `Router` is a cloneable handle over shared state; clone so we don't move
        // a field out of `self` (which now has a `Drop` impl). Dropping `self` at
        // the end aborts the reaper task.
        let _ = self.router.clone().shutdown().await;
    }

    /// Hard-stop sharing **without** consuming the node: stop accepting new
    /// connections (router) *and* close the QUIC endpoint so peers currently pulling
    /// from us are severed at once. Used when the user turns worldwide sharing off —
    /// "stop" must actually disconnect the swarm, not just stop re-announcing.
    pub async fn shutdown_handle(&self) {
        // Bound each step: the UI thread `block_on`s the `stop()` that calls this, so
        // an unresponsive router/endpoint must not freeze the app. On timeout, drop
        // the handles and move on.
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, self.router.clone().shutdown()).await;
        let _ = tokio::time::timeout(
            SHUTDOWN_TIMEOUT,
            self.endpoint.clone().close(0u32.into(), b"share stopped"),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    const MIB: u64 = 1024 * 1024;

    /// The connect/stall watchdog must never trip while inside its window, trip once
    /// the connect window elapses with no bytes, switch to the (longer) stall window
    /// the moment bytes flow, and trip again if a connected transfer goes silent.
    #[test]
    fn watchdog_bounds_connect_and_stall() {
        // Still connecting, within the connect window: keep waiting.
        assert!(!fetch_stalled(
            CONNECT_TIMEOUT - Duration::from_secs(1),
            false,
            Duration::ZERO
        ));
        // Connect window elapsed with no bytes: abort (dead/stale peers).
        assert!(fetch_stalled(CONNECT_TIMEOUT, false, Duration::ZERO));
        assert!(fetch_stalled(
            CONNECT_TIMEOUT + Duration::from_secs(5),
            false,
            Duration::ZERO
        ));

        // Once bytes arrive the connect window no longer applies — a long total
        // elapsed is fine as long as progress is recent (a big, healthy download).
        assert!(!fetch_stalled(
            CONNECT_TIMEOUT * 100,
            true,
            STALL_TIMEOUT - Duration::from_secs(1)
        ));
        // Connected but silent past the stall window: abort (peer dropped).
        assert!(fetch_stalled(CONNECT_TIMEOUT * 100, true, STALL_TIMEOUT));
    }

    /// The stripe partition must tile the blob: contiguous, full-size interior
    /// pieces, and the last piece reaching exactly the end.
    #[test]
    fn stripe_pieces_tile_the_blob_without_gaps() {
        let piece_chunks = STRIPE_PIECE_BYTES / BLAKE3_CHUNK;
        for &size in &[
            0u64,
            1,
            BLAKE3_CHUNK - 1,
            BLAKE3_CHUNK + 1,
            STRIPE_PIECE_BYTES,
            STRIPE_PIECE_BYTES + 1,
            12 * 1024 * 1024 + 123,
        ] {
            let pieces = stripe_pieces(size);
            let total_chunks = size.div_ceil(BLAKE3_CHUNK);
            if total_chunks == 0 {
                assert!(pieces.is_empty(), "empty blob needs no pieces");
                continue;
            }
            // Contiguous from 0 to total_chunks, every interior piece full-size.
            let mut expected_start = 0u64;
            for (i, &(s, e)) in pieces.iter().enumerate() {
                assert_eq!(
                    s, expected_start,
                    "piece {i} must start where the last ended"
                );
                assert!(e > s, "piece {i} must be non-empty");
                if i + 1 < pieces.len() {
                    assert_eq!(e - s, piece_chunks, "interior piece {i} must be full-size");
                }
                expected_start = e;
            }
            assert_eq!(
                pieces.back().unwrap().1,
                total_chunks,
                "the last piece must reach the end of the blob"
            );
        }
    }

    /// Journal round-trip: completed pieces load back for the exact blob+size
    /// (a torn trailing line is dropped, not fatal), and anything stale — wrong
    /// hash, wrong size, missing scratch file — yields a fresh start.
    #[test]
    fn stripe_journal_roundtrip_and_staleness() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("iroh-abc.tmp");
        let size = 3 * STRIPE_PIECE_BYTES + 123;
        let hash = Hash::from([7u8; 32]);
        std::fs::File::create(&dest).unwrap().set_len(size).unwrap();

        let journal = stripe_journal_path(&dest);
        let done: Vec<(u64, u64)> = stripe_pieces(size).iter().take(2).copied().collect();
        let mut text = format!("v1 {size} {}\n", hex::encode(hash.as_bytes()));
        for (s, e) in &done {
            text.push_str(&format!("{s} {e}\n"));
        }
        text.push_str("torn-li"); // killed mid-append
        std::fs::write(&journal, &text).unwrap();

        let loaded = load_stripe_journal(&journal, &hash, size, &dest);
        assert_eq!(loaded, done.iter().copied().collect());

        assert!(
            load_stripe_journal(&journal, &Hash::from([8u8; 32]), size, &dest).is_empty(),
            "wrong hash must not resume"
        );
        assert!(
            load_stripe_journal(&journal, &hash, size + 1, &dest).is_empty(),
            "wrong size must not resume"
        );
        std::fs::remove_file(&dest).unwrap();
        assert!(
            load_stripe_journal(&journal, &hash, size, &dest).is_empty(),
            "missing scratch must not resume"
        );
    }

    /// A full transfer (peer starts from zero) must count exactly the blob size.
    #[test]
    fn full_upload_counts_exactly_the_blob_size() {
        let metrics = IrohMetrics::default();
        let sink = ProviderEventSink::new(metrics.clone());
        let key = (1, 1);
        let hash = Hash::from([0u8; 32]);
        let total = 8 * MIB;
        let chunk = MIB;
        sink.note_started(key, hash);
        let mut off = 0;
        while off < total {
            off += chunk;
            sink.note_progress(key, hash, off); // absolute offset after each chunk
        }
        sink.note_finished(key, Some(total)); // wire total == blob size here
        assert_eq!(metrics.uploaded(), total);
        assert_eq!(metrics.active_uploads(), 0);
    }

    /// Regression: resuming a stopped download must NOT count the prefix the peer
    /// already had. Progress events carry the absolute blob offset, so the first one
    /// after a resume lands at the resume point — counting it produced a fake spike.
    #[test]
    fn resumed_upload_does_not_count_the_prefix_the_peer_already_had() {
        let metrics = IrohMetrics::default();
        let sink = ProviderEventSink::new(metrics.clone());
        let key = (7, 3);
        let hash = Hash::from([0u8; 32]);
        let prefix = 500 * MIB; // the peer already holds [0, 500 MiB)
        let tail = 12 * MIB; // and pulls only this tail
        let chunk = MIB;

        sink.note_started(key, hash);
        // First progress event jumps to the absolute resume offset (baseline).
        sink.note_progress(key, hash, prefix + chunk);
        // Stream the rest of the tail, one chunk at a time.
        let mut off = prefix + chunk;
        let end = prefix + tail;
        while off < end {
            off = (off + chunk).min(end);
            sink.note_progress(key, hash, off);
        }
        // Completion reports the *actual* bytes sent for this request: the tail.
        sink.note_finished(key, Some(tail));

        // We uploaded exactly the tail — never the 500 MiB prefix.
        assert_eq!(metrics.uploaded(), tail);
        assert_eq!(metrics.active_uploads(), 0);
    }

    /// Regression: a peer that quits a pull mid-stream often leaves no
    /// `TransferAborted` on our side (iroh pools its connection), so without the idle
    /// sweep its slot pins the count up forever. The reaper must reclaim a slot gone
    /// silent past the window — and leave a still-active one alone.
    #[test]
    fn reaper_reclaims_a_pull_whose_peer_vanished_without_a_final_event() {
        let metrics = IrohMetrics::default();
        let sink = ProviderEventSink::new(metrics.clone());
        let hash = Hash::from([3u8; 32]);
        let hex = hex::encode(hash.as_bytes());
        sink.note_started((9, 1), hash);
        sink.note_progress((9, 1), hash, 4 * MIB);
        assert_eq!(metrics.active_uploads(), 1);
        assert_eq!(metrics.active_uploads_for_hex(&hex), 1);

        // A generous window reclaims nothing — the slot was just touched.
        assert_eq!(sink.reap_stalled(std::time::Duration::from_secs(3600)), 0);
        assert_eq!(metrics.active_uploads(), 1);

        // A zero window treats it as gone: the slot is reclaimed and the counts
        // fall back to zero, even though no abort event ever arrived.
        assert_eq!(sink.reap_stalled(std::time::Duration::ZERO), 1);
        assert_eq!(metrics.active_uploads(), 0);
        assert_eq!(metrics.active_uploads_for_hex(&hex), 0);
    }

    /// When a serving connection's loop ends, every request still counted on it
    /// must be reclaimed at once — without disturbing pulls on other connections.
    #[test]
    fn connection_close_reclaims_only_its_in_flight_pulls() {
        let metrics = IrohMetrics::default();
        let sink = ProviderEventSink::new(metrics.clone());
        let hash = Hash::from([4u8; 32]);
        let hex = hex::encode(hash.as_bytes());
        sink.note_started((42, 1), hash);
        sink.note_started((42, 2), hash);
        sink.note_started((7, 1), hash);
        assert_eq!(metrics.active_uploads(), 3);
        assert_eq!(metrics.active_uploads_for_hex(&hex), 3);

        // Connection 42 drops: both of its requests clear, connection 7 stays.
        sink.connection_closed(42);
        assert_eq!(metrics.active_uploads(), 1);
        assert_eq!(metrics.active_uploads_for_hex(&hex), 1);
    }

    /// The whole self-exclusion / self-withdraw scheme relies on `node_id_from_store`
    /// producing the *exact same string* as a live node's `node_id()`; if they
    /// diverged, our own announces would leak back as phantom peers. Pin the invariant.
    #[tokio::test]
    async fn derived_node_id_matches_live_node_id() {
        let dir = std::env::temp_dir().join(format!("noema-iroh-id-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // No key yet → nothing to exclude.
        assert_eq!(node_id_from_store(&dir), None);

        let node = IrohNode::spawn(&dir).await.expect("spawn node");
        let live = node.node_id();
        let derived = node_id_from_store(&dir).expect("key persisted after spawn");
        assert_eq!(
            live, derived,
            "offline-derived NodeId must equal the live endpoint's NodeId"
        );
        node.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
