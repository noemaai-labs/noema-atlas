use crate::settings::StudioSettings;
use crate::AppState;
use noema_core::engine::{DownloadProgress, Progress};
use serde::Serialize;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

/// Basic app/engine info for the header and footer.
#[derive(Serialize)]
pub struct AppInfo {
    name: String,
    version: String,
    root: String,
}

#[tauri::command]
pub fn app_info(state: State<'_, AppState>) -> AppInfo {
    AppInfo {
        name: "Noema Atlas Studio".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        root: state.root.display().to_string(),
    }
}

/// One Hugging Face search result.
#[derive(Serialize)]
pub struct ModelHit {
    id: String,
    name: String,
    author: String,
    downloads: u64,
    likes: u64,
    gated: bool,
    /// Weight formats this repo publishes (`gguf`, `safetensors`, `mlx`, …), shown
    /// as per-format chips on the card. Canonical ids — pretty-printed in the UI.
    formats: Vec<String>,
    license: Option<String>,
    tags: Vec<String>,
    last_modified: Option<String>,
}

fn to_hit(m: noema_core::hf::HfModel) -> ModelHit {
    ModelHit {
        name: m.name().to_string(),
        author: m.author().to_string(),
        license: m.license(),
        formats: m.model_formats().into_iter().map(String::from).collect(),
        tags: m.display_tags(),
        id: m.id,
        downloads: m.downloads,
        likes: m.likes,
        gated: m.gated,
        last_modified: m.last_modified,
    }
}

#[tauri::command]
pub async fn search_models(
    state: State<'_, AppState>,
    query: String,
) -> Result<Vec<ModelHit>, String> {
    let models = state
        .engine
        .hf_search(&query, 25)
        .await
        .map_err(|e| e.to_string())?;
    Ok(models.into_iter().map(to_hit).collect())
}

/// The most-downloaded models on the Hub — Discover's default "home" listing.
#[tauri::command]
pub async fn popular_models(state: State<'_, AppState>) -> Result<Vec<ModelHit>, String> {
    let models = state
        .engine
        .hf_popular(30)
        .await
        .map_err(|e| e.to_string())?;
    Ok(models.into_iter().map(to_hit).collect())
}

/// A downloadable choice within a model: a single GGUF quant, or the whole
/// safetensors/MLX bundle.
#[derive(Serialize)]
pub struct Variant {
    label: String,
    /// The repo file to fetch for a GGUF quant; `None` for a bundle.
    file: Option<String>,
    size: u64,
    format: String,
    is_bundle: bool,
    /// Number of files in this quant; >1 means a split (sharded) GGUF.
    shards: u32,
    /// The hardware-aware recommended pick for this machine.
    recommended: bool,
    /// Short rationale shown next to the recommended pick (e.g. "fits your ~24 GB").
    fit_reason: String,
    /// The file's sha256 content id, when known (GGUF quants).
    content_id: String,
}

/// Pick the recommended quant by total size against the detected memory budget.
/// Returns the index into `sizes` plus a short human rationale. Operates on
/// quant totals, so a split GGUF is judged by its whole size, not one shard.
fn recommend_quant_index(sizes: &[u64], budget: u64) -> Option<(usize, String)> {
    if sizes.is_empty() {
        return None;
    }
    if budget == 0 {
        // No reliable budget; suggest a middle-of-the-road size.
        let mut idx: Vec<usize> = (0..sizes.len()).collect();
        idx.sort_by_key(|&i| sizes[i]);
        return Some((
            idx[idx.len() / 2],
            "a balanced default for this machine".to_string(),
        ));
    }
    let cap = (budget as f64 / 1.2) as u64; // 1.2x runtime headroom
    let mut best: Option<usize> = None;
    for (i, &s) in sizes.iter().enumerate() {
        if s <= cap && best.map(|b| sizes[b] < s).unwrap_or(true) {
            best = Some(i);
        }
    }
    let gb = budget / 1_000_000_000;
    match best {
        Some(i) => Some((i, format!("fits your ~{gb} GB"))),
        None => {
            let mut sm = 0;
            for i in 1..sizes.len() {
                if sizes[i] < sizes[sm] {
                    sm = i;
                }
            }
            Some((sm, "smallest available, may be tight".to_string()))
        }
    }
}

/// The GGUF quant to fetch when the user didn't choose one: the hardware-aware
/// recommendation (over quant totals), else the first. Returns all its shards.
fn pick_default_quant(detail: &noema_core::hf::HfModelDetail) -> Option<noema_core::hf::GgufQuant> {
    let quants = detail.gguf_quants();
    if quants.is_empty() {
        return None;
    }
    let budget = noema_core::platform::detect_memory_budget_bytes().unwrap_or(0);
    let sizes: Vec<u64> = quants.iter().map(|q| q.total_size()).collect();
    let idx = recommend_quant_index(&sizes, budget)
        .map(|(i, _)| i)
        .unwrap_or(0);
    quants.into_iter().nth(idx)
}

#[derive(Serialize)]
pub struct ModelDetail {
    id: String,
    revision: String,
    gated: bool,
    license: Option<String>,
    variants: Vec<Variant>,
    /// Detected memory budget (bytes) used for the recommendation; 0 if unknown.
    budget_bytes: u64,
}

#[tauri::command]
pub async fn model_detail(state: State<'_, AppState>, id: String) -> Result<ModelDetail, String> {
    let d = state
        .engine
        .hf_model_detail(&id)
        .await
        .map_err(|e| e.to_string())?;

    let budget = noema_core::platform::detect_memory_budget_bytes().unwrap_or(0);
    // Group GGUF files into one entry per quant, folding split shards together.
    let quants = d.gguf_quants();
    let sizes: Vec<u64> = quants.iter().map(|q| q.total_size()).collect();
    let rec = recommend_quant_index(&sizes, budget);

    let mut variants = Vec::new();
    for (i, q) in quants.iter().enumerate() {
        let recommended = rec.as_ref().map(|(ri, _)| *ri == i).unwrap_or(false);
        variants.push(Variant {
            label: q.label.clone(),
            file: Some(q.files[0].rfilename.clone()),
            size: q.total_size(),
            format: "gguf".into(),
            is_bundle: false,
            shards: q.files.len() as u32,
            recommended,
            fit_reason: if recommended {
                rec.as_ref().map(|(_, r)| r.clone()).unwrap_or_default()
            } else {
                String::new()
            },
            // A single-file quant exposes its content id; a split quant spans
            // several files, so there is no single id to copy.
            content_id: if q.files.len() == 1 {
                q.files[0].sha256.clone().unwrap_or_default()
            } else {
                String::new()
            },
        });
    }
    if d.has_safetensors_bundle() {
        variants.push(Variant {
            label: d.bundle_variant_label(),
            file: None,
            size: d.bundle_total_size(),
            format: d.bundle_format().to_string(),
            is_bundle: true,
            shards: 0,
            recommended: false,
            fit_reason: String::new(),
            content_id: String::new(),
        });
    }

    Ok(ModelDetail {
        id: d.id.clone(),
        revision: d.revision.clone(),
        gated: d.gated,
        license: d.license.clone(),
        variants,
        budget_bytes: budget,
    })
}

