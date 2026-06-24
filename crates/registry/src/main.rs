use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use noema_core::manifest::{Manifest, PublicKey};
use noema_core::sign::verify_manifest;
use noema_core::util::{now_unix_millis, slugify};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// How long a peer's "I have this content" announcement is trusted before it
/// must re-announce. Keeps the index fresh as nodes go offline: a peer that
/// removed a model (or crashed/force-quit without withdrawing) drops out of peer
/// counts within this window. Sized to comfortably outlast the client re-announce
/// cadence — 5 min for current clients, 10 min for older ones — so a live peer is
/// never dropped for a single missed beat, while halving the stale-entry window
/// from the original 30 min.
const PROVIDER_TTL_MS: i64 = 15 * 60 * 1000;

/// Worldwide content index. Providers are keyed by the file's **blake3** (Iroh's
/// content address); a `sha256 -> blake3` alias lets a client that only knows the
/// Hugging Face sha256 resolve the blake3 it needs to fetch over Iroh.
///
/// Browse metadata (name/size/license/listable/group/device) lives on **each
/// provider record**, not as a single content-global field. Public visibility
/// and group membership are therefore derived from the *currently-live* set of
/// providers: one peer cannot promote, hijack, or rewrite another peer's
/// listing, and a flag reverts as soon as its provider expires. Announces are
/// authenticated — each is keyed on a signature-verified NodeId — so no one can
/// publish under another node's id; a peer can still mislabel a hash *it* serves,
/// but bytes are always blake3/sha256-verified on fetch, so a bad label can never
/// deliver wrong content.
#[derive(Default)]
struct Tracker {
    providers: HashMap<String, Vec<Provider>>,
    alias: HashMap<String, String>, // sha256 -> blake3
}

#[derive(Clone)]
struct Provider {
    /// The current reachable node ticket (addresses change over time).
    node: String,
    /// The peer's *stable* identity (hex NodeId), cryptographically verified on
    /// announce. This — not the ticket — is the de-duplication key, so one device
    /// that re-announces with changed addresses stays a single peer instead of
    /// inflating the count.
    node_id: String,
    device: String,
    group: Option<String>,
    listable: bool,
    sha256: String,
    name: String,
    size: u64,
    quant: String,
    license: String,
    magnet: String,
    expires_at: i64,
}

/// One item a peer announces: content ids + optional browse metadata.
struct AnnouncedItem {
    blake3: String,
    sha256: Option<String>,
    name: String,
    size: u64,
    quant: String,
    license: String,
    magnet: String,
    listable: bool,
}

/// A browsable catalog row returned to clients.
#[derive(Serialize)]
struct CatalogRow {
    blake3: String,
    sha256: String,
    name: String,
    size: u64,
    quant: String,
    license: String,
    magnet: String,
    peers: usize,
    devices: Vec<String>,
    mine: bool,
}

impl Tracker {
    fn announce(
        &mut self,
        node: &str,
        node_id: &str,
        device: Option<&str>,
        group: Option<&str>,
        items: &[AnnouncedItem],
    ) {
        let now = now_unix_millis();
        let exp = now + PROVIDER_TTL_MS;
        // Rows are keyed on the cryptographically-verified stable NodeId (the caller
        // proves ownership before this is reached), never an attacker-supplied value.
        let id = node_id.to_string();
        for it in items {
            let b3 = it.blake3.to_lowercase();
            let sha = it
                .sha256
                .as_deref()
                .map(|s| s.to_lowercase())
                .filter(|s| s.len() == 64)
                .unwrap_or_default();
            let v = self.providers.entry(b3.clone()).or_default();
            // One record per device: replace this peer's prior claim, drop expired.
            v.retain(|p| p.node_id != id && p.expires_at > now);
            v.push(Provider {
                node: node.to_string(),
                node_id: id.clone(),
                device: device.unwrap_or("").to_string(),
                group: group.map(|g| g.to_string()),
                listable: it.listable,
                sha256: sha.clone(),
                name: it.name.clone(),
                size: it.size,
                quant: it.quant.clone(),
                license: it.license.clone(),
                magnet: it.magnet.clone(),
                expires_at: exp,
            });
            if !sha.is_empty() {
                self.alias.insert(sha, b3.clone());
            }
        }
    }

    /// Drop expired providers (and empty/zombie keys + dangling aliases).
    /// Returns the live distinct-peer count per blake3.
    fn prune(&mut self) -> HashMap<String, usize> {
        let now = now_unix_millis();
        let mut peers_by_b3: HashMap<String, usize> = HashMap::new();
        self.providers.retain(|b3, v| {
            v.retain(|p| p.expires_at > now);
            if v.is_empty() {
                return false;
            }
            // Distinct peers are counted by stable NodeId, so one device that
            // re-announced under a changed ticket isn't double-counted.
            let distinct: std::collections::HashSet<&str> =
                v.iter().map(|p| p.node_id.as_str()).collect();
            peers_by_b3.insert(b3.clone(), distinct.len());
            true
        });
        // The alias map is the one unbounded structure otherwise — keep only
        // aliases whose target blake3 still has live providers.
        self.alias.retain(|_, b3| peers_by_b3.contains_key(b3));
        peers_by_b3
    }

