pub mod announce_auth;
pub mod cas;
pub mod db;
pub mod engine;
pub mod error;
pub mod hash;
#[cfg(feature = "http")]
pub mod hf;
pub mod identity;
pub mod inspect;
#[cfg(feature = "iroh")]
pub mod iroh_node;
pub mod manifest;
pub mod paths;
pub mod planner;
pub mod platform;
pub mod policy;
pub mod readme;
pub mod secret;
pub mod share;
pub mod sign;
#[cfg(feature = "http")]
pub mod tracker;
pub mod transfer;
pub mod transport;
pub mod update;
pub mod util;
pub mod verify;

/// The default worldwide P2P content tracker (Noema's hosted instance). Override
/// per-engine via `EngineConfig::tracker_url`.
pub const DEFAULT_TRACKER: &str = "https://atlas.noemaai.com";

pub use cas::{BlobMeta, Cas, LinkKind};
pub use db::Db;
pub use engine::{
    aggregate_results, ArtifactOutcome, BandwidthSchedule, DownloadOutcome, DownloadProgress,
    Engine, EngineConfig, EvictPolicy, FileResult, ImportResult, InstallView, InstalledModel,
    LocalImportOutcome, LocalShareMeta, NetworkModel, Progress, QueueMove, RateLimit,
    ReconcileReport, SourceLocation,
};
#[cfg(feature = "iroh")]
pub use engine::{SeederHandle, WorldwideShare};
pub use error::{Error, Result, TransportErrorKind};
pub use hash::{ChunkTree, DualHasher, Hashes};
pub use inspect::{parse_model, read_file_meta, FileMeta, ParsedModel};
pub use manifest::{Artifact, Manifest, RedistributionClass, Source, SourceClass};
pub use planner::{plan_artifact, DownloadPreference, Plan, ScoredSource};
pub use platform::{Platform, PlatformProfile};
pub use policy::{PolicyConfig, PolicyDecision, PolicyEngine};
pub use secret::{SecretStore, SERVICE_PREFIX};
pub use share::{is_bundle_link, ShareBundle, ShareTarget};
pub use sign::{verify_manifest, KeyPair, VerificationReport};
pub use transfer::{TransferControl, TransferId, TransferManager, TransferState};
pub use transport::{BtPeer, BtPeerProtocol, TransportAdapter, TransportConfig, Transports};
pub use update::{
    AppRelease, PlatformAsset, ReleaseManifest, UpdateSignature, UPDATE_RELEASE_PUBKEYS,
};
pub use verify::{classify_file_safety, FileSafety, StreamingVerifier};