/// Progress event payload pushed to the webview during a download. `transfer_id`
/// keys the per-row card in the front-end's `transfers` map (it equals the
/// manifest id, the engine's stable transfer key).
#[derive(Serialize, Clone)]
pub struct ProgressEvent {
    transfer_id: String,
    manifest_id: String,
    artifact: String,
    source: Option<String>,
    bytes_done: u64,
    bytes_total: u64,
    phase: String,
    /// Why the downloader left a source (emitted at source boundaries).
    failover_reason: Option<String>,
    /// Verified offset a resumed source attempt started from.
    effective_start: Option<u64>,
    /// Connected swarm peers (BitTorrent; `0` for byte-only sources).
    peers: u32,
    /// Cumulative bytes uploaded to peers this transfer (BitTorrent seeding) —
    /// lets the UI show a seed ratio. `0` for non-swarm transports.
    uploaded_bytes: u64,
}

/// Build the `download://progress` emitter handed to the engine's downloader.
fn progress_emitter(app: AppHandle) -> Progress {
    Arc::new(move |p: DownloadProgress| {
        let _ = app.emit(
            "download://progress",
            ProgressEvent {
                transfer_id: p.manifest_id.clone(),
                manifest_id: p.manifest_id,
                artifact: p.artifact_path,
                source: p.source_id,
                bytes_done: p.bytes_done,
                bytes_total: p.bytes_total,
                phase: p.phase.to_string(),
                failover_reason: p.failover_reason,
                effective_start: p.effective_start,
                peers: p.peers,
                uploaded_bytes: p.uploaded_bytes,
            },
        );
    })
}

#[derive(Serialize)]
pub struct DownloadResult {
    manifest_id: String,
    artifacts: usize,
}

/// Classify a download outcome for the UI: a user Pause keeps the partial and is
/// resumable, a Stop discards it, no eligible source (typically a peer-only model
/// with nobody online) is a resumable "waiting for peers", anything else is a
/// genuine failure.
fn download_end_status(e: &noema_core::Error) -> &'static str {
    match e {
        noema_core::Error::Cancelled => "paused",
        noema_core::Error::Stopped => "stopped",
        noema_core::Error::NoEligibleSource(_) => "waiting",
        _ => "error",
    }
}

/// A double-press of Resume hits the engine's already-running guard, which is not
/// a real failure — the first run is still live and will emit its own `done`. Detect
/// it so the command can stay silent instead of flipping the live card to "error".
fn is_already_running(e: &noema_core::Error) -> bool {
    matches!(e, noema_core::Error::Other(msg) if msg.contains("already running"))
}

/// The engine's deterministic manifest id for a content / link download
/// (`mdl_p2p_<seed[..12]>`, seed = sha256 if 64 chars else blake3). Mirrors
/// `content_manifest` in the engine so the front-end can re-key its provisional
/// row by the exact id before the download starts, without a FIFO adopt.
fn content_manifest_id(sha256: &str, blake3: &str) -> String {
    let seed = if sha256.len() == 64 { sha256 } else { blake3 };
    // Slice on char boundaries: an untrusted link's id may be non-ASCII, and
    // byte-indexing would panic mid-codepoint.
    format!("mdl_p2p_{}", seed.chars().take(12).collect::<String>())
}

/// Import the chosen variant's manifest, then download it, streaming verified
/// bytes into the cache. Emits `download://progress` per chunk-boundary and
/// `download://done` at the end.
/// Tell the front-end the engine's real transfer id for a download it kicked off
/// with a provisional (`tmp_…`) key, so it can re-key *its own* row deterministically
/// instead of guessing via FIFO. Emitted right after the manifest is registered,
/// before the (long) download await.
#[derive(Serialize, Clone)]
struct RegisteredEvent {
    client_ref: String,
    transfer_id: String,
}

fn emit_registered(app: &AppHandle, client_ref: &Option<String>, transfer_id: &str) {
    if let Some(client_ref) = client_ref {
        if !client_ref.is_empty() {
            let _ = app.emit(
                "download://registered",
                RegisteredEvent {
                    client_ref: client_ref.clone(),
                    transfer_id: transfer_id.to_string(),
                },
            );
        }
    }
}

/// Emit the terminal `download://done` for one transfer id, settling its card.
/// `status` is one of done/paused/stopped/waiting/error; `message` carries the
/// error text when relevant.
fn emit_download_done(app: &AppHandle, mid: &str, status: &str, message: Option<&str>) {
    let mut payload = serde_json::json!({
        "transfer_id": mid,
        "manifest_id": mid,
        "status": status,
    });
    if let Some(msg) = message {
        payload["message"] = serde_json::Value::String(msg.to_string());
    }
    let _ = app.emit("download://done", payload);
}