    /// Live network stats: (distinct files shared, distinct peers online).
    fn stats(&mut self) -> (usize, usize) {
        let peers_by_b3 = self.prune();
        let files = peers_by_b3.len();
        let mut peers = std::collections::HashSet::new();
        for v in self.providers.values() {
            for p in v.iter() {
                peers.insert(p.node_id.clone());
            }
        }
        (files, peers.len())
    }

    /// Resolve a query hash (blake3 or sha256) to (blake3, live providers).
    /// `exclude` drops a caller's own NodeId, so a peer never discovers *itself*
    /// as a provider (which would both inflate its "seeding worldwide" count and
    /// make it try to fetch a file from its own node).
    fn get(&mut self, hash: &str, exclude: Option<&str>) -> (String, Vec<Provider>) {
        let now = now_unix_millis();
        let h = hash.to_lowercase();
        let blake3 = self.alias.get(&h).cloned().unwrap_or(h);
        let providers = match self.providers.get_mut(&blake3) {
            Some(v) => {
                v.retain(|p| p.expires_at > now);
                v.iter()
                    .filter(|p| exclude.map(|x| x != p.node_id).unwrap_or(true))
                    .cloned()
                    .collect()
            }
            None => Vec::new(),
        };
        (blake3, providers)
    }

    /// Browse the catalog, deriving each row from a file's live providers. A row
    /// is public if **any live provider** announced it as `listable`; it is
    /// `mine` if the querier is itself one of the providers (matched on its own
    /// `self_id` NodeId) *or* a provider announced it under the querier's group id.
    /// Displayed metadata comes from a representative live provider (preferring a
    /// listable one for public rows), so an expired or non-listable announcer
    /// can't dictate a public listing's labels.
    ///
    /// `self_id` is excluded from `peers`, so your own device is not shown as a
    /// downloadable peer.
    fn browse(
        &mut self,
        q: &str,
        group: Option<&str>,
        self_id: Option<&str>,
        limit: usize,
    ) -> Vec<CatalogRow> {
        let peers_by_b3 = self.prune();
        let q = q.to_lowercase();
        let self_id = self_id.filter(|s| !s.is_empty());
        let mut rows: Vec<CatalogRow> = Vec::new();
        for (b3, provs) in self.providers.iter() {
            let public = provs.iter().any(|p| p.listable);
            // You are "mine" if you're one of the live providers (by NodeId) or a
            // provider matches your device group.
            let self_is_provider = self_id.is_some_and(|s| provs.iter().any(|p| p.node_id == s));
            let mine = self_is_provider
                || (group.is_some() && provs.iter().any(|p| p.group.as_deref() == group));
            if !public && !mine {
                continue;
            }
            // Pick a representative provider for the displayed metadata: prefer a
            // listable one (for public rows), most-recently-seen.
            let rep = provs
                .iter()
                .filter(|p| !public || p.listable)
                .max_by_key(|p| p.expires_at)
                .or_else(|| provs.iter().max_by_key(|p| p.expires_at));
            let Some(rep) = rep else { continue };
            if !q.is_empty() && !rep.name.to_lowercase().contains(&q) {
                continue;
            }
            let devices: std::collections::BTreeSet<String> = provs
                .iter()
                .filter(|p| !p.device.is_empty())
                .map(|p| p.device.clone())
                .collect();
            // Distinct live peers for this file, minus yourself: "peers" means
            // *other* devices you could fetch it from.
            let total = peers_by_b3.get(b3).copied().unwrap_or(provs.len());
            let peers = total.saturating_sub(usize::from(self_is_provider));
            // Magnet from any live provider that has one (a single seeding peer is
            // enough to advertise the swarm), falling back to the representative's.
            let magnet = provs
                .iter()
                .map(|p| p.magnet.as_str())
                .find(|m| !m.is_empty())
                .unwrap_or(rep.magnet.as_str())
                .to_string();
            rows.push(CatalogRow {
                blake3: b3.clone(),
                sha256: rep.sha256.clone(),
                name: rep.name.clone(),
                size: rep.size,
                quant: rep.quant.clone(),
                license: rep.license.clone(),
                magnet,
                peers,
                devices: devices.into_iter().collect(),
                mine,
            });
        }
        // "Mine" first, then by peer count, then name for stability.
        rows.sort_by(|a, b| {
            b.mine
                .cmp(&a.mine)
                .then(b.peers.cmp(&a.peers))
                .then(a.name.cmp(&b.name))
        });
        rows.truncate(limit);
        rows
    }

