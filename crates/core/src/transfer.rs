//! Per-transfer control + registry for concurrent downloads.
//!
//! The engine used to run one download at a time behind a pair of global
//! cancel/discard flags. To support concurrent transfers (and a queue), each
//! transfer now owns its own [`TransferControl`]; the one currently executing is
//! stashed in the [`CURRENT_TRANSFER`] task-local so the streaming/verify code can
//! read its cancel flags without threading the control through every function.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Identifies one user-visible transfer. Derived from the manifest id, since a
/// manifest is downloaded by at most one transfer at a time.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TransferId(pub String);

impl TransferId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TransferId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lifecycle of a transfer, surfaced to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferState {
    /// Registered, waiting for a concurrency slot.
    Queued,
    /// Resolving sources / opening a connection.
    Connecting,
    /// Bytes are flowing.
    Downloading,
    /// Re-hashing the downloaded file against the manifest.
    Verifying,
    /// Seeding the committed blob to peers.
    Seeding,
    /// User paused; the partial is kept for resume.
    Paused,
    /// User stopped; the partial was discarded.
    Stopped,
    /// Finished and committed to the CAS.
    Complete,
    /// Terminated by an error.
    Failed,
    /// Connected but no peers/metadata yet (BitTorrent).
    WaitingForPeers,
}

/// Per-transfer cooperative control: the cancel/discard flags that used to be
/// global on the engine, now one set per concurrent transfer.
#[derive(Debug)]
pub struct TransferControl {
    pub id: TransferId,
    pub manifest_id: String,
    /// Cooperative cancel flag, kept as a shared atomic so it can be handed to
    /// in-`open()` transports via `FetchCtx` and to blocking verify tasks.
    pub cancel: Arc<AtomicBool>,
    /// `true` = a Stop (discard the partial); `false` = a Pause (keep it).
    pub discard_partial: Arc<AtomicBool>,
    state: Mutex<TransferState>,
    /// Set while a `run_transfer` task is executing this control, so a second
    /// concurrent run (e.g. the UI resuming a still-queued transfer) is rejected
    /// instead of racing two writers on the same partial file.
    executing: AtomicBool,
}

impl TransferControl {
    pub fn new(manifest_id: impl Into<String>) -> Self {
        let manifest_id = manifest_id.into();
        TransferControl {
            id: TransferId(manifest_id.clone()),
            manifest_id,
            cancel: Arc::new(AtomicBool::new(false)),
            discard_partial: Arc::new(AtomicBool::new(false)),
            state: Mutex::new(TransferState::Queued),
            executing: AtomicBool::new(false),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    pub fn discard_requested(&self) -> bool {
        self.discard_partial.load(Ordering::SeqCst)
    }

    /// Request a pause: keep the partial on disk for a later resume.
    pub fn request_pause(&self) {
        self.discard_partial.store(false, Ordering::SeqCst);
        self.cancel.store(true, Ordering::SeqCst);
        self.set_state(TransferState::Paused);
    }

    /// Request a stop: discard the partial. The discard intent is stored before
    /// the cancel flag so a loop that observes `cancel` also sees the discard.
    pub fn request_stop(&self) {
        self.discard_partial.store(true, Ordering::SeqCst);
        self.cancel.store(true, Ordering::SeqCst);
        self.set_state(TransferState::Stopped);
    }

    /// Clear the cancel/discard flags for a fresh or resumed run.
    pub fn reset(&self) {
        self.cancel.store(false, Ordering::SeqCst);
        self.discard_partial.store(false, Ordering::SeqCst);
    }

    pub fn state(&self) -> TransferState {
        *self.state.lock().expect("transfer state mutex poisoned")
    }

    pub fn set_state(&self, s: TransferState) {
        *self.state.lock().expect("transfer state mutex poisoned") = s;
    }

    /// Mark this control as executing; returns `false` if a run is already in
    /// progress (so the caller must NOT start a second one — that would race two
    /// writers on the same partial). Pair with [`TransferControl::end`].
    pub fn try_begin(&self) -> bool {
        if self.executing.swap(true, Ordering::SeqCst) {
            return false; // already running — do NOT clear a concurrent run's flags
        }
        // We are the sole runner now: clear stale pause/stop flags from a prior
        // attempt here (atomically with admission) so a Stop issued the instant we
        // become executing isn't erased by a separate later reset().
        self.cancel.store(false, Ordering::SeqCst);
        self.discard_partial.store(false, Ordering::SeqCst);
        true
    }

    /// Mark this control as no longer executing.
    pub fn end(&self) {
        self.executing.store(false, Ordering::SeqCst);
    }

    /// Whether a `run_transfer` task is currently driving this transfer.
    pub fn is_executing(&self) -> bool {
        self.executing.load(Ordering::SeqCst)
    }
}

/// Registry of live transfers, keyed by id.
#[derive(Default)]
pub struct TransferManager {
    inner: Mutex<HashMap<TransferId, Arc<TransferControl>>>,
}

impl TransferManager {
    /// Register a control for a manifest, or return the existing one.
    pub fn register(&self, manifest_id: &str) -> Arc<TransferControl> {
        let mut g = self.inner.lock().expect("transfer registry poisoned");
        let id = TransferId(manifest_id.to_string());
        if let Some(c) = g.get(&id) {
            return c.clone();
        }
        let ctl = Arc::new(TransferControl::new(manifest_id));
        g.insert(id, ctl.clone());
        ctl
    }

    pub fn get(&self, id: &TransferId) -> Option<Arc<TransferControl>> {
        self.inner
            .lock()
            .expect("transfer registry poisoned")
            .get(id)
            .cloned()
    }

    pub fn remove(&self, id: &TransferId) {
        self.inner
            .lock()
            .expect("transfer registry poisoned")
            .remove(id);
    }

    pub fn all(&self) -> Vec<Arc<TransferControl>> {
        self.inner
            .lock()
            .expect("transfer registry poisoned")
            .values()
            .cloned()
            .collect()
    }
}

tokio::task_local! {
    /// The control for the transfer running on the current task. Set by
    /// `Engine::run_transfer` via `CURRENT_TRANSFER.scope(...)`, so the streaming
    /// and verify code can poll its cancel flags without an extra parameter.
    pub static CURRENT_TRANSFER: Arc<TransferControl>;
}

/// The current task's transfer control, if running inside a transfer.
pub fn current() -> Option<Arc<TransferControl>> {
    CURRENT_TRANSFER.try_with(|c| c.clone()).ok()
}