#[tauri::command]
pub async fn download_model(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    file: Option<String>,
    bundle: Option<bool>,
    client_ref: Option<String>,
) -> Result<DownloadResult, String> {
    let engine = state.engine.clone();
    let detail = engine
        .hf_model_detail(&id)
        .await
        .map_err(|e| e.to_string())?;

    // Resolve the import: the explicitly requested safetensors/MLX bundle (its
    // variant has no `file`, so without this flag a mixed repo would fall through
    // to a GGUF quant the user didn't pick), else an explicit GGUF file, else the
    // first GGUF, else the bundle.
    let quants = detail.gguf_quants();
    let import = if bundle.unwrap_or(false) {
        if !detail.has_safetensors_bundle() {
            return Err("this repo has no safetensors/MLX bundle to download".into());
        }
        engine
            .hf_import_bundle(&detail)
            .map_err(|e| e.to_string())?
    } else if let Some(fname) = file.as_deref() {
        // Find the quant that owns the chosen file and fetch all of its shards
        // as one model (a split GGUF downloads every shard, not just one).
        if let Some(q) = quants
            .iter()
            .find(|q| q.files.iter().any(|f| f.rfilename == fname))
        {
            engine
                .hf_import_gguf_quant(&detail, &q.files)
                .map_err(|e| e.to_string())?
        } else {
            return Err(format!("file not found in repo: {fname}"));
        }
    } else if let Some(q) = pick_default_quant(&detail) {
        engine
            .hf_import_gguf_quant(&detail, &q.files)
            .map_err(|e| e.to_string())?
    } else if detail.has_safetensors_bundle() {
        engine
            .hf_import_bundle(&detail)
            .map_err(|e| e.to_string())?
    } else {
        return Err("no downloadable weights found in this model".into());
    };

    let manifest_id = import.manifest_id.clone();

    // Register up front so the front-end can re-key its provisional row by this
    // exact id before any progress arrives (no FIFO adopt).
    let transfer = engine.register_transfer(&manifest_id);
    emit_registered(&app, &client_ref, &manifest_id);

    let progress = progress_emitter(app.clone());

    match engine.run_download(&transfer, Some(progress)).await {
        Ok(out) => {
            let _ = app.emit(
                "download://done",
                serde_json::json!({ "transfer_id": out.manifest_id, "manifest_id": out.manifest_id, "status": "done" }),
            );
            Ok(DownloadResult {
                manifest_id: out.manifest_id,
                artifacts: out.artifacts.len(),
            })
        }
        Err(e) => {
            // Pause / Stop are not failures: report a status the UI can act on
            // (keep the card and offer resume, or clear it) instead of an error.
            let _ = app.emit(
                "download://done",
                serde_json::json!({
                    "transfer_id": manifest_id,
                    "manifest_id": manifest_id,
                    "status": download_end_status(&e),
                    "message": e.to_string(),
                }),
            );
            Ok(DownloadResult {
                manifest_id,
                artifacts: 0,
            })
        }
    }
}

/// Resume a paused (or peer-stalled) download straight from the Transfers page.
/// The engine picks up from the kept `.part` and re-plans sources, so it tries
/// whatever peers or fallbacks are reachable now. Best-effort: if no source is
/// available it reports "waiting" and can be resumed again later.
#[tauri::command]
pub async fn resume_download(
    app: AppHandle,
    state: State<'_, AppState>,
    manifest_id: String,
) -> Result<DownloadResult, String> {
    let progress = progress_emitter(app.clone());
    match state.engine.download(&manifest_id, Some(progress)).await {
        Ok(out) => {
            let _ = app.emit(
                "download://done",
                serde_json::json!({ "transfer_id": out.manifest_id, "manifest_id": out.manifest_id, "status": "done" }),
            );
            Ok(DownloadResult {
                manifest_id: out.manifest_id,
                artifacts: out.artifacts.len(),
            })
        }
        Err(e) if is_already_running(&e) => {
            // A double-press of Resume: the first run is still live and owns the
            // card. Stay silent — emitting a `done` here would flip the live row to
            // an error. The in-flight transfer emits its own terminal event.
            Ok(DownloadResult {
                manifest_id,
                artifacts: 0,
            })
        }
        Err(e) => {
            let _ = app.emit(
                "download://done",
                serde_json::json!({
                    "transfer_id": manifest_id,
                    "manifest_id": manifest_id,
                    "status": download_end_status(&e),
                    "message": e.to_string(),
                }),
            );
            Ok(DownloadResult {
                manifest_id,
                artifacts: 0,
            })
        }
    }
}

/// One row of the worldwide mesh (the tracker's content catalog).
#[derive(Serialize)]
pub struct MeshItem {
    name: String,
    quant: String,
    size: u64,
    license: String,
    sha256: String,
    blake3: String,
    /// Distinct peers seeding this over Iroh (excludes you).
    peers: usize,
    /// Distinct peers seeding this over BitTorrent (excludes you).
    bt_seeders: usize,
    /// BitTorrent magnet advertised by a seeding peer (empty when none); fed back
    /// into `add_from_mesh` so the receiver can join the swarm.
    magnet: String,
    in_library: bool,
    mine: bool,
    devices: Vec<String>,
}

/// Browse models peers are sharing worldwide (queries the tracker over HTTP).
#[tauri::command]
pub async fn mesh_search(
    state: State<'_, AppState>,
    query: String,
) -> Result<Vec<MeshItem>, String> {
    let rows = state
        .engine
        .network_catalog(&query)
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|m| MeshItem {
            name: m.name,
            quant: m.quant,
            size: m.size,
            license: m.license,
            sha256: m.sha256,
            blake3: m.blake3,
            peers: m.peers,
            bt_seeders: m.bt_seeders,
            magnet: m.magnet,
            in_library: m.in_library,
            mine: m.mine,
            devices: m.devices,
        })
        .collect())
}