    /// Drop a peer's provider records immediately. Empty `blakes` withdraws every
    /// record for this stable NodeId.
    fn withdraw(&mut self, node_id: &str, blakes: &[String]) {
        if node_id.is_empty() {
            return;
        }
        if blakes.is_empty() {
            for v in self.providers.values_mut() {
                v.retain(|p| p.node_id != node_id);
            }
        } else {
            for b3 in blakes {
                if let Some(v) = self.providers.get_mut(&b3.to_lowercase()) {
                    v.retain(|p| p.node_id != node_id);
                }
            }
        }
        self.prune();
    }
}

#[derive(Parser)]
#[command(
    name = "noema-registry",
    about = "Manifest publication + key metadata service"
)]
struct Args {
    /// Bind address.
    #[arg(long, default_value = "0.0.0.0:8077")]
    addr: String,
    /// Directory to persist manifests.
    #[arg(long, default_value = "./registry-data")]
    dir: PathBuf,
    /// This registry's own canonical public base URL. It's bound into every
    /// announce/withdraw signature as the *audience*, so a request captured for one
    /// registry can't be replayed against another. Recomputed from this value — not
    /// from an attacker-controllable `Host` header — so it must match the URL clients
    /// post to. Defaults to the well-known Atlas tracker URL.
    #[arg(long, default_value = noema_core::DEFAULT_TRACKER)]
    public_url: String,
    /// Require a signed ownership proof on every announce/withdraw. Off by default
    /// for a transition window: already-released, *unsigned* clients keep working,
    /// while signed clients are still cryptographically verified (a present-but-bad
    /// signature is always rejected). Flip this on once clients have upgraded, to
    /// reject all unsigned requests.
    #[arg(long)]
    require_signed: bool,
}

struct Store {
    dir: PathBuf,
    manifests: HashMap<String, Manifest>,
    /// model slug -> manifest ids (in insertion order; last is "latest").
    by_slug: HashMap<String, Vec<String>>,
}

impl Store {
    fn load(dir: PathBuf) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let mut manifests = HashMap::new();
        let mut by_slug: HashMap<String, Vec<String>> = HashMap::new();
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(m) = Manifest::from_json(&bytes) {
                    let slug = slugify(&m.model.name);
                    by_slug.entry(slug).or_default().push(m.manifest_id.clone());
                    manifests.insert(m.manifest_id.clone(), m);
                }
            }
        }
        Ok(Store {
            dir,
            manifests,
            by_slug,
        })
    }

    fn insert(&mut self, m: Manifest) -> anyhow::Result<()> {
        let path = self.dir.join(format!("{}.json", m.manifest_id));
        std::fs::write(&path, m.to_json_pretty()?)?;
        let slug = slugify(&m.model.name);
        let ids = self.by_slug.entry(slug).or_default();
        if !ids.contains(&m.manifest_id) {
            ids.push(m.manifest_id.clone());
        }
        self.manifests.insert(m.manifest_id.clone(), m);
        Ok(())
    }
}

#[derive(Clone)]
struct AppState {
    store: Arc<Mutex<Store>>,
    tracker: Arc<Mutex<Tracker>>,
    /// This registry's canonical base URL, bound as the audience in every
    /// announce/withdraw signature (cross-registry replay protection).
    public_url: Arc<str>,
    /// When true, reject any unsigned announce/withdraw; when false (transition),
    /// accept unsigned legacy clients but still verify any proof that *is* present.
    require_signed: bool,
}

#[derive(Serialize)]
struct ApiError {
    error: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(ApiError { error: msg.into() })).into_response()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "noema_registry=info".into()),
        )
        .init();

    let args = Args::parse();
    let store = Store::load(args.dir.clone())?;
    let state = AppState {
        store: Arc::new(Mutex::new(store)),
        tracker: Arc::new(Mutex::new(Tracker::default())),
        public_url: Arc::from(args.public_url.as_str()),
        require_signed: args.require_signed,
    };

    let app = Router::new()
        .route("/", get(landing))
        .route("/stats", get(stats))
        .route("/logo.png", get(logo))
        .route("/health", get(health))
        .route("/manifests", post(publish))
        .route("/manifests/:id", get(get_manifest))
        .route("/search", get(search))
        .route("/publishers/:id/keys", get(publisher_keys))
        .route("/models/:slug/latest", get(model_latest))
        .route("/signatures/verify", post(verify))
        // Worldwide P2P content tracker:
        .route("/announce", post(announce))
        .route("/withdraw", post(withdraw))
        .route("/providers/:hash", get(providers))
        .route("/catalog", get(catalog))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&args.addr).await?;
    tracing::info!("noema-registry listening on {}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

/// The Noema Atlas brand page (also the tracker host).
const LANDING_HTML: &str = include_str!("landing.html");
const LOGO_PNG: &[u8] = include_bytes!("../../../assets/logo.png");

async fn landing() -> Html<&'static str> {
    Html(LANDING_HTML)
}

