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
    has_gguf: bool,
    license: Option<String>,
    tags: Vec<String>,
    last_modified: Option<String>,
}

fn to_hit(m: noema_core::hf::HfModel) -> ModelHit {
    ModelHit {
        name: m.name().to_string(),
        author: m.author().to_string(),
        license: m.license(),
        has_gguf: m.has_gguf(),
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

/// Progress event payload pushed to the webview during a download.
#[derive(Serialize, Clone)]
pub struct ProgressEvent {
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
}

/// Build the `download://progress` emitter handed to the engine's downloader.
fn progress_emitter(app: AppHandle) -> Progress {
    Arc::new(move |p: DownloadProgress| {
        let _ = app.emit(
            "download://progress",
            ProgressEvent {
                manifest_id: p.manifest_id,
                artifact: p.artifact_path,
                source: p.source_id,
                bytes_done: p.bytes_done,
                bytes_total: p.bytes_total,
                phase: p.phase.to_string(),
                failover_reason: p.failover_reason,
                effective_start: p.effective_start,
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

/// A progress emitter that also records the engine's manifest id from the first
/// tick, so a download started by content/link (where the id is synthesized
/// internally) can still be resumed later from the Transfers page.
fn capturing_emitter(
    app: AppHandle,
    id_cell: std::sync::Arc<std::sync::Mutex<String>>,
) -> Progress {
    Arc::new(move |p: DownloadProgress| {
        if let Ok(mut g) = id_cell.lock() {
            if g.is_empty() {
                g.clone_from(&p.manifest_id);
            }
        }
        let _ = app.emit(
            "download://progress",
            ProgressEvent {
                manifest_id: p.manifest_id,
                artifact: p.artifact_path,
                source: p.source_id,
                bytes_done: p.bytes_done,
                bytes_total: p.bytes_total,
                phase: p.phase.to_string(),
                failover_reason: p.failover_reason,
                effective_start: p.effective_start,
            },
        );
    })
}

/// Import the chosen variant's manifest, then download it, streaming verified
/// bytes into the cache. Emits `download://progress` per chunk-boundary and
/// `download://done` at the end.
#[tauri::command]
pub async fn download_model(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    file: Option<String>,
) -> Result<DownloadResult, String> {
    let engine = state.engine.clone();
    let detail = engine
        .hf_model_detail(&id)
        .await
        .map_err(|e| e.to_string())?;

    // Resolve the import: an explicit GGUF file, else the first GGUF, else the
    // safetensors/MLX bundle.
    let quants = detail.gguf_quants();
    let import = if let Some(fname) = file.as_deref() {
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

    let progress = progress_emitter(app.clone());

    match engine.download(&manifest_id, Some(progress)).await {
        Ok(out) => {
            let _ = app.emit(
                "download://done",
                serde_json::json!({ "manifest_id": out.manifest_id, "status": "done" }),
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
                serde_json::json!({ "manifest_id": out.manifest_id, "status": "done" }),
            );
            Ok(DownloadResult {
                manifest_id: out.manifest_id,
                artifacts: out.artifacts.len(),
            })
        }
        Err(e) => {
            let _ = app.emit(
                "download://done",
                serde_json::json!({
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
    peers: usize,
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
    let group = noema_core::identity::group_id(&StudioSettings::load(&state.root).group_code);
    let rows = state
        .engine
        .network_catalog(&query, group)
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
) -> Result<usize, String> {
    let link = link.trim().to_string();
    if link.is_empty() {
        return Err("paste a share link (atlas1:… or atlasb1:…)".into());
    }
    let engine = state.engine.clone();
    let id_cell = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let progress = capturing_emitter(app.clone(), id_cell.clone());

    let result = if noema_core::is_bundle_link(&link) {
        let bundle = noema_core::ShareBundle::decode(&link).map_err(|e| e.to_string())?;
        engine
            .add_bundle(bundle, Some(progress))
            .await
            .map(|v| v.len())
    } else {
        let target = noema_core::ShareTarget::decode(&link).map_err(|e| e.to_string())?;
        engine
            .add_by_content(target, Some(progress))
            .await
            .map(|o| o.artifacts.len())
    };
    let mid = id_cell.lock().map(|g| g.clone()).unwrap_or_default();
    match result {
        Ok(n) => {
            let _ = app.emit(
                "download://done",
                serde_json::json!({ "manifest_id": mid, "status": "done" }),
            );
            Ok(n)
        }
        Err(e) => {
            let _ = app.emit(
                "download://done",
                serde_json::json!({
                    "manifest_id": mid,
                    "status": download_end_status(&e),
                    "message": e.to_string(),
                }),
            );
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
}

#[tauri::command]
pub fn list_library(state: State<'_, AppState>) -> Result<Vec<LibItem>, String> {
    let models = state.engine.installed_models().map_err(|e| e.to_string())?;
    Ok(models
        .into_iter()
        .map(|m| LibItem {
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
    if let Some(w) = state.share.lock().await.as_ref() {
        if on {
            if let Ok(items) = state.engine.share_announce_items() {
                w.seeder_handle()
                    .announce(&items, &StudioSettings::load(&state.root).tracker())
                    .await;
            }
        } else {
            w.unseed_and_disconnect(&blake3).await;
            state.engine.withdraw_from_tracker(&[blake3.clone()]).await;
        }
    }
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
pub fn save_settings(state: State<'_, AppState>, settings: StudioSettings) -> Result<(), String> {
    settings.save(&state.root).map_err(|e| e.to_string())?;
    let engine = &state.engine;
    engine.set_max_download_connections(settings.download_connections.max(1) as usize);
    engine.set_share_gated_enabled(settings.share_gated);
    engine.set_hf_download_enabled(settings.allow_hf_download);
    engine.rate_limit().set_bps(settings.cap_bps());
    Ok(())
}

/// Start the worldwide seeder (idempotent), storing the handle in managed state.
/// Shared by the `start_worldwide` command and the launch `setup` hook.
pub async fn start_worldwide_inner(
    engine: &noema_core::Engine,
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
    start_worldwide_inner(state.engine.as_ref(), &state.root, &state.share)
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

#[tauri::command]
pub async fn seeder_metrics(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let guard = state.share.lock().await;
    Ok(match guard.as_ref() {
        Some(w) => serde_json::json!({ "sharing": true, "active_uploads": w.active_uploads() }),
        None => serde_json::json!({ "sharing": false, "active_uploads": 0 }),
    })
}

/// Per-model upload activity: every shared model in the library together with
/// how many peers are pulling it right now. Drives the live "sharing" view.
#[tauri::command]
pub async fn uploads_list(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let guard = state.share.lock().await;
    let Some(w) = guard.as_ref() else {
        return Ok(serde_json::json!({ "sharing": false, "total": 0, "models": [] }));
    };
    let models = state.engine.installed_models().map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    let mut total: u64 = 0;
    for m in models.into_iter().filter(|m| m.shareable) {
        let uploads = w.active_uploads_for(&m.blake3);
        total += uploads;
        rows.push(serde_json::json!({
            "name": m.name,
            "blake3": m.blake3,
            "uploads": uploads,
        }));
    }
    Ok(serde_json::json!({ "sharing": true, "total": total, "models": rows }))
}

#[tauri::command]
pub async fn apply_identity(
    state: State<'_, AppState>,
    device_name: String,
    group_code: String,
) -> Result<(), String> {
    let mut s = StudioSettings::load(&state.root);
    s.device_name = device_name.trim().to_string();
    s.group_code = group_code.trim().to_string();
    s.save(&state.root).map_err(|e| e.to_string())?;
    if let Some(w) = state.share.lock().await.as_ref() {
        w.set_identity(s.identity());
    }
    Ok(())
}

#[tauri::command]
pub fn create_group() -> String {
    noema_core::identity::new_group_code()
}

#[tauri::command]
pub async fn worldwide_peers(state: State<'_, AppState>, hash: String) -> Result<usize, String> {
    Ok(state.engine.worldwide_peers(&hash).await)
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

#[tauri::command]
pub fn pause_download(state: State<'_, AppState>) {
    state.engine.request_pause();
}

#[tauri::command]
pub fn stop_download(state: State<'_, AppState>) {
    state.engine.request_stop();
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

#[tauri::command]
pub async fn import_local(
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
    let out = state
        .engine
        .import_local_file_with_meta(std::path::Path::new(&path), meta)
        .await
        .map_err(|e| e.to_string())?;
    if out.shareable {
        if let Some(w) = state.share.lock().await.as_ref() {
            if let Ok(items) = state.engine.share_announce_items() {
                w.seeder_handle()
                    .announce(&items, &StudioSettings::load(&state.root).tracker())
                    .await;
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
    if let Some(w) = state.share.lock().await.as_ref() {
        w.unseed_and_disconnect(&blake3).await;
    }
    state.engine.withdraw_from_tracker(&[blake3.clone()]).await;
    let _ = state.engine.set_model_shared(&blake3, &sha256, false);
    Ok(report.freed_bytes)
}

fn target_for(m: &noema_core::InstalledModel) -> noema_core::ShareTarget {
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
    }
}

fn link_for(engine: &noema_core::Engine, manifest_id: &str) -> Option<String> {
    let m = engine
        .installed_models()
        .ok()?
        .into_iter()
        .find(|m| m.manifest_id == manifest_id)?;
    Some(target_for(&m).encode())
}

#[tauri::command]
pub fn copy_share_link(state: State<'_, AppState>, manifest_id: String) -> Result<String, String> {
    link_for(state.engine.as_ref(), &manifest_id)
        .ok_or_else(|| "model not found in library".to_string())
}

#[tauri::command]
pub async fn add_from_mesh(
    app: AppHandle,
    state: State<'_, AppState>,
    blake3: String,
    sha256: String,
    name: String,
    size: u64,
    license: String,
) -> Result<usize, String> {
    let target = noema_core::ShareTarget {
        name,
        size,
        sha256,
        blake3,
        license,
        ..Default::default()
    };
    let id_cell = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let progress = capturing_emitter(app.clone(), id_cell.clone());
    let result = state.engine.add_by_content(target, Some(progress)).await;
    let mid = id_cell.lock().map(|g| g.clone()).unwrap_or_default();
    match result {
        Ok(out) => {
            let _ = app.emit(
                "download://done",
                serde_json::json!({ "manifest_id": mid, "status": "done" }),
            );
            Ok(out.artifacts.len())
        }
        Err(e) => {
            let _ = app.emit(
                "download://done",
                serde_json::json!({
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

/// Reveal a file/dir in the OS file manager (Finder / Explorer / xdg).
#[tauri::command]
pub fn reveal(path: String) -> Result<(), String> {
    let p = std::path::PathBuf::from(&path);
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