/// Fetch a model from a pasted share link (`atlas1:` single / `atlasb1:` bundle),
/// verifying every byte. Emits the same progress events as a Hub download.
#[tauri::command]
pub async fn add_by_link(
    app: AppHandle,
    state: State<'_, AppState>,
    link: String,
    client_ref: Option<String>,
) -> Result<usize, String> {
    let link = link.trim().to_string();
    if link.is_empty() {
        return Err("paste a share link (atlas1:… or atlasb1:…)".into());
    }
    let engine = state.engine.clone();

    if noema_core::is_bundle_link(&link) {
        // A bundle is N independent files, each with its own deterministic
        // manifest id. The engine downloads them one by one (each emitting progress
        // under its own id), so the UI sees N cards. Drive them per-file here: the
        // FIRST file re-keys the provisional row the UI created (via client_ref),
        // and every file gets its own `registered` + terminal `done`. Without the
        // per-file done, files 2..N spawned orphan cards (created on first progress)
        // that never settled and lingered forever.
        let bundle = noema_core::ShareBundle::decode(&link).map_err(|e| e.to_string())?;
        let files: Vec<_> = bundle
            .files
            .into_iter()
            .filter(|f| f.has_content_id())
            .collect();
        if files.is_empty() {
            return Err("bundle had no fetchable files".into());
        }
        let mut done = 0usize;
        for (i, file) in files.into_iter().enumerate() {
            let mid = content_manifest_id(&file.sha256, &file.blake3);
            // Register up front so Pause / Stop work before the first byte (see
            // `download_model`); `add_by_content` reuses this id.
            engine.register_transfer(&mid);
            // Re-key the one provisional row to the first file; later files have no
            // provisional row and surface as their own cards on first progress.
            let cref = if i == 0 { client_ref.clone() } else { None };
            emit_registered(&app, &cref, &mid);
            let progress = progress_emitter(app.clone());
            match engine.add_by_content(file, Some(progress)).await {
                Ok(out) => {
                    emit_download_done(&app, &mid, "done", None);
                    done += out.artifacts.len();
                }
                Err(e) => {
                    // Settle this file's card so it can't linger; keep going so a
                    // single bad shard doesn't strand the rest as orphan cards.
                    emit_download_done(&app, &mid, download_end_status(&e), Some(&e.to_string()));
                }
            }
        }
        return Ok(done);
    }

    let target = noema_core::ShareTarget::decode(&link).map_err(|e| e.to_string())?;
    let mid = content_manifest_id(&target.sha256, &target.blake3);
    // Register up front so Pause / Stop have a control before the first byte.
    engine.register_transfer(&mid);
    emit_registered(&app, &client_ref, &mid);
    let progress = progress_emitter(app.clone());
    match engine.add_by_content(target, Some(progress)).await {
        Ok(out) => {
            emit_download_done(&app, &mid, "done", None);
            Ok(out.artifacts.len())
        }
        Err(e) => {
            emit_download_done(&app, &mid, download_end_status(&e), Some(&e.to_string()));
            Ok(0)
        }
    }
}

/// A model held in the local Library (cached and/or installed).
#[derive(Serialize)]
pub struct LibItem {
    manifest_id: String,
    name: String,
    size_bytes: u64,
    blake3: String,
    sha256: String,
    family: Option<String>,
    quant: Option<String>,
    license: String,
    from_hf: bool,
    signed: bool,
    shareable: bool,
    gated: bool,
    install_path: Option<String>,
    /// Whether this model is being seeded over BitTorrent in the live session — so
    /// the Library can show a truthful "sharing" state even when only BT is active.
    bt_seeding: bool,
    /// Recognized weight format tag (`gguf`, `safetensors`, …); surfaced as a badge.
    format: Option<String>,
}

#[tauri::command]
pub fn list_library(state: State<'_, AppState>) -> Result<Vec<LibItem>, String> {
    let models = state.engine.installed_models().map_err(|e| e.to_string())?;
    Ok(models
        .into_iter()
        .map(|m| LibItem {
            bt_seeding: state.engine.is_bt_seeding(&m.blake3),
            manifest_id: m.manifest_id,
            name: m.name,
            size_bytes: m.size_bytes,
            blake3: m.blake3,
            sha256: m.sha256,
            family: m.family,
            quant: m.quant,
            license: m.license,
            from_hf: m.from_hf,
            signed: m.signed,
            shareable: m.shareable,
            gated: m.gated,
            install_path: m.install_path,
            format: m.format,
        })
        .collect())
}

#[tauri::command]
pub fn list_cache(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let list = state.engine.list_cache().map_err(|e| e.to_string())?;
    Ok(serde_json::json!(list
        .into_iter()
        .map(|b| serde_json::json!({
            "blake3": b.blake3,
            "size_bytes": b.size_bytes,
            "state": b.state,
            "committed_at": b.committed_at,
        }))
        .collect::<Vec<_>>()))
}

#[tauri::command]
pub fn source_health(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let list = state
        .engine
        .report_source_health()
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!(list
        .into_iter()
        .map(|h| serde_json::json!({
            "source_id": h.source_id,
            "success": h.success_count,
            "failure": h.failure_count,
            "integrity_failures": h.integrity_failures,
            "banned": h.banned,
            "last_latency_ms": h.last_latency_ms,
        }))
        .collect::<Vec<_>>()))
}

/// Re-announce the now-shareable catalog to the tracker (no-op when the seeder
/// isn't running). Shared by `set_share` and `confirm_gated_share`.
async fn announce_shared(state: &AppState) {
    if let Some(w) = state.share.lock().await.as_ref() {
        if let Ok(items) = state.engine.share_announce_items() {
            w.seeder_handle()
                .announce(&items, &StudioSettings::load(&state.root).tracker())
                .await;
        }
    }
}

#[tauri::command]
pub async fn set_share(
    state: State<'_, AppState>,
    blake3: String,
    sha256: String,
    on: bool,
) -> Result<(), String> {
    state
        .engine
        .set_model_shared(&blake3, &sha256, on)
        .map_err(|e| e.to_string())?;
    if on {
        announce_shared(&state).await;
    } else {
        // Stop seeding over BitTorrent (engine-owned) as well as Iroh (UI-held).
        let _ = state.engine.unseed_bittorrent(&blake3).await;
        if let Some(w) = state.share.lock().await.as_ref() {
            w.unseed_and_disconnect(&blake3).await;
        }
        state.engine.withdraw_from_tracker(&[blake3.clone()]).await;
    }
    Ok(())
}

/// The distinct transport paths a transfer's manifest can use ("iroh" / "bt" /
/// "https" / "hf"), so the Transfers card can show the standby routes alongside
/// the active one.
#[tauri::command]
pub fn transfer_routes(
    state: State<'_, AppState>,
    manifest_id: String,
) -> Result<Vec<String>, String> {
    let Some(manifest) = state
        .engine
        .get_manifest(&manifest_id)
        .map_err(|e| e.to_string())?
    else {
        return Ok(Vec::new());
    };
    let mut routes: Vec<String> = Vec::new();
    for art in &manifest.artifacts {
        for s in &art.sources {
            use noema_core::manifest::SourceClass as C;
            let tag = match s.class() {
                C::Iroh => "iroh",
                C::BittorrentV2 => "bt",
                C::HttpsMirror => "https",
                C::Huggingface => "hf",
                _ => continue,
            };
            if !routes.iter().any(|r| r == tag) {
                routes.push(tag.to_string());
            }
        }
    }
    let order = |r: &str| match r {
        "iroh" => 0,
        "bt" => 1,
        "https" => 2,
        _ => 3,
    };
    routes.sort_by_key(|r| order(r));
    Ok(routes)
}