async fn logo() -> Response {
    ([(header::CONTENT_TYPE, "image/png")], LOGO_PNG).into_response()
}

/// Live network stats for the brand page's swarm dashboard.
async fn stats(State(state): State<AppState>) -> Response {
    let (files, peers) = state.tracker.lock().unwrap().stats();
    let manifests = state.store.lock().unwrap().manifests.len();
    Json(serde_json::json!({
        "service": "noema-atlas",
        "files_shared": files,
        "peers_online": peers,
        "manifests": manifests,
    }))
    .into_response()
}

async fn health() -> impl IntoResponse {
    Json(
        serde_json::json!({"service":"noema-registry","status":"ok","version":env!("CARGO_PKG_VERSION")}),
    )
}

async fn publish(State(state): State<AppState>, body: Bytes) -> Response {
    let manifest = match Manifest::from_json(&body) {
        Ok(m) => m,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("invalid manifest: {e}")),
    };
    if let Err(e) = manifest.validate() {
        return err(StatusCode::BAD_REQUEST, format!("validation failed: {e}"));
    }
    let report = match verify_manifest(&manifest) {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                format!("signature check failed: {e}"),
            )
        }
    };
    if !report.is_signed() {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "manifest has no valid signature; the registry only stores signed manifests",
        );
    }
    let id = manifest.manifest_id.clone();
    if let Err(e) = state.store.lock().unwrap().insert(manifest) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("store error: {e}"),
        );
    }
    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "manifest_id": id,
            "valid_signatures": report.valid_signatures,
        })),
    )
        .into_response()
}

/// Search stored manifests by model name / publisher / id. Returns full
/// manifests so a client can show every source and import on demand.
async fn search(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let q = params.get("q").cloned().unwrap_or_default().to_lowercase();
    let store = state.store.lock().unwrap();
    let results: Vec<Manifest> = store
        .manifests
        .values()
        .filter(|m| {
            q.is_empty()
                || format!("{} {} {}", m.model.name, m.publisher.id, m.manifest_id)
                    .to_lowercase()
                    .contains(&q)
        })
        .cloned()
        .collect();
    Json(results).into_response()
}

async fn get_manifest(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.store.lock().unwrap().manifests.get(&id) {
        Some(m) => Json(m.clone()).into_response(),
        None => err(StatusCode::NOT_FOUND, "no such manifest"),
    }
}

async fn publisher_keys(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.store.lock().unwrap();
    let mut keys: Vec<PublicKey> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for m in store.manifests.values() {
        if m.publisher.id == id {
            for k in &m.publisher.public_keys {
                if seen.insert(k.key_id.clone()) {
                    keys.push(k.clone());
                }
            }
        }
    }
    Json(serde_json::json!({"publisher_id": id, "public_keys": keys})).into_response()
}

async fn model_latest(State(state): State<AppState>, Path(slug): Path<String>) -> Response {
    let store = state.store.lock().unwrap();
    match store.by_slug.get(&slug).and_then(|ids| ids.last()) {
        Some(id) => match store.manifests.get(id) {
            Some(m) => Json(m.clone()).into_response(),
            None => err(StatusCode::NOT_FOUND, "no such model"),
        },
        None => err(StatusCode::NOT_FOUND, "no such model"),
    }
}

#[derive(Deserialize)]
struct AnnounceItem {
    blake3: String,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    quant: String,
    #[serde(default)]
    license: String,
    /// BitTorrent magnet (info-hash) when the sharer is seeding this file over BT.
    #[serde(default)]
    magnet: String,
    /// Whether this file may appear in the public catalog (the sharer opted it in).
    #[serde(default)]
    listable: bool,
}

#[derive(Deserialize)]
struct AnnounceReq {
    /// The announcer's Iroh node ticket (reachable worldwide via relays). Bound into
    /// the signature, so a MITM can't swap it for an address it controls.
    node: String,
    /// The announcer's *stable* NodeId (hex/base32) — the peer de-dup key, and the
    /// public key the signature is verified against. Mandatory: an announce without a
    /// proven NodeId is rejected.
    #[serde(default)]
    node_id: Option<String>,
    /// Human device name (for "from your devices").
    #[serde(default)]
    device: Option<String>,
    /// Device-group capability id (private shares are scoped to it).
    #[serde(default)]
    group: Option<String>,
    /// Content this peer is sharing, with optional browse metadata.
    #[serde(default)]
    items: Vec<AnnounceItem>,
    /// Ownership-proof timestamp (ms) + base64 Ed25519 signature over the canonical
    /// payload, signed with the node secret key (see `noema_core::announce_auth`).
    /// Both are mandatory — an unsigned announce is rejected.
    #[serde(default)]
    ts: Option<i64>,
    #[serde(default)]
    sig: Option<String>,
}

