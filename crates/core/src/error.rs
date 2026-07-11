use std::path::PathBuf;

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// The top-level error type for the engine.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("filesystem error at {path}: {source}")]
    Fs {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("database error: {0}")]
    Db(String),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("cbor (de)serialization error: {0}")]
    Cbor(String),

    #[error("canonicalization error: {0}")]
    Canonical(String),
    #[error("signature error: {0}")]
    Signature(String),

    #[error("key error: {0}")]
    Key(String),

    #[error("manifest is not signed by any trusted key")]
    Untrusted,
    /// A digest did not match what the manifest declared.
    #[error("hash mismatch for {what}: expected {expected}, got {actual}")]
    HashMismatch {
        what: String,
        expected: String,
        actual: String,
    },

    #[error("size mismatch for {what}: expected {expected} bytes, got {actual} bytes")]
    SizeMismatch {
        what: String,
        expected: u64,
        actual: u64,
    },

    #[error("malformed {format} file: {reason}")]
    FormatInvalid { format: String, reason: String },
    #[error("policy denied: {0}")]
    PolicyDenied(String),

    #[error("blocked unsafe file type: {0}")]
    UnsafeFileType(String),

    #[error("path traversal or unsafe artifact path rejected: {0}")]
    UnsafePath(String),
    #[error("transport error from source `{source_id}`: {kind}")]
    Transport {
        source_id: String,
        kind: TransportErrorKind,
    },

    #[error("no eligible source could supply artifact `{0}`")]
    NoEligibleSource(String),

    #[error("authentication required for source `{0}` but no credential is available")]
    AuthRequired(String),
    #[error("manifest `{0}` not found")]
    ManifestNotFound(String),

    #[error("artifact `{0}` not found in manifest")]
    ArtifactNotFound(String),

    #[error("invalid manifest: {0}")]
    InvalidManifest(String),

    #[error("unsupported feature: {0}")]
    Unsupported(String),

    #[error("operation cancelled")]
    Cancelled,

    #[error("download stopped")]
    Stopped,

    #[error("{0}")]
    Other(String),
}

/// Fine-grained classification of transport failures; drives planner retry/failover/ban decisions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TransportErrorKind {
    /// Could not establish a connection / DNS / TLS handshake.
    #[error("connection failed: {0}")]
    Connect(String),

    /// Connected, but the request timed out.
    #[error("timed out")]
    Timeout,

    /// Server returned a non-success HTTP status (or protocol-level error).
    #[error("server status {status}: {message}")]
    Status { status: u16, message: String },

    /// The source does not have the requested object.
    #[error("not found")]
    NotFound,

    /// The source does not support a capability we required (e.g. ranges).
    #[error("capability unsupported: {0}")]
    Unsupported(String),

    /// Source served bytes that failed integrity verification.
    #[error("integrity failure: {0}")]
    Integrity(String),

    /// Authentication was rejected.
    #[error("unauthorized")]
    Unauthorized,

    /// Catch-all transient error.
    #[error("{0}")]
    Other(String),
}

impl TransportErrorKind {
    /// Whether retrying the *same* source could plausibly succeed.
    pub fn is_retriable(&self) -> bool {
        matches!(
            self,
            TransportErrorKind::Timeout
                | TransportErrorKind::Connect(_)
                | TransportErrorKind::Other(_)
                | TransportErrorKind::Status {
                    status: 500..=599,
                    ..
                }
        )
    }

    /// Whether this failure should immediately *ban* the source for the session
    pub fn is_poisoning(&self) -> bool {
        matches!(self, TransportErrorKind::Integrity(_))
    }
}

impl Error {
    /// Build a transport error tied to a particular source id.
    pub fn transport(source_id: impl Into<String>, kind: TransportErrorKind) -> Self {
        Error::Transport {
            source_id: source_id.into(),
            kind,
        }
    }

    /// Build a filesystem error that records the offending path.
    pub fn fs(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Fs {
            path: path.into(),
            source,
        }
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e.to_string())
    }
}