/// Peers actively pulling this blob from us right now (live Iroh uploads), so
/// the Library can warn before a share-off hard-disconnects them mid-download.
#[tauri::command]
pub async fn share_activity(state: State<'_, AppState>, blake3: String) -> Result<u32, String> {
    Ok(state
        .share
        .lock()
        .await
        .as_ref()
        .map(|w| w.active_uploads_for(&blake3) as u32)
        .unwrap_or(0))
}

/// Whether turning sharing on for this model needs an explicit confirmation —
/// true for gated / restrictively-licensed content the user hasn't confirmed
/// before. The Library shows a modal and, on accept, calls `confirm_gated_share`.
#[tauri::command]
pub fn share_needs_confirmation(
    state: State<'_, AppState>,
    manifest_id: String,
) -> Result<bool, String> {
    let manifest = state
        .engine
        .get_manifest(&manifest_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "model not found in library".to_string())?;
    Ok(state.engine.needs_share_confirmation(&manifest))
}

/// Record the explicit confirm-before-share for a gated / restrictive model and
/// announce it. Called after the user accepts the confirmation modal.
#[tauri::command]
pub async fn confirm_gated_share(
    state: State<'_, AppState>,
    blake3: String,
    sha256: String,
) -> Result<(), String> {
    state
        .engine
        .confirm_gated_share(&blake3, &sha256)
        .map_err(|e| e.to_string())?;
    announce_shared(&state).await;
    Ok(())
}

#[derive(Serialize)]
pub struct InstalledView {
    artifact: String,
    dest: String,
    link: String,
}

#[tauri::command]
pub fn install_model(
    state: State<'_, AppState>,
    manifest_id: String,
    target: String,
) -> Result<Vec<InstalledView>, String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("an install directory is required".into());
    }
    let views = state
        .engine
        .materialize_install(&manifest_id, std::path::Path::new(target))
        .map_err(|e| e.to_string())?;
    Ok(views
        .into_iter()
        .map(|v| InstalledView {
            artifact: v.artifact_path,
            dest: v.dest.to_string_lossy().to_string(),
            link: v.link_kind.as_str().to_string(),
        })
        .collect())
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> StudioSettings {
    StudioSettings::load(&state.root)
}

/// Persist settings and live-apply the toggles the engine can change without a
/// restart. Proxy / mirror / tracker live in `EngineConfig` and take effect on
/// next launch — the front-end says as much.
#[tauri::command]
pub async fn save_settings(
    state: State<'_, AppState>,
    settings: StudioSettings,
) -> Result<(), String> {
    settings.save(&state.root).map_err(|e| e.to_string())?;
    let engine = &state.engine;
    engine.set_max_download_connections(settings.download_connections.max(1) as usize);
    engine.set_hf_download_enabled(settings.allow_hf_download);
    engine.rate_limit().set_bps(settings.cap_bps());
    engine.set_bittorrent_max_ratio(settings.bt_max_ratio.max(0.0));
    engine.set_bittorrent_sequential(settings.bt_sequential);
    // Time-of-day bandwidth schedule (alternative speed limits). Applied live; the
    // schedule's "normal" caps are the manual caps above so disabling it restores them.
    engine.set_bandwidth_schedule(settings.bandwidth_schedule());
    // Download-routing preference is runtime-adjustable (no restart) — apply it live
    // so the next download's plan honors the new bias immediately.
    engine.set_download_preference(noema_core::DownloadPreference::from_u8(
        settings.download_preference,
    ));

    // Turning the gated-share toggle OFF must promptly stop sharing every confirmed
    // gated model — clear the confirmations and sever its blobs (Iroh + BitTorrent)
    // — instead of leaving them seeding until the slow background reconcile.
    // `revoke_gated_shares` clears the overrides and tears down BT seeding itself,
    // returning each cleared `(blake3, sha256)` so we Iroh-unseed + withdraw here.
    // Best-effort.
    let gated_was_on = engine.share_gated_enabled();
    if gated_was_on && !settings.share_gated {
        engine.set_share_gated_enabled(false);
        let cleared = engine.revoke_gated_shares().await.unwrap_or_default();
        let blake3s: Vec<String> = cleared.into_iter().map(|(b3, _)| b3).collect();
        if let Some(w) = state.share.lock().await.as_ref() {
            for b3 in &blake3s {
                w.unseed_and_disconnect(b3).await;
            }
        }
        if !blake3s.is_empty() {
            engine.withdraw_from_tracker(&blake3s).await;
        }
    } else {
        engine.set_share_gated_enabled(settings.share_gated);
    }
    Ok(())
}

/// Start the worldwide seeder (idempotent), storing the handle in managed state.
/// Shared by the `start_worldwide` command and the launch `setup` hook.
pub async fn start_worldwide_inner(
    engine: &Arc<noema_core::Engine>,
    root: &std::path::Path,
    share: &tokio::sync::Mutex<Option<noema_core::engine::WorldwideShare>>,
) -> anyhow::Result<String> {
    let mut guard = share.lock().await;
    if let Some(w) = guard.as_ref() {
        return Ok(w.node_ticket().to_string());
    }
    let s = StudioSettings::load(root);
    let w = engine
        .start_worldwide_share(s.tracker(), s.identity())
        .await?;
    let ticket = w.node_ticket().to_string();
    *guard = Some(w);
    Ok(ticket)
}

#[tauri::command]
pub async fn start_worldwide(state: State<'_, AppState>) -> Result<String, String> {
    start_worldwide_inner(&state.engine, &state.root, &state.share)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn stop_worldwide(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(w) = state.share.lock().await.take() {
        w.stop().await;
    }
    state.engine.withdraw_from_tracker(&[]).await;
    Ok(())
}

#[tauri::command]
pub async fn worldwide_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let guard = state.share.lock().await;
    Ok(serde_json::json!({
        "sharing": guard.is_some(),
        "ticket": guard.as_ref().map(|w| w.node_ticket().to_string()),
        "active_uploads": guard.as_ref().map(|w| w.active_uploads()).unwrap_or(0),
    }))
}