/// A peer announces the content it shares so others can find it worldwide. The
/// announce carries a NodeId and, for upgraded clients, a ts + signature over the
/// canonical payload (binding the node ticket, this registry's audience, and the
/// announced blake3s), verified *before* any state mutation. During the transition
/// window an unsigned legacy client is still accepted (see `--require-signed`); a
/// signature that IS present is always verified, so a forged/replayed proof is
/// rejected. Catalog/provider rows are keyed on the announced NodeId.
async fn announce(State(state): State<AppState>, Json(req): Json<AnnounceReq>) -> Response {
    if req.node.trim().is_empty() || req.items.is_empty() {
        return err(StatusCode::BAD_REQUEST, "node and items are required");
    }
    // Proof of ownership is mandatory: a missing/empty node_id is unauthenticated.
    let Some(node_id) = req.node_id.as_deref().filter(|s| !s.is_empty()) else {
        return err(
            StatusCode::UNAUTHORIZED,
            "announce requires a signed node_id",
        );
    };
    // Verify the Ed25519 signature over the announced blake3s — bound to the received
    // ticket and this registry's own canonical URL as the audience — against the
    // claimed NodeId (which *is* the public key), BEFORE touching any state. Reject a
    // spoofed, unsigned, MITM-rewritten, or cross-registry-replayed request.
    let item_ids: Vec<String> = req.items.iter().map(|i| i.blake3.clone()).collect();
    if !announce_authorized(
        state.require_signed,
        "announce",
        node_id,
        req.ts,
        req.sig.as_deref(),
        &req.node,
        &state.public_url,
        &item_ids,
    ) {
        return err(
            StatusCode::UNAUTHORIZED,
            "invalid or missing announce signature",
        );
    }
    let items: Vec<AnnouncedItem> = req
        .items
        .into_iter()
        .take(10_000)
        .filter(|i| i.blake3.len() == 64)
        .map(|i| AnnouncedItem {
            blake3: i.blake3,
            sha256: i.sha256,
            name: i.name,
            size: i.size,
            quant: i.quant,
            license: i.license,
            magnet: i.magnet,
            listable: i.listable,
        })
        .collect();
    let n = items.len();
    state.tracker.lock().unwrap().announce(
        &req.node,
        node_id,
        req.device.as_deref(),
        req.group.as_deref(),
        &items,
    );
    (
        StatusCode::CREATED,
        Json(serde_json::json!({"ok": true, "announced": n})),
    )
        .into_response()
}

/// Browse the worldwide catalog of shared models. Query params: `q` (name
/// filter), `group` (capability id to also see your group's private shares),
/// `self` (the caller's own NodeId — excluded from each row's peer count and used
/// to flag your own shares as `mine`), `limit` (default 100, max 500).
async fn catalog(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let q = params.get("q").cloned().unwrap_or_default();
    let group = params
        .get("group")
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty());
    let self_id = params
        .get("self")
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty());
    let limit = params
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100)
        .min(500);
    let rows = state
        .tracker
        .lock()
        .unwrap()
        .browse(&q, group, self_id, limit);
    Json(serde_json::json!({ "models": rows })).into_response()
}

#[derive(Deserialize)]
struct WithdrawReq {
    /// The peer's stable NodeId — it can only withdraw records under this id.
    node_id: String,
    /// blake3 hashes to withdraw; empty/absent means "withdraw everything".
    #[serde(default)]
    items: Vec<String>,
    /// Ownership-proof timestamp (ms) + base64 Ed25519 signature over the canonical
    /// payload. Without it a registry can't tell a real owner from an attacker
    /// trying to wipe a victim's records.
    #[serde(default)]
    ts: Option<i64>,
    #[serde(default)]
    sig: Option<String>,
}

/// A peer un-announces content it no longer serves (deleted, stopped sharing, or
/// shutting down), so it drops out of the catalog and provider lists right away
/// rather than lingering until its TTL.
async fn withdraw(State(state): State<AppState>, Json(req): Json<WithdrawReq>) -> Response {
    if req.node_id.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "node_id is required");
    }
    // Withdraw is the dangerous verb (it deletes records). When present, the signature
    // over the requested item ids is verified against the claimed NodeId so no one can
    // wipe another peer's entries; during the transition window an unsigned legacy
    // withdraw is still honored (a stale entry would otherwise just TTL out anyway).
    // Withdraw carries no node ticket, so the bound ticket is empty; the audience is
    // this registry's own URL (cross-registry replay protection).
    if !announce_authorized(
        state.require_signed,
        "withdraw",
        &req.node_id,
        req.ts,
        req.sig.as_deref(),
        "",
        &state.public_url,
        &req.items,
    ) {
        return err(
            StatusCode::UNAUTHORIZED,
            "invalid or missing withdraw signature",
        );
    }
    state
        .tracker
        .lock()
        .unwrap()
        .withdraw(&req.node_id, &req.items);
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

