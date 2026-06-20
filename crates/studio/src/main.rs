#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod commands;
mod settings;

use std::path::PathBuf;
use std::sync::Arc;

use noema_core::{Engine, EngineConfig};
use settings::StudioSettings;
use tauri::Manager;
use tokio::sync::Mutex;

/// Shared application state handed to every command.
pub struct AppState {
    pub engine: Arc<Engine>,
    pub root: PathBuf,
    /// The live worldwide-share (iroh) handle while seeding is running.
    pub share: Mutex<Option<noema_core::engine::WorldwideShare>>,
}

/// Build an engine from persisted settings (mirrors the CLI/desktop `build_engine`).
fn build_engine(root: &std::path::Path, s: &StudioSettings) -> anyhow::Result<Engine> {
    let mut cfg = EngineConfig::new(root.to_path_buf());
    cfg.max_download_connections = s.download_connections.max(1) as usize;
    cfg.rate_limit.set_bps(s.cap_bps());
    cfg.share_gated = s.share_gated;
    // Always wire the tracker (falls back to the hosted default) so Explore /
    // worldwide discovery works.
    cfg.tracker_url = Some(s.tracker());
    if s.proxy_enabled {
        if let Some(p) = nonempty(&s.proxy_url) {
            cfg.transport.proxy = Some(p);
        }
    }
    if s.hf_mirror_enabled {
        if let Some(m) = nonempty(&s.hf_mirror_url) {
            cfg.transport.hf_endpoint = m;
        }
    }
    Ok(Engine::open(cfg)?)
}

fn nonempty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn main() {
    let root = noema_core::paths::default_root();
    let mut settings = StudioSettings::load(&root);

    // First-run identity bootstrap: a stable id + a friendly device name, both
    // persisted so peers see a consistent name.
    let mut dirty = false;
    if settings.device_id.trim().is_empty() {
        settings.device_id = noema_core::identity::new_device_id();
        dirty = true;
    }
    if settings.device_name.trim().is_empty() {
        settings.device_name = noema_core::identity::default_device_name();
        dirty = true;
    }
    if dirty {
        let _ = settings.save(&root);
    }

    let engine = build_engine(&root, &settings).expect("failed to open the Noema engine");
    engine.set_hf_download_enabled(settings.allow_hf_download);

    let start_share_on_launch = settings.share_worldwide;
    let state = AppState {
        engine: Arc::new(engine),
        root,
        share: Mutex::new(None),
    };

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            commands::app_info,
            commands::search_models,
            commands::popular_models,
            commands::model_detail,
            commands::download_model,
            commands::resume_download,
            commands::mesh_search,
            commands::add_by_link,
            commands::add_from_mesh,
            commands::list_library,
            commands::list_cache,
            commands::source_health,
            commands::set_share,
            commands::install_model,
            commands::get_settings,
            commands::save_settings,
            commands::start_worldwide,
            commands::stop_worldwide,
            commands::worldwide_status,
            commands::seeder_metrics,
            commands::uploads_list,
            commands::apply_identity,
            commands::create_group,
            commands::worldwide_peers,
            commands::set_token,
            commands::clear_token,
            commands::token_status,
            commands::pause_download,
            commands::stop_download,
            commands::import_local,
            commands::edit_model,
            commands::delete_model,
            commands::copy_share_link,
            commands::clear_cache,
            commands::export_diagnostics,
            commands::reveal,
        ])
        .setup(move |app| {
            if start_share_on_launch {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let state = handle.state::<AppState>();
                    if let Err(e) = commands::start_worldwide_inner(
                        state.engine.as_ref(),
                        &state.root,
                        &state.share,
                    )
                    .await
                    {
                        eprintln!("noema-studio: worldwide share failed to start: {e}");
                    }
                });
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Noema Studio");

    app.run(|handle, event| match &event {
        // Quit (Cmd+Q / the Quit menu).
        tauri::RunEvent::ExitRequested { .. } => shutdown_cleanup(handle),
        // Window close (the red traffic-light button). On macOS this does *not*
        // quit the process by default, so without handling it the seeder would keep
        // running and re-announcing after the user thinks the app is closed — and
        // the device would linger as a peer in everyone else's Explore. Clean up,
        // then exit so a close behaves like a quit.
        tauri::RunEvent::WindowEvent {
            event: tauri::WindowEvent::CloseRequested { .. },
            ..
        } => {
            shutdown_cleanup(handle);
            handle.exit(0);
        }
        _ => {}
    });
}

/// Stop seeding and withdraw this device's announces from the tracker, so it stops
/// showing as a peer in others' Explore the instant the app closes rather than
/// lingering until the announce TTL. Runs once (guarded against the close→exit
/// re-entry), and is bounded so a slow or unreachable tracker can't hang the quit.
fn shutdown_cleanup(handle: &tauri::AppHandle) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    let state = handle.state::<AppState>();
    tauri::async_runtime::block_on(async {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            // Withdraw first (it reads our stable node id), then stop the seeder.
            state.engine.withdraw_from_tracker(&[]).await;
            if let Some(w) = state.share.lock().await.take() {
                w.stop().await;
            }
        })
        .await;
    });
}