/// Per-model upload activity: every shared model in the library together with how
/// many peers are pulling it right now (Iroh) and whether it's being seeded over
/// BitTorrent. Drives the live "sharing" view. BitTorrent seeding is engine-owned
/// and runs even when the Iroh worldwide session isn't up, so a model can be
/// truthfully "sharing" over BT alone.
#[tauri::command]
pub async fn uploads_list(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let iroh_live = state.share.lock().await.is_some();
    let models = state.engine.installed_models().map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    let mut total: u64 = 0;
    let mut bt_seeding = false;
    for m in models.into_iter().filter(|m| m.shareable) {
        // Iroh peers actively pulling this blob (0 when the worldwide session is off).
        let uploads = {
            let guard = state.share.lock().await;
            guard
                .as_ref()
                .map(|w| w.active_uploads_for(&m.blake3))
                .unwrap_or(0)
        };
        let seeding_bt = state.engine.is_bt_seeding(&m.blake3);
        bt_seeding |= seeding_bt;
        total += uploads;
        rows.push(serde_json::json!({
            "name": m.name,
            "blake3": m.blake3,
            "uploads": uploads,
            "bt_seeding": seeding_bt,
            // Whether the live Iroh worldwide session is seeding this blob — so the
            // sharing view can show a "seeding · Iroh" pill symmetric with BT.
            "iroh_seeding": iroh_live && state.engine.is_iroh_seeding(&m.blake3),
        }));
    }
    // "sharing" is true if either route is live: the Iroh worldwide session is up,
    // or at least one blob is seeded over BitTorrent.
    Ok(serde_json::json!({
        "sharing": iroh_live || bt_seeding,
        "iroh": iroh_live,
        "bt_seeding": bt_seeding,
        "total": total,
        "models": rows,
    }))
}

#[tauri::command]
pub async fn apply_identity(state: State<'_, AppState>, device_name: String) -> Result<(), String> {
    let mut s = StudioSettings::load(&state.root);
    s.device_name = device_name.trim().to_string();
    s.save(&state.root).map_err(|e| e.to_string())?;
    if let Some(w) = state.share.lock().await.as_ref() {
        w.set_identity(s.identity());
    }
    Ok(())
}