/// Verify an announce/withdraw ownership proof: the `ts` + `sig` must be a valid,
/// fresh Ed25519 signature over the canonical payload — bound to `ticket` (the
/// node ticket the registry received) and `audience` (this registry's own canonical
/// URL) — signed by the secret key whose public key *is* `node_id`. Missing `ts`/`sig`
/// is a rejection (no proof).
#[allow(clippy::too_many_arguments)]
fn verify_announce_auth(
    method: &str,
    node_id: &str,
    ts: Option<i64>,
    sig: Option<&str>,
    ticket: &str,
    audience: &str,
    item_ids: &[String],
) -> bool {
    let (Some(ts), Some(sig)) = (ts, sig) else {
        return false;
    };
    noema_core::announce_auth::verify(
        method,
        node_id,
        ts,
        ticket,
        audience,
        item_ids,
        sig,
        now_unix_millis(),
    )
}

/// Authorize an announce/withdraw under the registry's auth mode.
/// - A request carrying a `ts` + `sig` is ALWAYS cryptographically verified; a
///   present-but-invalid proof (forged, tampered, replayed) is rejected regardless
///   of mode.
/// - A request with no proof is accepted only during the transition window
///   (`require_signed == false`), so already-released unsigned clients keep working;
///   with `require_signed == true` it is rejected.
#[allow(clippy::too_many_arguments)]
fn announce_authorized(
    require_signed: bool,
    method: &str,
    node_id: &str,
    ts: Option<i64>,
    sig: Option<&str>,
    ticket: &str,
    audience: &str,
    item_ids: &[String],
) -> bool {
    if ts.is_some() && sig.is_some() {
        verify_announce_auth(method, node_id, ts, sig, ticket, audience, item_ids)
    } else {
        !require_signed
    }
}

