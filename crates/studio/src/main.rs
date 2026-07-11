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
    // Bound at engine open, so takes effect on next launch.
    cfg.max_concurrent_downloads = s.bt_max_concurrent.max(1) as usize;
    cfg.rate_limit.set_bps(s.cap_bps());
    cfg.share_gated = s.share_gated;
    // Always wire the tracker (falls back to the hosted default) so discovery works.
    cfg.tracker_url = Some(s.tracker());
    // Iroh fetch route: master AND its download sub-switch (seeding is gated separately).
    cfg.transport.iroh_enabled = s.iroh_enabled && s.iroh_download;
    // BitTorrent settings take effect on next launch (bound at engine open).
    cfg.transport.bittorrent_enabled = s.bt_enabled;
    cfg.transport.bittorrent_download = s.bt_download;
    cfg.transport.bittorrent_seed = s.bt_seed;
    cfg.transport.bittorrent_listen_port_range = s.bt_listen_range();
    cfg.transport.bittorrent_max_up_bps = s.bt_up_bps();
    cfg.transport.bittorrent_max_down_bps = s.bt_down_bps();
    // Stop-at-ratio: armed at session open, so takes effect on next launch.
    cfg.transport.bittorrent_max_ratio = s.bt_max_ratio.max(0.0);
    cfg.transport.bittorrent_sequential = s.bt_sequential;
    // Public trackers (in addition to the DHT). Privacy-relevant; takes effect on next launch.
    cfg.transport.bittorrent_use_public_trackers = s.bt_use_public_trackers;
    // Connection/discovery options — all bind at session init, so next launch.
    cfg.transport.bittorrent_enable_upnp = s.bt_upnp;
    cfg.transport.bittorrent_enable_dht = s.bt_dht;
    cfg.transport.bittorrent_enable_lsd = s.bt_lsd;
    cfg.transport.bittorrent_peer_protocol = noema_core::BtPeerProtocol::from_u8(s.bt_protocol);
    cfg.transport.bittorrent_max_peers_per_torrent = s.bt_max_peers;
    cfg.transport.bittorrent_anonymous = s.bt_anonymous;
    // Applied live by `set_download_preference`; seeded so the first download honors it.
    cfg.download_preference = noema_core::DownloadPreference::from_u8(s.download_preference);
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

    // First-run identity bootstrap: stable id + device name, persisted so peers see a consistent name.
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
    // Install the time-of-day bandwidth schedule and start its ticker.
    engine.set_bandwidth_schedule(settings.bandwidth_schedule());

    // Seed over Iroh only when the master is on AND the seed sub-switch is on.
    let start_share_on_launch = settings.iroh_enabled && settings.share_worldwide;
    let state = AppState {
        engine: Arc::new(engine),
        root,
        share: Mutex::new(None),
    };

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            commands::app_info,
            commands::search_models,
            commands::popular_models,
            commands::model_list,
            commands::model_list_page,
            commands::model_conversions,
            commands::model_detail,
            commands::check_model_updates,
            commands::scan_import,
            commands::runtimes_present,
            commands::handoff_lmstudio,
            commands::handoff_ollama,
            commands::model_readme,
            commands::open_external,
            commands::download_model,
            commands::resume_download,
            commands::mesh_search,
            commands::add_by_link,
            commands::add_from_mesh,
            commands::list_transfers,
            commands::resumable_downloads,
            commands::remove_transfer,
            commands::discard_transfer,
            commands::list_library,
            commands::list_cache,
            commands::source_health,
            commands::set_share,
            commands::share_activity,
            commands::transfer_routes,
            commands::share_needs_confirmation,
            commands::confirm_gated_share,
            commands::install_model,
            commands::get_settings,
            commands::save_settings,
            commands::start_worldwide,
            commands::stop_worldwide,
            commands::worldwide_status,
            commands::uploads_list,
            commands::apply_identity,
            commands::set_token,
            commands::clear_token,
            commands::token_status,
            commands::pause_download,
            commands::stop_download,
            commands::import_local,
            commands::edit_model,
            commands::delete_model,
            commands::copy_share_link,
            commands::bt_magnet,
            commands::is_iroh_seeding,
            commands::bt_peers,
            commands::bt_peers_for_blob,
            commands::set_download_preference,
            commands::pause_all,
            commands::bt_blob_ratio,
            commands::set_bt_blob_ratio,
            commands::bt_force_recheck,
            commands::download_queue_order,
            commands::queue_reorder,
            commands::clear_cache,
            commands::export_diagnostics,
            commands::reveal,
        ])
        .setup(move |app| {
            if start_share_on_launch {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let state = handle.state::<AppState>();
                    if let Err(e) =
                        commands::start_worldwide_inner(&state.engine, &state.root, &state.share)
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
        // Window close: on macOS this doesn't quit by default, so clean up and exit
        // to stop the seeder lingering as a peer in others' Explore.
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

/// Stop seeding and withdraw tracker announces on close; runs once and is time-bounded.
fn shutdown_cleanup(handle: &tauri::AppHandle) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    let state = handle.state::<AppState>();
    // Pause every in-flight transfer first so partials survive and BT flushes fastresume for a clean resume.
    state.engine.request_pause();
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