#[tauri::command]
pub fn set_token(state: State<'_, AppState>, token: String) -> Result<(), String> {
    state
        .engine
        .set_token("huggingface", token.trim())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn clear_token(state: State<'_, AppState>) -> Result<(), String> {
    state
        .engine
        .delete_token("huggingface")
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn token_status(state: State<'_, AppState>) -> Result<bool, String> {
    let src = noema_core::Source::Huggingface {
        repo_id: String::new(),
        revision: String::new(),
        path: String::new(),
        auth: noema_core::manifest::AuthPolicy::Token,
    };
    state.engine.token_status(&src).map_err(|e| e.to_string())
}

/// Pause one transfer by id (keeps its partial for resume). With no id, pauses
/// every active transfer (back-compat).
#[tauri::command]
pub fn pause_download(state: State<'_, AppState>, transfer_id: Option<String>) {
    match transfer_id {
        Some(id) if !id.is_empty() => {
            state.engine.pause(&noema_core::TransferId(id));
        }
        _ => state.engine.request_pause(),
    }
}

/// Stop one transfer by id (discards its partial). With no id, stops every
/// active transfer (back-compat).
#[tauri::command]
pub fn stop_download(state: State<'_, AppState>, transfer_id: Option<String>) {
    match transfer_id {
        Some(id) if !id.is_empty() => {
            state.engine.stop(&noema_core::TransferId(id));
        }
        _ => state.engine.request_stop(),
    }
}

/// Snapshot of every live transfer the engine is tracking — id + lifecycle state.
/// Lets the front-end reconcile its `transfers` map after a reload or restart.
#[derive(Serialize)]
pub struct TransferRow {
    transfer_id: String,
    state: String,
}

#[tauri::command]
pub fn list_transfers(state: State<'_, AppState>) -> Vec<TransferRow> {
    state
        .engine
        .list_transfers()
        .into_iter()
        .map(|(id, st)| TransferRow {
            transfer_id: id.0,
            state: format!("{st:?}"),
        })
        .collect()
}

/// Forget a finished / stopped transfer, freeing its registry slot. Called when
/// the user dismisses a row from the Transfers list.
#[tauri::command]
pub fn remove_transfer(state: State<'_, AppState>, transfer_id: String) {
    state
        .engine
        .remove_transfer(&noema_core::TransferId(transfer_id));
}

/// Fully discard a paused / waiting transfer: delete its leftover `.part` temp(s)
/// and resumable DB row (the bytes that would otherwise leak), then free the
/// registry slot. Used by the front-end's Remove on a not-done row.
#[tauri::command]
pub fn discard_transfer(state: State<'_, AppState>, transfer_id: String) -> Result<(), String> {
    state
        .engine
        .discard_transfer(&noema_core::TransferId(transfer_id))
        .map_err(|e| e.to_string())
}

/// One interrupted download a restart can re-offer: its manifest id (the transfer
/// key), display artifact, and how far it got. The front-end seeds a Paused card
/// per row on launch so paused downloads reappear after a full app restart.
#[derive(Serialize)]
pub struct ResumableRow {
    transfer_id: String,
    artifact: String,
    bytes_done: u64,
    bytes_total: u64,
}

#[tauri::command]
pub fn resumable_downloads(state: State<'_, AppState>) -> Result<Vec<ResumableRow>, String> {
    Ok(state
        .engine
        .resumable_downloads()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(
            |(manifest_id, artifact, bytes_done, bytes_total)| ResumableRow {
                transfer_id: manifest_id,
                artifact,
                bytes_done,
                bytes_total,
            },
        )
        .collect())
}

#[derive(Serialize)]
pub struct LocalImportView {
    manifest_id: String,
    model_name: String,
    blake3: String,
    sha256: String,
    size_bytes: u64,
    matched: bool,
    shareable: bool,
    share_link: Option<String>,
}

/// `import://progress` payload: the current phase and, for `hashing`, how far
/// the file read has got. Lets the UI show "Hashing… 47%" instead of a dead
/// "Working…" label while a multi-gigabyte import is read.
#[derive(Clone, Serialize)]
struct ImportProgressEvent {
    phase: &'static str,
    bytes_done: u64,
    bytes_total: u64,
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn import_local(
    app: AppHandle,
    state: State<'_, AppState>,
    path: String,
    title: Option<String>,
    family: Option<String>,
    quant: Option<String>,
    architecture: Option<String>,
    license: Option<String>,
    description: Option<String>,
    origin_url: Option<String>,
    skip_hf_match: bool,
    publish: bool,
) -> Result<LocalImportView, String> {
    let meta = noema_core::engine::LocalShareMeta {
        title,
        family,
        quant,
        architecture,
        license,
        description,
        origin_url,
        skip_hf_match,
        publish,
    };

    // Stream hashing progress (the import's long pole) so the modal shows a live
    // phase + percentage instead of an indefinite spinner.
    let app_cb = app.clone();
    let on_hash: noema_core::engine::ImportProgress = Arc::new(move |done, total| {
        let _ = app_cb.emit(
            "import://progress",
            ImportProgressEvent {
                phase: "hashing",
                bytes_done: done,
                bytes_total: total,
            },
        );
    });

    let out = state
        .engine
        .import_local_file_with_meta_progress(std::path::Path::new(&path), meta, Some(on_hash))
        .await
        .map_err(|e| e.to_string())?;

    // The bytes are now durably in the cache — the import has succeeded. Announce
    // to the worldwide mesh in the *background*: a slow or unreachable seeder /
    // tracker must never keep the user staring at "Working…" after the import is
    // already done on disk. `SeederHandle` is cloneable + `Send` for exactly this.
    if out.shareable {
        if let Ok(items) = state.engine.share_announce_items() {
            let handle = state.share.lock().await.as_ref().map(|w| w.seeder_handle());
            if let Some(handle) = handle {
                let tracker = StudioSettings::load(&state.root).tracker();
                tauri::async_runtime::spawn(async move {
                    handle.announce(&items, &tracker).await;
                });
            }
        }
    }

    let share_link = link_for(state.engine.as_ref(), &out.manifest_id);
    Ok(LocalImportView {
        manifest_id: out.manifest_id,
        model_name: out.model_name,
        blake3: out.blake3,
        sha256: out.sha256,
        size_bytes: out.size_bytes,
        matched: out.matched,
        shareable: out.shareable,
        share_link,
    })
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn edit_model(
    state: State<'_, AppState>,
    manifest_id: String,
    title: Option<String>,
    family: Option<String>,
    quant: Option<String>,
    architecture: Option<String>,
    license: Option<String>,
    description: Option<String>,
    origin_url: Option<String>,
    publish: bool,
) -> Result<(), String> {
    let meta = noema_core::engine::LocalShareMeta {
        title,
        family,
        quant,
        architecture,
        license,
        description,
        origin_url,
        skip_hf_match: true,
        publish,
    };
    state
        .engine
        .rename_model(&manifest_id, &meta)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_model(
    state: State<'_, AppState>,
    blake3: String,
    sha256: String,
) -> Result<u64, String> {
    let report = state
        .engine
        .evict_cache(noema_core::EvictPolicy::Blob(blake3.clone()))
        .map_err(|e| e.to_string())?;
    // Stop seeding the deleted blob over BitTorrent (engine-owned) and Iroh
    // (UI-held). evict_cache already detaches BT internally, but call it
    // explicitly so a delete never leaves a dangling seed if eviction was a no-op.
    let _ = state.engine.unseed_bittorrent(&blake3).await;
    if let Some(w) = state.share.lock().await.as_ref() {
        w.unseed_and_disconnect(&blake3).await;
    }
    state.engine.withdraw_from_tracker(&[blake3.clone()]).await;
    let _ = state.engine.set_model_shared(&blake3, &sha256, false);
    Ok(report.freed_bytes)
}

fn target_for(
    engine: &noema_core::Engine,
    m: &noema_core::InstalledModel,
) -> noema_core::ShareTarget {
    noema_core::ShareTarget {
        name: m.name.clone(),
        size: m.size_bytes,
        sha256: m.sha256.clone(),
        blake3: m.blake3.clone(),
        license: m.license.clone(),
        title: m.name.clone(),
        family: m.family.clone().unwrap_or_default(),
        quant: m.quant.clone().unwrap_or_default(),
        desc: m.description.clone().unwrap_or_default(),
        origin: m.origin.clone().unwrap_or_default(),
        // Advertise the BitTorrent swarm in the link when this blob is seeded over
        // BT, so a receiver can join it straight from the link (empty otherwise).
        magnet: engine.bt_magnet(&m.blake3),
    }
}

fn link_for(engine: &noema_core::Engine, manifest_id: &str) -> Option<String> {
    let m = engine
        .installed_models()
        .ok()?
        .into_iter()
        .find(|m| m.manifest_id == manifest_id)?;
    Some(target_for(engine, &m).encode())
}

#[tauri::command]
pub fn copy_share_link(state: State<'_, AppState>, manifest_id: String) -> Result<String, String> {
    link_for(state.engine.as_ref(), &manifest_id)
        .ok_or_else(|| "model not found in library".to_string())
}

/// The BitTorrent magnet for a seeded blob (empty string when it isn't seeded over
/// BT). Returned to the front-end so a Library / Transfers row can copy it to the
/// clipboard. The blob must be seeded over BitTorrent for a magnet to exist.
#[tauri::command]
pub fn bt_magnet(state: State<'_, AppState>, blake3: String) -> String {
    state.engine.bt_magnet(&blake3)
}

/// Whether a blob is being seeded over **Iroh** right now (the live worldwide
/// session is up and serving it). Symmetric with the `bt_seeding` flag on library
/// rows so the UI can show a per-transport "seeding · Iroh" pill.
#[tauri::command]
pub fn is_iroh_seeding(state: State<'_, AppState>, blake3: String) -> bool {
    state.engine.is_iroh_seeding(&blake3)
}

/// One live BitTorrent peer of a transfer, surfaced to the per-transfer peers
/// table in Transfers.
#[derive(Serialize)]
pub struct PeerRow {
    addr: String,
    client: String,
    conn_kind: String,
    state: String,
    downloaded: u64,
    uploaded: u64,
    down_bps: u64,
    up_bps: u64,
}

fn to_peer_rows(peers: Vec<noema_core::transport::BtPeer>) -> Vec<PeerRow> {
    peers
        .into_iter()
        .map(|p| PeerRow {
            addr: p.addr,
            client: p.client,
            conn_kind: p.conn_kind,
            state: p.state,
            downloaded: p.downloaded,
            uploaded: p.uploaded,
            down_bps: p.down_bps,
            up_bps: p.up_bps,
        })
        .collect()
}

/// Live BitTorrent peers for a transfer, by its transfer/manifest id (the key the
/// Transfers list already holds). Resolves the manifest's blob blake3(s) and
/// aggregates each blob's managed-torrent peers (its seed and/or in-flight leech).
/// Empty when BitTorrent is off / not built in, the session isn't up, the manifest
/// is unknown, or the blob isn't a live torrent here. Synchronous.
#[tauri::command]
pub fn bt_peers(state: State<'_, AppState>, transfer_id: String) -> Vec<PeerRow> {
    let manifest = match state.engine.get_manifest(&transfer_id) {
        Ok(Some(m)) => m,
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for art in &manifest.artifacts {
        let b3 = &art.hashes.blake3;
        if b3.is_empty() {
            continue;
        }
        out.extend(to_peer_rows(state.engine.bt_peers(b3)));
    }
    out
}

/// Live BitTorrent peers for a blob by its blake3 directly (used where the caller
/// already has a content id, e.g. a Library row). Same semantics as `bt_peers`.
#[tauri::command]
pub fn bt_peers_for_blob(state: State<'_, AppState>, blake3: String) -> Vec<PeerRow> {
    to_peer_rows(state.engine.bt_peers(&blake3))
}

/// Set the download-routing preference live (no restart). Mirrors the engine's
/// other runtime toggles; persisted separately in `StudioSettings` via
/// `save_settings`.
#[tauri::command]
pub fn set_download_preference(state: State<'_, AppState>, preference: u8) {
    state
        .engine
        .set_download_preference(noema_core::DownloadPreference::from_u8(preference));
}

/// Pause every active transfer (keeps each partial for resume). Header action in
/// Transfers.
#[tauri::command]
pub fn pause_all(state: State<'_, AppState>) {
    state.engine.request_pause();
}

/// The model's per-model stop-at-ratio override. `None` follows the global cap;
/// `Some(0.0)` = unlimited for this model.
#[tauri::command]
pub fn bt_blob_ratio(state: State<'_, AppState>, blake3: String) -> Option<f64> {
    state.engine.bittorrent_blob_max_ratio(&blake3)
}

/// Set (or clear, when `cap` is `None`) the per-model stop-at-ratio override.
#[tauri::command]
pub fn set_bt_blob_ratio(state: State<'_, AppState>, blake3: String, cap: Option<f64>) {
    state.engine.set_bittorrent_blob_max_ratio(&blake3, cap);
}

/// Force a full piece re-hash of a seeded/downloaded blob against its torrent.
#[tauri::command]
pub async fn bt_force_recheck(state: State<'_, AppState>, blake3: String) -> Result<(), String> {
    state
        .engine
        .bt_force_recheck(&blake3)
        .await
        .map_err(|e| e.to_string())
}

/// Ordered list of waiting (queued) transfer ids; front = next to start.
#[tauri::command]
pub fn download_queue_order(state: State<'_, AppState>) -> Vec<String> {
    state
        .engine
        .download_queue_order()
        .into_iter()
        .map(|id| id.0)
        .collect()
}

/// Reposition a queued transfer. `dir` is "up" | "down" | "top" | "bottom".
#[tauri::command]
pub fn queue_reorder(state: State<'_, AppState>, id: String, dir: String) {
    let mv = match dir.as_str() {
        "up" => noema_core::QueueMove::Up,
        "down" => noema_core::QueueMove::Down,
        "top" => noema_core::QueueMove::Top,
        "bottom" => noema_core::QueueMove::Bottom,
        _ => return,
    };
    state
        .engine
        .queue_reorder(&noema_core::transfer::TransferId(id), mv);
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn add_from_mesh(
    app: AppHandle,
    state: State<'_, AppState>,
    blake3: String,
    sha256: String,
    name: String,
    size: u64,
    license: String,
    magnet: Option<String>,
    client_ref: Option<String>,
) -> Result<usize, String> {
    // The id is deterministic from the content id, so tell the front-end now (no
    // FIFO adopt) — the engine derives the same id inside `add_by_content`.
    let mid = content_manifest_id(&sha256, &blake3);
    // Register the transfer up front (mirrors `download_model`) so Pause / Stop have
    // a live control the instant the card appears — `add_by_content` reuses this
    // registered id, so a pause pressed before the first byte isn't a silent no-op.
    state.engine.register_transfer(&mid);
    emit_registered(&app, &client_ref, &mid);
    let target = noema_core::ShareTarget {
        name,
        size,
        sha256,
        blake3,
        license,
        // A non-empty magnet adds a BitTorrent source to the synthesized manifest,
        // so the receiver can join the swarm (RECV side of Phase 7).
        magnet: magnet.unwrap_or_default(),
        ..Default::default()
    };
    let progress = progress_emitter(app.clone());
    let result = state.engine.add_by_content(target, Some(progress)).await;
    match result {
        Ok(out) => {
            let _ = app.emit(
                "download://done",
                serde_json::json!({ "transfer_id": mid, "manifest_id": mid, "status": "done" }),
            );
            Ok(out.artifacts.len())
        }
        Err(e) => {
            let _ = app.emit(
                "download://done",
                serde_json::json!({
                    "transfer_id": mid,
                    "manifest_id": mid,
                    "status": download_end_status(&e),
                    "message": e.to_string(),
                }),
            );
            Ok(0)
        }
    }
}

#[tauri::command]
pub fn clear_cache(state: State<'_, AppState>) -> Result<u64, String> {
    state
        .engine
        .evict_cache(noema_core::EvictPolicy::Unreferenced)
        .map(|r| r.freed_bytes)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn export_diagnostics(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    state.engine.export_diagnostics().map_err(|e| e.to_string())
}

/// Reveal a file/dir in the OS file manager (Finder / Explorer / xdg). Errors on an
/// empty or nonexistent path so the front-end can show "file is missing" feedback
/// instead of silently opening nothing (or the wrong place).
#[tauri::command]
pub fn reveal(path: String) -> Result<(), String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("no location to open".into());
    }
    let p = std::path::PathBuf::from(path);
    if !p.exists() {
        return Err("file not found — it may have moved or been deleted".into());
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(&p)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg("/select,")
            .arg(&p)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = p.parent().unwrap_or(&p);
        std::process::Command::new("xdg-open")
            .arg(dir)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