/// Look up peers worldwide that have a given content hash (blake3 or sha256).
/// The optional `self` query param is the caller's own NodeId; it's excluded
/// from the result so a peer never sees (or fetches from) itself.
async fn providers(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let exclude = params
        .get("self")
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty());
    let (blake3, ps) = state.tracker.lock().unwrap().get(&hash, exclude);
    let now = now_unix_millis();
    Json(serde_json::json!({
        "hash": hash,
        "blake3": blake3,
        "providers": ps.iter().map(|p| serde_json::json!({
            "node": p.node,
            "ttl_secs": ((p.expires_at - now) / 1000).max(0),
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

async fn verify(body: Bytes) -> Response {
    let manifest = match Manifest::from_json(&body) {
        Ok(m) => m,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("invalid manifest: {e}")),
    };
    match verify_manifest(&manifest) {
        Ok(report) => Json(serde_json::json!({
            "signed": report.is_signed(),
            "valid_signatures": report.valid_signatures,
            "invalid_signatures": report.invalid_signatures,
            "total_signatures": report.total_signatures,
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("verify error: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(b3: &str, name: &str, listable: bool) -> AnnouncedItem {
        AnnouncedItem {
            blake3: b3.to_string(),
            // A valid 64-hex sha256 distinct from the blake3 (digit-substituted),
            // so the alias only stores well-formed shas.
            sha256: Some(fake_sha(b3)),
            name: name.to_string(),
            size: 100,
            quant: String::new(),
            license: "apache-2.0".to_string(),
            magnet: String::new(),
            listable,
        }
    }

    fn fake_sha(b3: &str) -> String {
        b3.chars()
            .map(|c| match c {
                'a' => '1',
                'b' => '2',
                'c' => '3',
                'd' => '4',
                'e' => '5',
                other => other,
            })
            .collect()
    }

    #[test]
    fn catalog_public_vs_group_private() {
        let mut t = Tracker::default();
        let pub_b3 = "aa".repeat(32);
        let priv_b3 = "bb".repeat(32);
        t.announce(
            "nodeA",
            "nodeA",
            Some("Mac A"),
            Some("grp1"),
            &[
                item(&pub_b3, "Qwen3 0.6B", true),
                item(&priv_b3, "Secret Model", false),
            ],
        );

        // Anonymous browse: only the public (opted-in / listable) model.
        let anon = t.browse("", None, None, 100);
        assert_eq!(anon.len(), 1);
        assert_eq!(anon[0].name, "Qwen3 0.6B");
        assert!(!anon[0].mine);
        assert_eq!(anon[0].peers, 1);

        // Browsing with the group code also reveals the private one, flagged mine.
        let grp = t.browse("", Some("grp1"), None, 100);
        assert_eq!(grp.len(), 2);
        assert!(grp.iter().any(|r| r.name == "Secret Model" && r.mine));
        assert!(grp.iter().all(|r| r.mine));
        let other = t.browse("", Some("grpX"), None, 100);
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].name, "Qwen3 0.6B");
        assert!(!other[0].mine);
        let q = t.browse("qwen", Some("grp1"), None, 100);
        assert_eq!(q.len(), 1);

        // sha256 alias resolves to the blake3 + providers for fetching.
        let (b3, ps) = t.get(&fake_sha(&pub_b3), None);
        assert_eq!(b3, pub_b3);
        assert_eq!(ps.len(), 1);
    }

    #[test]
    fn catalog_entries_drop_when_providers_expire() {
        let mut t = Tracker::default();
        let b3 = "cc".repeat(32);
        t.announce(
            "nodeA",
            "nodeA",
            Some("Mac A"),
            None,
            &[item(&b3, "Open Model", true)],
        );
        assert_eq!(t.browse("", None, None, 100).len(), 1);
        // Force expiry: providers in the past => prune drops the catalog entry.
        for v in t.providers.values_mut() {
            for p in v.iter_mut() {
                p.expires_at = 0;
            }
        }
        assert_eq!(t.browse("", None, None, 100).len(), 0);
        assert_eq!(t.stats().0, 0);
    }

    #[test]
    fn private_metadata_never_leaks_to_anonymous_browse() {
        let mut t = Tracker::default();
        let x = "dd".repeat(32);
        // Owner A shares X privately (not listable) under group A.
        t.announce(
            "A",
            "A",
            Some("Mac A"),
            Some("grpA"),
            &[item(&x, "A-Private-Name", false)],
        );
        // Anonymous browse must not see it at all.
        assert!(t.browse("", None, None, 100).is_empty());

        // A second peer B announces the SAME content publicly (a spoof or a
        // genuine public re-share). Now the hash is publicly browsable — but the
        // public row must show B's label, never A's private one.
        t.announce(
            "B",
            "B",
            Some("Mac B"),
            None,
            &[item(&x, "B-Public-Name", true)],
        );
        let anon = t.browse("", None, None, 100);
        assert_eq!(anon.len(), 1);
        assert_eq!(anon[0].name, "B-Public-Name");
        assert_ne!(anon[0].name, "A-Private-Name");
    }

    #[test]
    fn two_groups_share_same_content_without_hijacking_each_other() {
        let mut t = Tracker::default();
        let x = "ee".repeat(32);
        t.announce(
            "A",
            "A",
            Some("Mac A"),
            Some("grpA"),
            &[item(&x, "Model", false)],
        );
        t.announce(
            "B",
            "B",
            Some("Mac B"),
            Some("grpB"),
            &[item(&x, "Model", false)],
        );
        // Each group still sees it as theirs; neither overwrote the other.
        let a = t.browse("", Some("grpA"), None, 100);
        assert_eq!(a.len(), 1);
        assert!(a[0].mine);
        let b = t.browse("", Some("grpB"), None, 100);
        assert_eq!(b.len(), 1);
        assert!(b[0].mine);
        // A stranger sees nothing (not listable, no group match).
        assert!(t.browse("", Some("grpZ"), None, 100).is_empty());
    }

    #[test]
    fn same_device_reannouncing_with_a_changed_ticket_stays_one_peer() {
        let mut t = Tracker::default();
        let b3 = "ff".repeat(32);
        // One device announces, then re-announces after its addresses changed:
        // same stable NodeId, but a *different* node ticket string.
        t.announce(
            "ticket-v1",
            "node-1",
            Some("Mac A"),
            None,
            &[item(&b3, "Open Model", true)],
        );
        t.announce(
            "ticket-v2",
            "node-1",
            Some("Mac A"),
            None,
            &[item(&b3, "Open Model", true)],
        );
        // De-dup by NodeId: still a single peer, carrying the latest ticket.
        let rows = t.browse("", None, None, 100);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].peers, 1);
        let (_b3, ps) = t.get(&b3, None);
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].node, "ticket-v2");

        // A genuinely different device is a second peer.
        t.announce(
            "ticket-x",
            "node-2",
            Some("Mac B"),
            None,
            &[item(&b3, "Open Model", true)],
        );
        assert_eq!(t.browse("", None, None, 100)[0].peers, 2);
        assert_eq!(t.stats().1, 2);

        // Self-exclusion: a peer querying with its own NodeId never sees itself.
        let (_b3, mine_excluded) = t.get(&b3, Some("node-1"));
        assert!(mine_excluded.iter().all(|p| p.node_id != "node-1"));
        assert_eq!(mine_excluded.len(), 1);
    }

    #[test]
    fn browse_excludes_self_from_peer_count_and_flags_mine() {
        let mut t = Tracker::default();
        let b3 = "12".repeat(32);
        // You are the only seeder of a public model.
        t.announce(
            "ticket-a",
            "node-self",
            Some("My Mac"),
            None,
            &[item(&b3, "Solo Model", true)],
        );

        // Anonymous browse (no self): you look like 1 peer, not "mine".
        let anon = t.browse("", None, None, 100);
        assert_eq!(anon.len(), 1);
        assert_eq!(anon[0].peers, 1);
        assert!(!anon[0].mine);

        // Browsing as yourself: 0 *other* peers, and flagged mine — so the app
        // shows "on your devices", never "1 peer seeding your file".
        let me = t.browse("", None, Some("node-self"), 100);
        assert_eq!(me.len(), 1);
        assert_eq!(me[0].peers, 0);
        assert!(me[0].mine);

        // A second, genuinely different device joins as a real peer.
        t.announce(
            "ticket-b",
            "node-other",
            Some("Their Mac"),
            None,
            &[item(&b3, "Solo Model", true)],
        );
        let me2 = t.browse("", None, Some("node-self"), 100);
        assert_eq!(me2[0].peers, 1); // the other device only — still excludes you
        assert!(me2[0].mine);
        let them = t.browse("", None, Some("node-other"), 100);
        assert_eq!(them[0].peers, 1); // you only — excludes them
        assert!(them[0].mine);
    }

    #[test]
    fn withdraw_removes_only_the_callers_records() {
        let mut t = Tracker::default();
        // Letters in the blake3 so its fake sha256 alias is genuinely distinct.
        let b3 = "ab".repeat(32);
        t.announce(
            "ta",
            "node-a",
            Some("Mac A"),
            None,
            &[item(&b3, "Shared", true)],
        );
        t.announce(
            "tb",
            "node-b",
            Some("Mac B"),
            None,
            &[item(&b3, "Shared", true)],
        );
        assert_eq!(t.browse("", None, None, 100)[0].peers, 2);

        // A withdraws this file: it drops to one peer, the row survives (B has it).
        t.withdraw("node-a", &[b3.clone()]);
        let after = t.browse("", None, None, 100);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].peers, 1);
        // A is gone from the provider list; B can still be fetched.
        let (_b3, ps) = t.get(&b3, None);
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].node_id, "node-b");

        // B withdraws everything: the file disappears from the catalog entirely,
        // and its sha256 alias is cleaned up too.
        t.withdraw("node-b", &[]);
        assert!(t.browse("", None, None, 100).is_empty());
        assert_eq!(t.stats(), (0, 0));
        let (resolved, ps) = t.get(&fake_sha(&b3), None);
        // Alias gone, so the sha256 no longer resolves to the blake3.
        assert_ne!(resolved, b3);
        assert!(ps.is_empty());
    }

    #[test]
    fn unsigned_or_partial_proof_is_rejected() {
        let node_id = "aa".repeat(32);
        let items = vec!["bb".repeat(32)];
        // No ts/sig at all: rejected (the legacy unsigned fallback is gone).
        assert!(!verify_announce_auth(
            "announce",
            &node_id,
            None,
            None,
            "ticket",
            noema_core::DEFAULT_TRACKER,
            &items,
        ));
        // ts present but sig missing (and vice versa): still rejected.
        assert!(!verify_announce_auth(
            "announce",
            &node_id,
            Some(now_unix_millis()),
            None,
            "ticket",
            noema_core::DEFAULT_TRACKER,
            &items,
        ));
        assert!(!verify_announce_auth(
            "announce",
            &node_id,
            None,
            Some("AAAA"),
            "ticket",
            noema_core::DEFAULT_TRACKER,
            &items,
        ));
        // A garbage signature over an otherwise well-formed request is rejected.
        assert!(!verify_announce_auth(
            "announce",
            &node_id,
            Some(now_unix_millis()),
            Some("AAAA"),
            "ticket",
            noema_core::DEFAULT_TRACKER,
            &items,
        ));
    }

    #[test]
    fn transition_mode_accepts_unsigned_but_still_verifies_a_present_proof() {
        let node_id = "aa".repeat(32);
        let items = vec!["bb".repeat(32)];
        let aud = noema_core::DEFAULT_TRACKER;
        // Transition (require_signed = false): an unsigned legacy announce is accepted.
        assert!(announce_authorized(
            false, "announce", &node_id, None, None, "t", aud, &items
        ));
        // ...but a present-but-garbage signature is still rejected even in transition.
        assert!(!announce_authorized(
            false,
            "announce",
            &node_id,
            Some(now_unix_millis()),
            Some("AAAA"),
            "t",
            aud,
            &items
        ));
        // Strict (require_signed = true): the unsigned announce is rejected.
        assert!(!announce_authorized(
            true, "announce", &node_id, None, None, "t", aud, &items
        ));
    }
}
