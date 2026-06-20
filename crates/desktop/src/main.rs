#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod transport_ui;

use eframe::egui;
use noema_core::db::CacheBlobRow;
use noema_core::engine::{
    DownloadProgress, Engine, EngineConfig, EvictPolicy, InstalledModel, NetworkModel, Progress,
};
use noema_core::hf::{HfFile, HfModel, HfModelDetail};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use transport_ui::{paint_transport_glyph, transport_badge, transport_chip, TransportKind};

/// The app logo (navy "N" + compass rose), used for the window/dock icon and the
/// in-app header. Single source of truth at `assets/logo.png`.
const LOGO_PNG: &[u8] = include_bytes!("../../../assets/logo.png");

/// Decode the logo to an eframe window icon (RGBA).
fn window_icon() -> Option<egui::IconData> {
    let img = image::load_from_memory(LOGO_PNG).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    Some(egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    })
}

/// Decode the logo into an egui texture for in-app use (header).
fn load_logo_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(LOGO_PNG).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw());
    Some(ctx.load_texture("noema-logo", color, egui::TextureOptions::LINEAR))
}

fn main() -> eframe::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "noema_core=warn".into()),
        )
        .try_init();
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1120.0, 760.0])
        .with_min_inner_size([880.0, 560.0])
        .with_title("Noema Atlas");
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Noema Atlas",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

/// Which color theme the UI renders in. Persisted in `Settings`.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default, Debug)]
#[serde(rename_all = "lowercase")]
enum ThemeMode {
    #[default]
    Dark,
    Light,
}

impl ThemeMode {
    fn is_dark(self) -> bool {
        matches!(self, ThemeMode::Dark)
    }
    fn toggled(self) -> ThemeMode {
        match self {
            ThemeMode::Dark => ThemeMode::Light,
            ThemeMode::Light => ThemeMode::Dark,
        }
    }
}

/// A modern theme (light or dark) layered on top of egui's stock visuals.
///
/// This is *pure styling*. egui is immediate-mode, so the entire "theme" is a
/// handful of colors, roundings and paddings stored in one `Style` struct and
/// handed to the renderer that already runs every frame. It allocates nothing
/// per-frame and adds no textures — so it costs essentially zero extra RAM,
/// which is exactly why we can modernize the look (and offer both modes) without
/// giving up the low-memory property that made us choose egui over a webview in
/// the first place. (A custom font would be the one thing with a real cost; we
/// deliberately keep the stock font so the default footprint is unchanged.)
fn apply_theme(ctx: &egui::Context, dark: bool) {
    use egui::{Color32, Margin, Rounding, Shadow, Stroke};

    // Two coordinated surface ramps. Dark is a cool blue-charcoal; light is a
    // soft near-white that avoids harsh pure #fff. The accent (brand blue) is
    // shared but darkened in light mode so it stays legible on pale surfaces.
    let rgb = Color32::from_rgb;
    let (accent, panel, window, extreme, surface, surface_hi, surface_on, line, text, text_hi) =
        if dark {
            (
                rgb(0x6c, 0x9c, 0xff), // accent
                rgb(0x13, 0x15, 0x1a), // panel
                rgb(0x1a, 0x1d, 0x24), // window
                rgb(0x0d, 0x0f, 0x13), // extreme (text-edit wells)
                rgb(0x23, 0x27, 0x31), // resting button
                rgb(0x2d, 0x32, 0x3e), // hovered button
                rgb(0x36, 0x3c, 0x4a), // pressed button
                rgb(0x2c, 0x31, 0x3c), // separators / borders
                rgb(0xcc, 0xd2, 0xda), // body text
                rgb(0xf2, 0xf4, 0xf8), // emphasis / headings
            )
        } else {
            (
                rgb(0x2f, 0x6f, 0xe0), // accent
                rgb(0xf2, 0xf3, 0xf6), // panel
                rgb(0xfb, 0xfc, 0xfd), // window
                rgb(0xff, 0xff, 0xff), // extreme
                rgb(0xe9, 0xeb, 0xf0), // resting button
                rgb(0xde, 0xe1, 0xe8), // hovered button
                rgb(0xd0, 0xd5, 0xde), // pressed button
                rgb(0xd2, 0xd6, 0xde), // separators / borders
                rgb(0x41, 0x47, 0x50), // body text
                rgb(0x14, 0x17, 0x1c), // emphasis / headings
            )
        };
    let (faint_bg, sel_bg, sel_stroke, shadow_alpha) = if dark {
        (
            rgb(0x1b, 0x1f, 0x27),
            rgb(0x2c, 0x4a, 0x86),
            rgb(0xe2, 0xec, 0xff),
            110,
        )
    } else {
        (
            rgb(0xea, 0xed, 0xf2),
            rgb(0xcf, 0xe0, 0xff),
            rgb(0x1c, 0x3f, 0x7a),
            38,
        )
    };

    let mut style = (*ctx.style()).clone();
    let v = &mut style.visuals;
    v.dark_mode = dark;
    v.panel_fill = panel;
    v.window_fill = window;
    v.extreme_bg_color = extreme;
    v.code_bg_color = extreme;
    v.faint_bg_color = faint_bg; // striped-row tint
    v.hyperlink_color = accent;

    // Selected tabs and the text-selection highlight share these. The selected
    // tab's text color is taken from `selection.stroke`.
    v.selection.bg_fill = sel_bg;
    v.selection.stroke = Stroke::new(1.0, sel_stroke);

    // Softer, rounder window/menu chrome with gentle drop shadows.
    v.window_rounding = Rounding::same(12.0);
    v.menu_rounding = Rounding::same(10.0);
    v.window_stroke = Stroke::new(1.0, line);
    v.window_shadow = Shadow {
        offset: egui::vec2(0.0, 10.0),
        blur: 32.0,
        spread: 0.0,
        color: Color32::from_black_alpha(shadow_alpha),
    };
    v.popup_shadow = Shadow {
        offset: egui::vec2(0.0, 6.0),
        blur: 20.0,
        spread: 0.0,
        color: Color32::from_black_alpha((shadow_alpha as f32 * 0.8) as u8),
    };

    // Widget states: a consistent 8px radius, subtle layered surfaces, and an
    // accent edge that lights up on hover/press instead of the stock flat gray.
    let r = Rounding::same(8.0);
    // Text on a pressed widget: white reads on the dark active fill, but would
    // vanish on light mode's pale active fill — use the near-black emphasis there.
    let on_active = if dark { Color32::WHITE } else { text_hi };
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = panel;
    w.noninteractive.weak_bg_fill = panel;
    w.noninteractive.bg_stroke = Stroke::new(1.0, line); // separators
    w.noninteractive.fg_stroke = Stroke::new(1.0, text);
    w.noninteractive.rounding = r;

    w.inactive.bg_fill = surface;
    w.inactive.weak_bg_fill = surface;
    w.inactive.bg_stroke = Stroke::new(1.0, line);
    w.inactive.fg_stroke = Stroke::new(1.0, text_hi);
    w.inactive.rounding = r;

    w.hovered.bg_fill = surface_hi;
    w.hovered.weak_bg_fill = surface_hi;
    w.hovered.bg_stroke = Stroke::new(1.0, accent.gamma_multiply(0.55));
    w.hovered.fg_stroke = Stroke::new(1.0, text_hi);
    w.hovered.rounding = r;
    w.hovered.expansion = 1.0;

    w.active.bg_fill = surface_on;
    w.active.weak_bg_fill = surface_on;
    w.active.bg_stroke = Stroke::new(1.0, accent);
    w.active.fg_stroke = Stroke::new(1.0, on_active);
    w.active.rounding = r;
    w.active.expansion = 1.0;

    w.open.bg_fill = surface;
    w.open.weak_bg_fill = surface_hi;
    w.open.bg_stroke = Stroke::new(1.0, line);
    w.open.fg_stroke = Stroke::new(1.0, text_hi);
    w.open.rounding = r;

    // A little more air than the cramped stock defaults (item 8×3, button 4×1).
    let s = &mut style.spacing;
    s.item_spacing = egui::vec2(8.0, 6.0);
    s.button_padding = egui::vec2(10.0, 6.0);
    s.window_margin = Margin::same(12.0);
    s.menu_margin = Margin::same(8.0);

    ctx.set_style(style);
}

/// Semantic colors for the app's *custom-painted* UI (status-banner cards,
/// badges, toasts, accents) that egui's `Style` doesn't reach. Each entry has a
/// dark and a light value so the whole UI — not just the chrome — flips cleanly.
///
/// Fetch it at the top of a draw fn with [`pal_of`]; it's `Copy`, so nested
/// closures capture it for free. No allocation, no per-frame cost.
#[derive(Clone, Copy)]
struct Palette {
    // Accents (foreground / icon / badge background).
    blue: egui::Color32,    // info, "Recommended", primary
    blue_dl: egui::Color32, // download direction
    green: egui::Color32,   // success, upload, online
    amber: egui::Color32,   // warning
    amber_dim: egui::Color32,
    red: egui::Color32,    // error / danger
    orange: egui::Color32, // stalled / partial
    // Status callout cards: fill, border, and text-on-card.
    green_bg: egui::Color32,
    green_bg_hi: egui::Color32,
    on_green: egui::Color32, // strong text on green_bg_hi
    green_text: egui::Color32,
    green_text2: egui::Color32,
    amber_bg: egui::Color32,
    amber_border: egui::Color32,
    amber_text: egui::Color32,
    amber_text2: egui::Color32,
    amber_faint: egui::Color32,
    red_bg: egui::Color32,
    red_border: egui::Color32,
    red_text: egui::Color32,
    red_text2: egui::Color32,
    red_strong: egui::Color32,
    // Neutrals.
    muted: egui::Color32,  // secondary text / generic badge
    faint: egui::Color32,  // dimmed / offline
    strong: egui::Color32, // near-fg emphasis text
    // Toast surface.
    toast_bg: egui::Color32,
    toast_text: egui::Color32,
}

impl Palette {
    const fn dark() -> Self {
        let rgb = egui::Color32::from_rgb;
        Self {
            blue: rgb(0x6c, 0x9c, 0xff),
            blue_dl: rgb(0x5d, 0xb0, 0xff),
            green: rgb(0x56, 0xd3, 0x64),
            amber: rgb(0xe3, 0xb3, 0x41),
            amber_dim: rgb(0xd0, 0x9a, 0x3c),
            red: rgb(0xff, 0x6b, 0x6b),
            orange: rgb(0xd0, 0x6a, 0x3c),
            green_bg: rgb(0x12, 0x2e, 0x1c),
            green_bg_hi: rgb(0x18, 0x4a, 0x2a),
            on_green: rgb(0xff, 0xff, 0xff),
            green_text: rgb(0x7b, 0xe0, 0x95),
            green_text2: rgb(0xc8, 0xe6, 0xd0),
            amber_bg: rgb(0x2e, 0x27, 0x10),
            amber_border: rgb(0x9d, 0x7b, 0x3b),
            amber_text: rgb(0xe8, 0xd9, 0xb0),
            amber_text2: rgb(0xff, 0xc0, 0x7a),
            amber_faint: rgb(0xbf, 0xb0, 0x8a),
            red_bg: rgb(0x3a, 0x16, 0x16),
            red_border: rgb(0x9d, 0x3b, 0x3b),
            red_text: rgb(0xff, 0xd0, 0xd0),
            red_text2: rgb(0xff, 0xb7, 0xb7),
            red_strong: rgb(0xff, 0x7a, 0x7a),
            muted: rgb(0x9a, 0x9a, 0x9a),
            faint: rgb(0x7a, 0x7a, 0x7a),
            strong: rgb(0xe6, 0xe6, 0xe6),
            toast_bg: rgb(0x18, 0x1b, 0x22),
            toast_text: rgb(0xf2, 0xf4, 0xf8),
        }
    }

    const fn light() -> Self {
        let rgb = egui::Color32::from_rgb;
        Self {
            blue: rgb(0x2f, 0x6f, 0xe0),
            blue_dl: rgb(0x1f, 0x7f, 0xd6),
            green: rgb(0x1f, 0x9d, 0x4d),
            amber: rgb(0xb9, 0x82, 0x1a),
            amber_dim: rgb(0x9a, 0x6f, 0x12),
            red: rgb(0xcf, 0x2f, 0x2f),
            orange: rgb(0xc2, 0x54, 0x1f),
            green_bg: rgb(0xe6, 0xf5, 0xec),
            green_bg_hi: rgb(0xd3, 0xef, 0xdc),
            on_green: rgb(0x10, 0x50, 0x2b),
            green_text: rgb(0x1c, 0x7a, 0x3e),
            green_text2: rgb(0x2f, 0x6b, 0x48),
            amber_bg: rgb(0xfb, 0xf1, 0xd6),
            amber_border: rgb(0xdc, 0xb8, 0x68),
            amber_text: rgb(0x7a, 0x5a, 0x12),
            amber_text2: rgb(0x8a, 0x5a, 0x12),
            amber_faint: rgb(0x8c, 0x7a, 0x4a),
            red_bg: rgb(0xfb, 0xe2, 0xe2),
            red_border: rgb(0xe0, 0x9a, 0x9a),
            red_text: rgb(0x9d, 0x26, 0x26),
            red_text2: rgb(0xb2, 0x3a, 0x3a),
            red_strong: rgb(0xb3, 0x20, 0x20),
            muted: rgb(0x6b, 0x6b, 0x6b),
            faint: rgb(0xa6, 0xac, 0xb5),
            strong: rgb(0x1a, 0x1d, 0x24),
            toast_bg: rgb(0xf5, 0xf6, 0xf9),
            toast_text: rgb(0x1a, 0x1d, 0x24),
        }
    }
}

/// The semantic palette for the theme `ui` is currently rendering in.
fn pal_of(ui: &egui::Ui) -> Palette {
    if ui.visuals().dark_mode {
        Palette::dark()
    } else {
        Palette::light()
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Discover,
    Network,
    Transfers,
    Library,
    Settings,
}

#[derive(Serialize, Deserialize, Clone)]
struct Settings {
    models_dir: String,
    /// Download cap in megabits/sec; 0 = unlimited.
    download_cap_mbps: u32,
    /// Max parallel connections for a single large download (1 = single
    /// connection / no segmentation). Speeds up big HTTP/Hugging Face fetches.
    #[serde(default = "default_download_connections")]
    download_connections: u32,
    /// Worldwide P2P content tracker URL.
    #[serde(default = "default_tracker")]
    tracker_url: String,
    /// Share downloaded models worldwide (seed over Iroh + announce to tracker).
    #[serde(default)]
    share_worldwide: bool,
    /// Also auto-share gated/token-walled/licensed models (default off — only
    /// openly-licensed models auto-share). Per-model opt-in still works either way.
    #[serde(default)]
    share_gated: bool,
    /// Stable per-install id (generated once).
    #[serde(default)]
    device_id: String,
    /// Human device name shown to peers ("from your devices").
    #[serde(default)]
    device_name: String,
    /// Shared "My Devices" group code (capability). Empty = no group.
    #[serde(default)]
    group_code: String,
    /// Skip the confirmation popup when fetching from a share link or Explore.
    /// Off by default: a click shouldn't silently start a multi-GB download.
    #[serde(default)]
    skip_download_confirm: bool,
    /// Route Hugging Face search + downloads through a mirror (e.g. for regions
    /// where huggingface.co is slow or blocked). Works exactly like the real Hub.
    #[serde(default)]
    hf_mirror_enabled: bool,
    /// Allow Hugging Face as a byte-download fallback. Search remains on either way.
    #[serde(default)]
    allow_hf_download: bool,
    /// The HF mirror origin to use when `hf_mirror_enabled`. Defaults to the
    /// community mirror; editable so any compatible mirror works.
    #[serde(default = "default_hf_mirror")]
    hf_mirror_url: String,
    /// Route the app's internet traffic (HF, tracker, IPFS) through a proxy —
    /// the in-app "VPN tunnel".
    #[serde(default)]
    proxy_enabled: bool,
    /// Proxy URL used when `proxy_enabled`: `http://`, `https://`, `socks5://`,
    /// or `socks5h://host:port` (socks5h resolves DNS through the tunnel).
    #[serde(default)]
    proxy_url: String,
    /// Whether the one-time first-run intro card has been dismissed.
    #[serde(default)]
    seen_intro: bool,
    /// Light or dark UI theme. Defaults to dark (the original look) for existing
    /// installs whose settings file predates this field.
    #[serde(default)]
    theme: ThemeMode,
}

/// The subset of settings that only take effect at startup (the engine reads
/// them once in `App::new`). Snapshotted so the UI can prompt for a restart when
/// any of them is edited but not yet applied.
#[derive(Clone, PartialEq, Eq)]
struct ConnSnapshot {
    tracker_url: String,
    hf_mirror_enabled: bool,
    allow_hf_download: bool,
    hf_mirror_url: String,
    proxy_enabled: bool,
    proxy_url: String,
}

fn conn_snapshot(s: &Settings) -> ConnSnapshot {
    ConnSnapshot {
        tracker_url: s.tracker_url.trim().to_string(),
        hf_mirror_enabled: s.hf_mirror_enabled,
        allow_hf_download: s.allow_hf_download,
        hf_mirror_url: s.hf_mirror_url.trim().to_string(),
        proxy_enabled: s.proxy_enabled,
        proxy_url: s.proxy_url.trim().to_string(),
    }
}

fn default_tracker() -> String {
    noema_core::DEFAULT_TRACKER.to_string()
}

/// Default parallel connections for large downloads (aria2-style segmented fetch).
fn default_download_connections() -> u32 {
    4
}

/// The default community HF mirror (mirrors the same API + resolve endpoints).
fn default_hf_mirror() -> String {
    "https://hf-mirror.com".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            models_dir: default_models_dir().to_string_lossy().to_string(),
            download_cap_mbps: 0,
            download_connections: default_download_connections(),
            tracker_url: default_tracker(),
            // Default ON: a healthy swarm needs seeders. Openly-licensed models
            // you download are shared so others can find them in Explore;
            // gated/token-walled and privately-imported models stay private
            // unless you opt them in (see `share_gated`).
            share_worldwide: true,
            // Default OFF: gated/licensed models are only auto-shared if the
            // operator explicitly opts in (verify content, not licenses — but
            // don't redistribute licensed weights by surprise).
            share_gated: false,
            device_id: String::new(),
            device_name: String::new(),
            group_code: String::new(),
            skip_download_confirm: false,
            hf_mirror_enabled: false,
            // Default ON so day-one downloads work before the mesh has peers. The
            // planner stays P2P-first (a peer with the file is preferred); HF is
            // the verified fallback when no peer has it. Toggle off in Settings.
            allow_hf_download: true,
            hf_mirror_url: default_hf_mirror(),
            proxy_enabled: false,
            proxy_url: String::new(),
            seen_intro: false,
            theme: ThemeMode::Dark,
        }
    }
}

impl Settings {
    /// The announce identity (device name + derived group id) for the catalog.
    fn identity(&self) -> noema_core::tracker::Identity {
        noema_core::tracker::Identity {
            device: if self.device_name.trim().is_empty() {
                "A Noema device".to_string()
            } else {
                self.device_name.trim().to_string()
            },
            group: noema_core::identity::group_id(&self.group_code),
        }
    }
}

/// Messages from background tasks back to the UI thread.
/// How a download task finished, sent to the UI thread. Carries the *typed*
/// interruption so the UI never classifies pause-vs-stop by sniffing error text:
/// a peer disconnect whose message merely contains "stopped" must not read as a
/// user Stop. Built from the engine's typed `Error` at the spawn site.
enum DownloadEnd {
    /// Completed and verified — (manifest_id, bytes).
    Ok(String, u64),
    /// User Pause — partial kept, resumable.
    Paused,
    /// User Stop — partial discarded.
    Stopped,
    /// Anything else: a real failure, carrying the message for `friendly_error`.
    Failed(String),
}

impl DownloadEnd {
    fn from_result(res: Result<(String, u64), noema_core::Error>) -> Self {
        match res {
            Ok((id, bytes)) => DownloadEnd::Ok(id, bytes),
            Err(noema_core::Error::Cancelled) => DownloadEnd::Paused,
            Err(noema_core::Error::Stopped) => DownloadEnd::Stopped,
            Err(e) => DownloadEnd::Failed(e.to_string()),
        }
    }
}

enum Msg {
    Models(Result<Vec<HfModel>, String>),
    Detail(Result<HfModelDetail, String>),
    /// sha256 -> worldwide peer count (from the tracker).
    WorldwidePeers(HashMap<String, usize>),
    Progress {
        done: u64,
        total: u64,
        phase: String,
        source: Option<String>,
        failover_reason: Option<String>,
        effective_start: Option<u64>,
    },
    Done(DownloadEnd),
    Imported(Result<noema_core::LocalImportOutcome, String>),
    /// Worldwide catalog rows for the Network tab.
    Catalog(Result<Vec<noema_core::NetworkModel>, String>),
    /// Tracker state changed after a withdraw/refresh; visible peer counts and
    /// catalog rows should be rechecked now, not after the normal stale timer.
    PeerStateChanged,
}

enum Action {
    Search,
    Suggest(String),
    OpenModel(String),
    Back,
    Download(String),
    /// One-click download of an entire sharded safetensors/MLX model.
    DownloadBundle,
    /// Pause the in-flight download — keeps partial progress so it can resume.
    PauseDownload,
    /// Stop the in-flight download and discard its partial progress.
    StopDownload,
    /// Open the file picker, then the "Send a model" composer (title/license/share).
    OpenComposer,
    /// Confirm the composer: import the file with the chosen metadata and share it.
    ComposerConfirm,
    /// Dismiss the composer without importing.
    ComposerCancel,
    /// Open the composer pre-filled from an already-imported Library model, to
    /// retitle / relicense / share it after the fact.
    EditModel(String),
    Reveal(PathBuf),
    SaveToken,
    ToggleWorldwide,
    /// Confirm stopping worldwide sharing while peers are mid-transfer.
    ConfirmStopWorldwide,
    /// Keep sharing after the stop-while-active confirmation.
    CancelStopWorldwide,
    /// Confirm turning a single file's open-mesh share off while peers are
    /// mid-transfer of it (severs those peers from that file).
    ConfirmStopFileShare,
    /// Keep sharing this file after the stop-while-active confirmation.
    CancelStopFileShare,
    /// Open the per-protocol routes & peers popup for a quant/file.
    OpenQuantDetail(QuantDetail),
    /// Close the routes & peers popup.
    CloseQuantDetail,
    /// Per-model worldwide-share opt-in/out.
    ShareModel {
        blake3: String,
        sha256: String,
        on: bool,
    },
    /// Copy a self-contained share link to the clipboard.
    CopyShareLink(String),
    /// Copy arbitrary text to the clipboard and confirm it in the status line.
    CopyText {
        text: String,
        what: String,
    },
    /// Dismiss the one-time first-run intro; `true` also jumps to Settings.
    DismissIntro(bool),
    /// Fetch a model from the "Add by Content ID / share link" box.
    AddByLink,
    /// Confirm the pending share-link / Explore download from the popup.
    ConfirmDownload,
    /// Dismiss the pending download confirmation popup.
    CancelDownload,
    /// Ask before deleting a Library model from disk and peers.
    RequestDelete(PendingDelete),
    /// Confirm the pending Library deletion.
    ConfirmDelete,
    /// Dismiss the pending Library deletion.
    CancelDelete,
    /// Refresh the worldwide Network catalog.
    RefreshNetwork,
    /// Jump to Settings, usually from an inline transport CTA.
    OpenSettings,
    /// One-click download of a Network-catalog model.
    AddFromNetwork(noema_core::NetworkModel),
    /// Apply a changed device name / group code (restart the worldwide session).
    ApplyIdentity,
    /// Generate a fresh "My Devices" group code on this device.
    CreateGroup,
    ApplySpeedCap,
    /// Apply the parallel-connections setting to the running engine (live).
    ApplyDownloadConnections,
    SaveSettings,
    /// Apply the "Allow Hugging Face downloads" toggle live (no restart).
    SetHfDownload(bool),
    /// Apply the "Also share gated/licensed models" toggle live (no restart).
    SetShareGated(bool),
    /// Switch the UI between light and dark, persist it, and re-apply the style.
    SetTheme(ThemeMode),
    Refresh,
    Evict(EvictPolicy),
}

struct ActiveDownload {
    name: String,
    done: u64,
    total: u64,
    source: Option<String>,
    prev_done: u64,
    /// Whether the current attempt's download baseline has been set (at the first
    /// transfer event after each "connecting"). Guards the session byte counter
    /// against counting a resumed transfer's already-present prefix as an instant
    /// spike on the graph. See [`fold_download_progress`].
    dl_baselined: bool,
    /// Bytes attributed to each source id so far — the multi-source story made
    /// visible ("Hugging Face 1.2 GB · worldwide peer 380 MB"). The engine fetches one
    /// source at a time (with failover), so each byte delta is attributed to the
    /// source that reported it.
    by_source: HashMap<String, u64>,
    /// When this transfer started — used for a "calculating…" ETA grace period.
    started: Instant,
    route_history: Vec<RouteLeg>,
    switched_at: Option<Instant>,
    pending_failover_reason: Option<String>,
    seen_switch_pairs: HashSet<(String, String)>,
    /// Disk verification progress after an in-`open()` transfer; the UI keeps
    /// the network bar full while showing verification progress as the caption.
    verifying: bool,
    verify_done: u64,
}

fn active_download(name: String, total: u64) -> ActiveDownload {
    ActiveDownload {
        name,
        done: 0,
        total,
        source: None,
        prev_done: 0,
        dl_baselined: false,
        by_source: HashMap::new(),
        started: Instant::now(),
        route_history: Vec::new(),
        switched_at: None,
        pending_failover_reason: None,
        seen_switch_pairs: HashSet::new(),
        verifying: false,
        verify_done: 0,
    }
}

#[derive(Clone)]
struct RouteLeg {
    source_id: String,
    started_at: Instant,
    reason: Option<String>,
    start_offset: u64,
}

struct Toast {
    text: String,
    kind: ToastKind,
    shown_at: Instant,
}

#[derive(Clone, Copy)]
enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

/// A share-link / Explore fetch awaiting the user's confirmation (shown in a
/// popup unless they've opted into skipping it in Settings).
#[derive(Clone)]
struct PendingDownload {
    target: noema_core::ShareTarget,
    /// Known worldwide peer count for the dialog, if any.
    peers: Option<usize>,
}

/// A Library model awaiting destructive-delete confirmation.
#[derive(Clone)]
struct PendingDelete {
    name: String,
    blake3: String,
    size_bytes: u64,
    install_path: Option<String>,
    shareable: bool,
}

/// Turning worldwide sharing off while peers are mid-transfer needs confirmation:
/// stopping hard-disconnects them. Holds how many transfers are in flight so the
/// dialog can say so.
#[derive(Clone)]
struct PendingShareOff {
    active_uploads: u64,
}

/// Turning a *single file's* open-mesh share off while peers are pulling that
/// file needs the same confirmation: stopping hard-disconnects them from it.
/// Holds the blob identity (so the actual off can run on confirm), a display name
/// for the dialog, and how many peers are mid-transfer of this file.
#[derive(Clone)]
struct PendingFileShareOff {
    blake3: String,
    sha256: String,
    name: String,
    active_uploads: u64,
}

/// What a "Download" button in the quant routes popup should trigger.
#[derive(Clone)]
enum QuantDownload {
    /// Fetch this Hugging-Face file by rfilename (Discover detail).
    Hf(String),
    /// Fetch the whole safetensors/MLX bundle (Discover detail).
    Bundle,
    /// Fetch a worldwide-catalog row over P2P (Explore tab).
    Network(noema_core::NetworkModel),
    /// Already cached / your own share — no download offered.
    None,
}

/// A single quant/file the user tapped to inspect: where it can be fetched from,
/// broken down per transport with live peer counts. The popup re-reads live peer
/// maps from app state each frame (keyed on `sha256`) so the numbers stay fresh.
#[derive(Clone)]
struct QuantDetail {
    title: String,
    subtitle: String,
    size: u64,
    sha256: String,
    blake3: String,
    download: QuantDownload,
    cached: bool,
}

/// License options for the "Send a model" composer, phrased as consequences. The
/// `spdx` maps onto `RedistributionClass::for_license`, which decides whether a
/// shared model may be auto-reseeded (and thus listed publicly on Explore).
const LICENSE_OPTIONS: &[(&str, &str)] = &[
    ("I'm not sure — link-only, no auto-reshare", "unknown"),
    ("Apache-2.0 — open, anyone can reshare", "apache-2.0"),
    ("MIT — open, anyone can reshare", "mit"),
    ("Llama 3.x Community", "llama3"),
    ("Gemma", "gemma"),
    ("Qwen", "qwen"),
    ("Mistral (Apache-2.0)", "apache-2.0"),
    ("CC-BY-4.0", "cc-by-4.0"),
    ("GPL-3.0", "gpl-3.0"),
];

/// State for the "Send a model" composer — the dialog that lets a user title,
/// license, describe, and share a model that isn't on Hugging Face.
struct ComposerState {
    path: PathBuf,
    filename: String,
    size: u64,
    format: Option<String>,
    /// Read straight from the file header (shown read-only as a trust signal).
    architecture: Option<String>,
    /// Set when retitling/relicensing a model already in the Library (we update
    /// the manifest in place instead of importing a file).
    edit_manifest_id: Option<String>,
    /// Content ids — known up front in edit mode, so the share link is immediate.
    blake3: String,
    sha256: String,
    /// True for fields that came from the file header vs. guessed from the name.
    title_from_file: bool,
    quant_from_file: bool,
    // Editable fields.
    title: String,
    family: String,
    quant: String,
    license_idx: usize,
    description: String,
    origin: String,
    /// false = private link (default); true = publish to the worldwide Explore mesh.
    publish: bool,
    /// Also attempt a Hugging Face match (off by default — this flow is for
    /// models that aren't on HF).
    check_hf: bool,
}

impl ComposerState {
    fn license_spdx(&self) -> &'static str {
        LICENSE_OPTIONS
            .get(self.license_idx)
            .map(|(_, s)| *s)
            .unwrap_or("unknown")
    }

    /// Whether the chosen license permits public redistribution (gates "Publish
    /// to Explore"). Unknown => private link only.
    fn license_permits_public(&self) -> bool {
        noema_core::RedistributionClass::for_license(Some(self.license_spdx()))
            .allows_public_redistribution()
    }

    /// The live receiver's-eye title, e.g. `Mistral-7B-Instruct-v0.3 · Q4_K_M · GGUF`.
    fn preview(&self) -> String {
        let mut s = if self.title.trim().is_empty() {
            self.filename.clone()
        } else {
            self.title.trim().to_string()
        };
        if !self.quant.trim().is_empty() {
            s.push_str(" · ");
            s.push_str(self.quant.trim());
        }
        if let Some(f) = &self.format {
            s.push_str(" · ");
            s.push_str(&f.to_ascii_uppercase());
        }
        s
    }
}

/// Pre-select the license dropdown from a tag parsed out of the file (or the
/// existing manifest): exact spdx match first, then by family prefix, else
fn license_idx_for(spdx: Option<&str>) -> usize {
    let tag = spdx.map(|s| s.trim().to_lowercase()).unwrap_or_default();
    if tag.is_empty() || tag == "unknown" {
        return 0;
    }
    if let Some(i) = LICENSE_OPTIONS.iter().position(|(_, s)| *s == tag) {
        return i;
    }
    LICENSE_OPTIONS
        .iter()
        .position(|(_, s)| tag.starts_with(s) || s.starts_with(tag.as_str()))
        .unwrap_or(0)
}

struct App {
    engine: Option<Arc<Engine>>,
    init_error: Option<String>,
    rt: tokio::runtime::Runtime,
    egui_ctx: egui::Context,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    tab: Tab,
    logo: Option<egui::TextureHandle>,

    query: String,
    results: Vec<HfModel>,
    searching: bool,
    /// Set when the last Discover search failed, so we can show a retry block
    /// instead of silently falling back to the empty-state hero.
    last_search_error: Option<String>,
    /// One-shot: focus the Discover search field on the next frame (Cmd/Ctrl+F).
    focus_search: bool,
    /// Connection settings as applied at startup — to detect when a restart-only
    /// setting (tracker/mirror/proxy/seed) is dirty and prompt for a restart.
    applied_connection: ConnSnapshot,
    /// Best-effort memory budget for hardware-aware quant recommendation.
    mem_budget: u64,
    detail: Option<HfModelDetail>,
    loading_detail: bool,
    add_link_input: String,
    /// sha256 -> worldwide peer count (from the tracker) for the open model.
    worldwide_peers: HashMap<String, usize>,
    /// When the open model's peer counts were last sampled — so they auto-refresh
    /// while viewing and a remote delete/withdraw stops showing as a phantom peer.
    last_peer_check: Option<Instant>,

    network: Vec<noema_core::NetworkModel>,
    network_loading: bool,
    network_query: String,
    last_network_fetch: Option<Instant>,
    last_network_error: Option<String>,
    /// Live identity snapshot used to avoid redundant re-announces.
    applied_identity: (String, String),

    busy: bool,
    progress: Option<(f32, String)>,
    active: Option<ActiveDownload>,
    last_saved: Option<PathBuf>,
    /// A share-link / Explore fetch awaiting confirmation in the popup.
    pending_download: Option<PendingDownload>,
    pending_share_off: Option<PendingShareOff>,
    /// A single file's open-mesh share-off awaiting confirmation (peers are
    /// actively pulling that file, so stopping disconnects them).
    pending_file_share_off: Option<PendingFileShareOff>,
    /// The quant whose per-protocol routes popup is open, if any.
    quant_detail: Option<QuantDetail>,
    /// A Library model awaiting destructive-delete confirmation.
    pending_delete: Option<PendingDelete>,
    /// The "Send a model" composer, when open.
    composer: Option<ComposerState>,
    /// A share link to copy to the clipboard once the composer's import lands
    pending_share_link: Option<noema_core::ShareTarget>,

    // library / storage
    cached_sha256: HashSet<String>,
    cached_blake3: HashSet<String>,
    installed: Vec<InstalledModel>,
    cache: Vec<CacheBlobRow>,

    // settings / sharing
    settings: Settings,
    has_token: bool,
    token_input: String,
    show_token: bool,
    worldwide: Option<noema_core::WorldwideShare>,

    // stats / speed graph
    cumulative_dl: u64,
    dl_samples: VecDeque<f64>,
    ul_samples: VecDeque<f64>,
    cur_dl_bps: f64,
    cur_ul_bps: f64,
    last_sample: Instant,
    last_dl_mark: u64,
    last_iroh_ul_mark: u64,
    /// Bytes uploaded to peers this session (worldwide Iroh seeding).
    session_uploaded: u64,

    status: String,
    toasts: Vec<Toast>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (tx, rx) = std::sync::mpsc::channel();
        let root = noema_core::paths::default_root();
        let mut settings = load_settings(&root);
        // Modern theme (light or dark) — pure styling, no extra RAM (see
        // `apply_theme`). Applied once the persisted choice is known.
        apply_theme(&cc.egui_ctx, settings.theme.is_dark());
        let mut settings_dirty = false;
        if settings.device_id.trim().is_empty() {
            settings.device_id = noema_core::identity::new_device_id();
            settings_dirty = true;
        }
        if settings.device_name.trim().is_empty() {
            settings.device_name = noema_core::identity::default_device_name();
            settings_dirty = true;
        }
        let mut cfg = EngineConfig::new(root.clone());
        // Wire the worldwide P2P tracker so downloads can discover peers globally.
        if !settings.tracker_url.trim().is_empty() {
            cfg.tracker_url = Some(settings.tracker_url.trim().to_string());
        }
        // HF mirror: point both catalog search and weight downloads at the mirror.
        if settings.hf_mirror_enabled && !settings.hf_mirror_url.trim().is_empty() {
            cfg.transport.hf_endpoint = settings.hf_mirror_url.trim().to_string();
        }
        // Proxy ("VPN tunnel"): route the app's internet HTTP traffic through it.
        if settings.proxy_enabled && !settings.proxy_url.trim().is_empty() {
            cfg.transport.proxy = Some(settings.proxy_url.trim().to_string());
        }
        cfg.platform.huggingface_download = settings.allow_hf_download;
        cfg.max_download_connections = settings.download_connections.max(1) as usize;
        cfg.share_gated = settings.share_gated;
        let (engine, init_error) = match Engine::open(cfg) {
            Ok(e) => (Some(Arc::new(e)), None),
            Err(e) => (None, Some(format!("Couldn't open the local store: {e}"))),
        };

        let mut app = App {
            engine,
            init_error,
            rt,
            egui_ctx: cc.egui_ctx.clone(),
            tx,
            rx,
            tab: Tab::Discover,
            logo: load_logo_texture(&cc.egui_ctx),
            query: String::new(),
            results: Vec::new(),
            searching: false,
            last_search_error: None,
            focus_search: false,
            applied_connection: conn_snapshot(&settings),
            mem_budget: noema_core::platform::detect_memory_budget_bytes().unwrap_or(0),
            detail: None,
            loading_detail: false,
            add_link_input: String::new(),
            worldwide_peers: HashMap::new(),
            last_peer_check: None,
            network: Vec::new(),
            network_loading: false,
            network_query: String::new(),
            last_network_fetch: None,
            last_network_error: None,
            applied_identity: (
                settings.device_name.trim().to_string(),
                settings.group_code.trim().to_string(),
            ),
            busy: false,
            progress: None,
            active: None,
            last_saved: None,
            pending_download: None,
            pending_share_off: None,
            pending_file_share_off: None,
            quant_detail: None,
            pending_delete: None,
            composer: None,
            pending_share_link: None,
            cached_sha256: HashSet::new(),
            cached_blake3: HashSet::new(),
            installed: Vec::new(),
            cache: Vec::new(),
            settings,
            has_token: false,
            token_input: String::new(),
            show_token: false,
            worldwide: None,
            cumulative_dl: 0,
            dl_samples: VecDeque::with_capacity(128),
            ul_samples: VecDeque::with_capacity(128),
            cur_dl_bps: 0.0,
            cur_ul_bps: 0.0,
            last_sample: Instant::now(),
            last_dl_mark: 0,
            last_iroh_ul_mark: 0,
            session_uploaded: 0,
            status: "Search for a model to get started.".into(),
            toasts: Vec::new(),
        };
        if settings_dirty {
            app.save_settings();
        }
        app.refresh();
        app.apply_speed_cap();
        if app.settings.share_worldwide {
            app.start_worldwide();
        }
        app
    }

    fn start_worldwide(&mut self) {
        let Some(engine) = &self.engine else { return };
        if self.worldwide.is_some() {
            return;
        }
        let tracker = self.settings.tracker_url.trim().to_string();
        if tracker.is_empty() {
            self.settings.share_worldwide = false;
            self.status = "Set a tracker URL first (Settings).".into();
            return;
        }
        let eng = engine.clone();
        let identity = self.settings.identity();
        match self
            .rt
            .block_on(eng.start_worldwide_share(tracker, identity))
        {
            Ok(handle) => {
                self.status = "Sharing your models worldwide (P2P).".into();
                self.worldwide = Some(handle);
            }
            Err(e) => {
                self.settings.share_worldwide = false;
                self.status = format!("Couldn't start worldwide sharing: {e}");
            }
        }
    }

    /// Apply a changed device name / group code to the LIVE worldwide session
    fn apply_identity_live(&self) {
        if let Some(w) = &self.worldwide {
            w.set_identity(self.settings.identity());
            self.reannounce_worldwide();
        }
    }

    fn stop_worldwide(&mut self) {
        if let Some(w) = self.worldwide.take() {
            // Hard-disconnect: shut the router + close the QUIC endpoint so peers
            // currently pulling from us are severed now, not just left to time out.
            self.rt.block_on(w.stop());
            // Tell the tracker we're gone so our shares leave Explore immediately
            // instead of lingering until their TTL.
            self.withdraw_from_tracker(Vec::new());
        }
    }

    /// After worldwide sharing stops, drop "mine"/own rows from the cached Explore
    /// list so the UI doesn't keep showing models only we were seeding.
    fn clear_network_after_stop(&mut self) {
        for row in &mut self.network {
            row.mine = false;
        }
        self.network.retain(|row| row.peers > 0);
        self.last_network_fetch = None;
    }

    /// Best-effort un-announce of content from the worldwide tracker, off the UI
    /// thread — after deleting a model or turning a share off, so it disappears
    /// from Explore right away rather than waiting out its 30-minute TTL. An empty
    /// list withdraws *everything* this device announced. No-op until worldwide
    /// sharing has run (that's when the engine knows our NodeId).
    fn withdraw_from_tracker(&self, blake3s: Vec<String>) {
        let Some(engine) = &self.engine else { return };
        if self.settings.tracker_url.trim().is_empty() {
            return;
        }
        let eng = engine.clone();
        let tx = self.tx.clone();
        let ctx = self.egui_ctx.clone();
        self.rt.spawn(async move {
            eng.withdraw_from_tracker(&blake3s).await;
            let _ = tx.send(Msg::PeerStateChanged);
            ctx.request_repaint();
        });
    }

    /// Stop serving a blob over Iroh right now **and** hard-disconnect any peer
    /// mid-transfer of it — drops it from the live seeder's store so no new pull
    /// can start, then severs the connections already pulling it so an in-flight
    /// transfer is cut rather than left to finish. Runs off the UI thread. Pairs
    /// with `withdraw_from_tracker`: one stops discovery, this stops the upload.
    fn unseed_blob(&self, blake3: &str) {
        let Some(w) = &self.worldwide else { return };
        let handle = w.seeder_handle();
        let b3 = blake3.to_string();
        self.rt.spawn(async move {
            handle.unseed_and_disconnect(&b3).await;
        });
    }

    /// Actually turn a single file's open-mesh share off: flip the DB flag, drop it
    /// from Explore (withdraw), and stop serving it + hard-disconnect any peer
    /// mid-transfer (unseed_blob). Called either immediately (no active peers) or
    /// after the user confirms the stop-while-active dialog.
    fn apply_share_off(&mut self, blake3: &str, sha256: &str) {
        let Some(engine) = self.engine.clone() else {
            return;
        };
        match engine.set_model_shared(blake3, sha256, false) {
            Ok(()) => {
                self.status = "Stopped sharing — removed from the network.".into();
                // Two halves: withdraw from the tracker (stop discovery) AND unseed
                // the blob from our Iroh node, severing peers already pulling it —
                // otherwise a peer that already knows the hash keeps downloading.
                self.forget_network_share(blake3);
                self.withdraw_from_tracker(vec![blake3.to_string()]);
                self.unseed_blob(blake3);
                self.refresh();
                self.refresh_worldwide();
            }
            Err(e) => self.status = format!("Couldn't update sharing: {e}"),
        }
    }

    fn refresh(&mut self) {
        if let Some(engine) = &self.engine {
            if let Ok(report) = engine.reconcile() {
                if !report.removed_blake3s.is_empty() {
                    self.withdraw_from_tracker(report.removed_blake3s);
                }
            }
            self.installed = engine.installed_models().unwrap_or_default();
            self.cache = engine.list_cache().unwrap_or_default();
            self.cached_sha256 = self.cache.iter().map(|b| b.sha256.clone()).collect();
            self.cached_blake3 = self.cache.iter().map(|b| b.blake3.clone()).collect();
            self.has_token = engine
                .token_status(&noema_core::manifest::Source::Huggingface {
                    repo_id: String::new(),
                    revision: String::new(),
                    path: String::new(),
                    auth: noema_core::manifest::AuthPolicy::Token,
                })
                .unwrap_or(false);
        }
    }

    fn refresh_visible_peer_counts(&mut self) {
        if let Some(detail) = self.detail.clone() {
            self.worldwide_peers.clear();
            self.start_seeder_check(&detail);
        }
    }

    fn refresh_after_peer_state_change(&mut self) {
        self.refresh();
        self.refresh_visible_peer_counts();
        self.last_network_fetch = None;
        if self.tab == Tab::Network {
            self.start_network_fetch();
        }
    }

    fn forget_network_share(&mut self, blake3: &str) {
        for row in &mut self.network {
            if row.blake3 == blake3 {
                row.mine = false;
            }
        }
        self.network
            .retain(|row| row.blake3 != blake3 || row.peers > 0);
        self.last_network_fetch = None;
    }

    fn apply_speed_cap(&self) {
        if let Some(engine) = &self.engine {
            let bps = self.settings.download_cap_mbps as u64 * 125_000; // Mbps -> bytes/s
            engine.rate_limit().set_bps(bps);
        }
    }

    fn start_search(&mut self) {
        let Some(engine) = &self.engine else { return };
        let q = self.query.trim().to_string();
        if q.is_empty() {
            return;
        }
        self.searching = true;
        self.last_search_error = None;
        self.detail = None;
        self.status = format!("Searching Hugging Face for “{q}”…");
        let (eng, tx, ctx) = (engine.clone(), self.tx.clone(), self.egui_ctx.clone());
        self.rt.spawn(async move {
            let res = eng.hf_search(&q, 30).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::Models(res));
            ctx.request_repaint();
        });
    }

    fn start_open_model(&mut self, id: String) {
        let Some(engine) = &self.engine else { return };
        self.loading_detail = true;
        self.detail = None;
        self.worldwide_peers.clear();
        self.status = format!("Loading {id}…");
        let (eng, tx, ctx) = (engine.clone(), self.tx.clone(), self.egui_ctx.clone());
        self.rt.spawn(async move {
            let res = eng.hf_model_detail(&id).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::Detail(res));
            ctx.request_repaint();
        });
    }

    fn start_seeder_check(&mut self, detail: &HfModelDetail) {
        let Some(engine) = &self.engine else { return };
        let shas: Vec<String> = detail
            .weight_files()
            .iter()
            .filter_map(|f| f.sha256.clone())
            .collect();
        if shas.is_empty() {
            return;
        }
        self.last_peer_check = Some(Instant::now());
        // Worldwide peers via the tracker — the "anyone seeding this?" signal.
        // Fan the per-quant lookups out concurrently so the slowest single
        // request bounds latency, not the sum.
        let (eng2, tx2, ctx2) = (engine.clone(), self.tx.clone(), self.egui_ctx.clone());
        self.rt.spawn(async move {
            let handles: Vec<_> = shas
                .into_iter()
                .map(|sha| {
                    let e = eng2.clone();
                    tokio::spawn(async move {
                        let n = e.worldwide_peers(&sha).await;
                        (sha, n)
                    })
                })
                .collect();
            let mut map: HashMap<String, usize> = HashMap::new();
            for h in handles {
                if let Ok((sha, n)) = h.await {
                    map.insert(sha, n);
                }
            }
            let _ = tx2.send(Msg::WorldwidePeers(map));
            ctx2.request_repaint();
        });
    }

    fn start_download(&mut self, rfilename: String) {
        let (Some(engine), Some(detail)) = (&self.engine, &self.detail) else {
            return;
        };
        if self.busy {
            self.status = "A download is already running…".into();
            return;
        }
        // Resolve what to fetch. A GGUF quant can be split across several shard
        // files; clicking any of its rows fetches the whole quant as one model.
        let quant = detail
            .gguf_quants()
            .into_iter()
            .find(|q| q.files.iter().any(|f| f.rfilename == rfilename));
        let (imported, name, size, sha_for_peers) = if let Some(q) = quant {
            let imported = match engine.hf_import_gguf_quant(detail, &q.files) {
                Ok(r) => r,
                Err(e) => {
                    self.status = format!("Couldn't prepare download: {e}");
                    return;
                }
            };
            let sha = q
                .files
                .iter()
                .find(|f| f.rfilename == rfilename)
                .or_else(|| q.files.first())
                .and_then(|f| f.sha256.clone());
            (
                imported,
                format!("{} · {}", detail.name(), q.label),
                q.total_size(),
                sha,
            )
        } else {
            let Some(file) = detail
                .weight_files()
                .into_iter()
                .find(|f| f.rfilename == rfilename)
                .cloned()
            else {
                return;
            };
            let imported = match engine.hf_import_file(detail, &file) {
                Ok(r) => r,
                Err(e) => {
                    self.status = format!("Couldn't prepare download: {e}");
                    return;
                }
            };
            (
                imported,
                format!("{} · {}", detail.name(), file.variant_label()),
                file.size,
                file.sha256.clone(),
            )
        };
        if !imported.policy.allowed {
            self.status = format!("Can't download: {}", imported.policy.reason);
            return;
        }
        let wpeers = sha_for_peers
            .as_ref()
            .and_then(|s| self.worldwide_peers.get(s))
            .copied()
            .unwrap_or(0);
        self.busy = true;
        self.progress = Some((0.0, "starting…".into()));
        self.active = Some(active_download(name.clone(), size));
        self.status = if wpeers > 0 {
            format!("Downloading {name} — {wpeers} worldwide peer(s) available…")
        } else if hf_download_live(self) {
            format!("Downloading {name} — Hugging Face is available as a last resort…")
        } else {
            format!("Looking for peers for {name} — Hugging Face downloads are off…")
        };

        let eng = engine.clone();
        let id = imported.manifest_id;
        let (tx_p, tx_d, ctx) = (self.tx.clone(), self.tx.clone(), self.egui_ctx.clone());
        self.rt.spawn(async move {
            let ctx_p = ctx.clone();
            let progress: Progress = Arc::new(move |p: DownloadProgress| {
                let _ = tx_p.send(Msg::Progress {
                    done: p.bytes_done,
                    total: p.bytes_total,
                    phase: p.phase.to_string(),
                    source: p.source_id.clone(),
                    failover_reason: p.failover_reason.clone(),
                    effective_start: p.effective_start,
                });
                ctx_p.request_repaint();
            });
            let res = eng
                .download(&id, Some(progress))
                .await
                .map(|_| (id.clone(), size));
            let _ = tx_d.send(Msg::Done(DownloadEnd::from_result(res)));
            ctx.request_repaint();
        });
    }

    /// One-click download of the whole sharded safetensors/MLX model: every shard
    /// together with its config/tokenizer sidecars, as a single multi-artifact
    /// manifest. The progress bar is driven by the *sum* of per-artifact bytes so
    /// it advances smoothly across files instead of resetting at each one.
    fn start_download_bundle(&mut self) {
        let (Some(engine), Some(detail)) = (&self.engine, &self.detail) else {
            return;
        };
        if self.busy {
            self.status = "A download is already running…".into();
            return;
        }
        if !detail.has_safetensors_bundle() {
            return;
        }
        let shard_shas: Vec<String> = detail
            .safetensors_shards()
            .iter()
            .filter_map(|f| f.sha256.clone())
            .collect();
        let shard_count = shard_shas.len();
        let iroh_covered = shard_shas
            .iter()
            .filter(|sha| self.worldwide_peers.get(*sha).copied().unwrap_or(0) > 0)
            .count();
        let imported = match engine.hf_import_bundle(detail) {
            Ok(r) => r,
            Err(e) => {
                self.status = format!("Couldn't prepare download: {e}");
                return;
            }
        };
        if !imported.policy.allowed {
            self.status = format!("Can't download: {}", imported.policy.reason);
            return;
        }
        let total = detail.bundle_total_size();
        let name = format!("{} · {}", detail.name(), detail.bundle_variant_label());
        self.busy = true;
        self.progress = Some((0.0, "starting…".into()));
        self.active = Some(active_download(name.clone(), total));
        self.status = if shard_count > 0 && iroh_covered == shard_count {
            format!("Downloading {name} — worldwide peers cover every file…")
        } else if iroh_covered > 0 {
            format!(
                "Downloading {name} — peer coverage {iroh_covered}/{shard_count} file(s); Atlas will fill gaps from the next eligible route…"
            )
        } else if hf_download_live(self) {
            format!("Downloading {name} — Hugging Face is available as a last resort…")
        } else {
            format!("Looking for peers for {name} — Hugging Face downloads are off…")
        };

        let eng = engine.clone();
        let id = imported.manifest_id;
        let (tx_p, tx_d, ctx) = (self.tx.clone(), self.tx.clone(), self.egui_ctx.clone());
        self.rt.spawn(async move {
            let ctx_p = ctx.clone();
            let per_artifact: Arc<Mutex<HashMap<String, u64>>> =
                Arc::new(Mutex::new(HashMap::new()));
            let progress: Progress = Arc::new(move |p: DownloadProgress| {
                let done = {
                    let mut m = per_artifact.lock().unwrap();
                    m.insert(p.artifact_path.clone(), p.bytes_done);
                    m.values().sum::<u64>()
                };
                let _ = tx_p.send(Msg::Progress {
                    done,
                    total,
                    phase: p.phase.to_string(),
                    source: p.source_id.clone(),
                    failover_reason: p.failover_reason.clone(),
                    effective_start: p.effective_start,
                });
                ctx_p.request_repaint();
            });
            let res = eng
                .download(&id, Some(progress))
                .await
                .map(|_| (id.clone(), total));
            let _ = tx_d.send(Msg::Done(DownloadEnd::from_result(res)));
            ctx.request_repaint();
        });
    }

    /// Pick an already-downloaded model file and import it (matched to HF by hash).
    /// Open the file picker, then the "Send a model" composer.
    fn open_composer_picker(&mut self) {
        if self.busy || self.composer.is_some() {
            return;
        }
        let picked = rfd::FileDialog::new()
            .add_filter("Models", &["gguf", "safetensors", "bin"])
            .set_title("Choose a model file to share")
            .pick_file();
        if let Some(path) = picked {
            self.open_composer_path(path);
        }
    }

    /// Open the composer for a freshly-picked / dropped file: sniff its header +
    /// filename so the fields are pre-filled before the user sees the dialog.
    fn open_composer_path(&mut self, path: PathBuf) {
        if self.busy || self.composer.is_some() {
            return;
        }
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("model.gguf")
            .to_string();
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let meta = noema_core::read_file_meta(&path);
        let parsed = noema_core::parse_model(&filename, &meta);
        self.composer = Some(ComposerState {
            path,
            filename,
            size,
            format: parsed.format.clone(),
            architecture: parsed.architecture.clone(),
            edit_manifest_id: None,
            blake3: String::new(),
            sha256: String::new(),
            title_from_file: meta.name.is_some(),
            quant_from_file: meta.quantization.is_some(),
            title: parsed.title.clone(),
            family: parsed.family.clone().unwrap_or_default(),
            quant: parsed.quant.clone().unwrap_or_default(),
            license_idx: license_idx_for(parsed.license.as_deref()),
            description: String::new(),
            origin: parsed.source_url.clone().unwrap_or_default(),
            publish: false,
            check_hf: false,
        });
        self.status = "Title your model, then send it.".into();
    }

    /// Open the composer pre-filled from a model already in the Library, to
    /// retitle / relicense / describe / share it after the fact.
    fn open_composer_edit(&mut self, manifest_id: &str) {
        if self.busy || self.composer.is_some() {
            return;
        }
        let Some(m) = self.installed.iter().find(|m| m.manifest_id == manifest_id) else {
            return;
        };
        self.composer = Some(ComposerState {
            path: PathBuf::new(),
            filename: m.name.clone(),
            size: m.size_bytes,
            format: None,
            architecture: None,
            edit_manifest_id: Some(manifest_id.to_string()),
            blake3: m.blake3.clone(),
            sha256: m.sha256.clone(),
            title_from_file: false,
            quant_from_file: false,
            title: m.name.clone(),
            family: m.family.clone().unwrap_or_default(),
            quant: m.quant.clone().unwrap_or_default(),
            license_idx: license_idx_for(Some(&m.license)),
            description: m.description.clone().unwrap_or_default(),
            origin: m.origin.clone().unwrap_or_default(),
            publish: m.shareable,
            check_hf: false,
        });
        self.status = "Edit this model's details.".into();
    }

    /// Build a `LocalShareMeta` from the composer's current fields.
    fn composer_meta(c: &ComposerState) -> noema_core::LocalShareMeta {
        let some = |s: &str| {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        };
        noema_core::LocalShareMeta {
            title: some(&c.title),
            family: some(&c.family),
            quant: some(&c.quant),
            architecture: c.architecture.clone(),
            license: Some(c.license_spdx().to_string()),
            description: some(&c.description),
            origin_url: some(&c.origin),
            skip_hf_match: !c.check_hf,
            publish: c.publish,
        }
    }

    /// Build the share link the receiver will see (sans content ids, filled in
    /// once hashing completes for an import; immediate for an edit).
    fn composer_link(c: &ComposerState) -> noema_core::ShareTarget {
        noema_core::ShareTarget {
            name: c.filename.clone(),
            size: c.size,
            sha256: c.sha256.clone(),
            blake3: c.blake3.clone(),
            license: c.license_spdx().to_string(),
            title: c.title.trim().to_string(),
            family: c.family.trim().to_string(),
            quant: c.quant.trim().to_string(),
            desc: c.description.trim().to_string(),
            origin: c.origin.trim().to_string(),
        }
    }

    /// Confirm the composer: import (or, in edit mode, update) the model with the
    /// chosen metadata, then surface a share link.
    fn confirm_composer(&mut self) {
        let Some(c) = self.composer.take() else {
            return;
        };
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let meta = Self::composer_meta(&c);
        let mut link = Self::composer_link(&c);

        if let Some(manifest_id) = c.edit_manifest_id.clone() {
            // Edit mode: update the manifest in place (synchronous, fast).
            match engine.rename_model(&manifest_id, &meta) {
                Ok(()) => {
                    let copied = link.encode();
                    self.egui_ctx.output_mut(|o| o.copied_text = copied);
                    self.status = format!(
                        "✓ Updated “{}” — share link copied to clipboard.",
                        link.display_title()
                    );
                    if meta.publish {
                        self.refresh_worldwide();
                    } else if !c.blake3.is_empty() {
                        self.forget_network_share(&c.blake3);
                        self.withdraw_from_tracker(vec![c.blake3.clone()]);
                    }
                    self.refresh();
                }
                Err(e) => self.status = format!("Couldn't update: {e}"),
            }
            return;
        }

        // Import mode: hash + import off the UI thread; the link is completed and
        // copied once the content ids are known (in the Imported handler).
        self.busy = true;
        self.progress = Some((0.0, "Hashing your model…".into()));
        self.status = format!("Preparing “{}” to share…", link.display_title());
        link.sha256.clear();
        link.blake3.clear();
        self.pending_share_link = Some(link);
        let path = c.path.clone();
        let (eng, tx, ctx) = (engine, self.tx.clone(), self.egui_ctx.clone());
        self.rt.spawn(async move {
            let res = eng
                .import_local_file_with_meta(&path, meta)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Imported(res));
            ctx.request_repaint();
        });
    }

    /// Re-seed + re-announce the currently-shareable models on the running
    /// worldwide session (after an import or a per-model opt-in), off the UI
    /// thread so a large file's hashing doesn't freeze the window.
    fn refresh_worldwide(&self) {
        let (Some(engine), Some(w)) = (&self.engine, &self.worldwide) else {
            return;
        };
        let seeder = w.seeder_handle();
        let eng = engine.clone();
        let tracker = self.settings.tracker_url.trim().to_string();
        if tracker.is_empty() {
            return;
        }
        self.rt.spawn(async move {
            let items = eng.share_announce_items().unwrap_or_default();
            seeder.announce(&items, &tracker).await;
        });
    }

    /// Re-announce already-seeded models with the current identity (after a
    /// device-name / group change) — no re-hash, off the UI thread.
    fn reannounce_worldwide(&self) {
        let (Some(engine), Some(w)) = (&self.engine, &self.worldwide) else {
            return;
        };
        let seeder = w.seeder_handle();
        let eng = engine.clone();
        let tracker = self.settings.tracker_url.trim().to_string();
        if tracker.is_empty() {
            return;
        }
        self.rt.spawn(async move {
            let items: Vec<_> = eng
                .share_announce_items()
                .unwrap_or_default()
                .into_iter()
                .map(|(_, it)| it)
                .collect();
            seeder.reannounce(&items, &tracker).await;
        });
    }

    /// Fetch the worldwide catalog for the Network tab (off the UI thread).
    fn start_network_fetch(&mut self) {
        let Some(engine) = &self.engine else { return };
        if self.network_loading {
            return;
        }
        self.network_loading = true;
        self.last_network_fetch = Some(Instant::now());
        let eng = engine.clone();
        let q = self.network_query.trim().to_string();
        let group = noema_core::identity::group_id(&self.settings.group_code);
        let (tx, ctx) = (self.tx.clone(), self.egui_ctx.clone());
        self.rt.spawn(async move {
            let res = eng
                .network_catalog(&q, group)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Catalog(res));
            ctx.request_repaint();
        });
    }

    /// One-click download of a model discovered on the Network (Explore) tab.
    fn start_add_from_network(&mut self, m: &noema_core::NetworkModel) {
        let target = noema_core::ShareTarget {
            name: m.name.clone(),
            size: m.size,
            sha256: m.sha256.clone(),
            blake3: m.blake3.clone(),
            license: m.license.clone(),
            quant: m.quant.clone(),
            ..Default::default()
        };
        self.request_download(target, Some(m.peers));
    }

    /// Fetch a model from a pasted Content ID / share link (Discover tab). Builds
    /// a verifiable manifest and downloads it — over Iroh from a worldwide peer
    /// when the tracker can resolve the content id.
    fn start_add_by_link(&mut self) {
        let token = self.add_link_input.trim().to_string();
        if token.is_empty() {
            return;
        }
        let target = match noema_core::ShareTarget::decode(&token) {
            Ok(t) => t,
            Err(e) => {
                self.status = format!("Couldn't read that link: {e}");
                return;
            }
        };
        self.add_link_input.clear();
        let peers = self.worldwide_peers.get(&target.sha256).copied();
        self.request_download(target, peers);
    }

    /// Gate a share-link / Explore fetch behind a confirmation popup, so a single
    /// click doesn't silently kick off a multi-GB download. Power users can turn
    /// the popup off in Settings, which downloads immediately instead.
    fn request_download(&mut self, target: noema_core::ShareTarget, peers: Option<usize>) {
        // Already in the Library? Don't "re-download" content you already have
        // (e.g. pasting a link to one of your own models) — just point there.
        if self.have_content(&target) {
            let name = if target.name.trim().is_empty() {
                "That model".to_string()
            } else {
                format!("“{}”", target.name.trim())
            };
            self.status = format!("✓ {name} is already in your Library.");
            self.tab = Tab::Library;
            return;
        }
        if self.busy {
            self.status = "A download is already running…".into();
            return;
        }
        if self.settings.skip_download_confirm {
            self.download_share_target(target);
        } else {
            self.pending_download = Some(PendingDownload { target, peers });
        }
    }

    /// Whether this exact content (by sha256 or blake3) is already cached locally.
    fn have_content(&self, target: &noema_core::ShareTarget) -> bool {
        (!target.sha256.is_empty() && self.cached_sha256.contains(&target.sha256))
            || (!target.blake3.is_empty() && self.cached_blake3.contains(&target.blake3))
    }

    /// Download a resolved share target over P2P (verified by content hash).
    fn download_share_target(&mut self, target: noema_core::ShareTarget) {
        let Some(engine) = &self.engine else { return };
        if self.busy {
            self.status = "A download is already running…".into();
            return;
        }
        let name = if target.name.trim().is_empty() {
            "shared model".to_string()
        } else {
            target.name.clone()
        };
        self.busy = true;
        self.progress = Some((0.0, "finding worldwide peers…".into()));
        self.active = Some(active_download(name.clone(), target.size));
        self.status = format!("Fetching {name} from worldwide peers…");

        let eng = engine.clone();
        let (tx_p, tx_d, ctx) = (self.tx.clone(), self.tx.clone(), self.egui_ctx.clone());
        self.rt.spawn(async move {
            let ctx_p = ctx.clone();
            let progress: Progress = Arc::new(move |p: DownloadProgress| {
                let _ = tx_p.send(Msg::Progress {
                    done: p.bytes_done,
                    total: p.bytes_total,
                    phase: p.phase.to_string(),
                    source: p.source_id.clone(),
                    failover_reason: p.failover_reason.clone(),
                    effective_start: p.effective_start,
                });
                ctx_p.request_repaint();
            });
            let res = eng
                .add_by_content(target, Some(progress))
                .await
                .map(|o| (o.manifest_id, 0u64));
            let _ = tx_d.send(Msg::Done(DownloadEnd::from_result(res)));
            ctx.request_repaint();
        });
    }

    fn poll(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Imported(res) => {
                    self.busy = false;
                    self.progress = None;
                    let link = self.pending_share_link.take();
                    match res {
                        Ok(o) => {
                            if let Some(mut t) = link {
                                // The "Send a model" composer just landed: complete
                                // the share link with the now-known content ids and
                                // copy it so the user can paste it straight away.
                                t.sha256 = o.sha256.clone();
                                t.blake3 = o.blake3.clone();
                                if t.size == 0 {
                                    t.size = o.size_bytes;
                                }
                                let token = t.encode();
                                self.egui_ctx.output_mut(|out| out.copied_text = token);
                                self.status = if o.shareable {
                                    format!(
                                        "✓ Sharing “{}” — link copied. Anyone you send it to can fetch & verify it.",
                                        t.display_title()
                                    )
                                } else {
                                    format!(
                                        "✓ “{}” is ready — link copied. Keep Atlas open so peers can fetch it.",
                                        t.display_title()
                                    )
                                };
                            } else if o.matched {
                                self.status = format!(
                                    "Imported {} — matched on Hugging Face{}",
                                    o.model_name,
                                    if o.shareable {
                                        ", shareable to peers"
                                    } else {
                                        " (download-only license)"
                                    }
                                );
                            } else {
                                self.status = format!(
                                    "Imported {}. Give it a title and license under Library to share it.",
                                    o.model_name
                                );
                            }
                            self.tab = Tab::Library;
                        }
                        Err(e) => self.status = friendly_error(&e),
                    }
                    self.refresh();
                    // Publish the freshly-imported model to peers right away if
                    // it's eligible (permissive license) or already opted-in.
                    self.refresh_worldwide();
                }
                Msg::Models(res) => {
                    self.searching = false;
                    match res {
                        Ok(mut models) => {
                            self.last_search_error = None;
                            self.status = if models.is_empty() {
                                "No models found — try a different search.".into()
                            } else {
                                format!("{} result(s)", models.len())
                            };
                            // Atlas is GGUF-first: surface repos that actually
                            // carry GGUF quants ahead of base/safetensors-only
                            // ones, without hiding the latter. Stable, so HF's
                            // relevance ranking is preserved within each group.
                            models.sort_by_key(|m| !m.has_gguf());
                            self.results = models;
                        }
                        Err(e) => {
                            self.last_search_error = Some(friendly_error(&e));
                            self.status = friendly_error(&e);
                        }
                    }
                }
                Msg::Detail(res) => {
                    self.loading_detail = false;
                    match res {
                        Ok(detail) => {
                            self.start_seeder_check(&detail);
                            self.status = format!("{} — choose a version", detail.name());
                            self.detail = Some(detail);
                        }
                        Err(e) => self.status = friendly_error(&e),
                    }
                }
                Msg::WorldwidePeers(map) => self.worldwide_peers = map,
                Msg::Progress {
                    done,
                    total,
                    phase,
                    source,
                    failover_reason,
                    effective_start,
                } => {
                    let mut toast = None;
                    // The local "verifying" re-read of an already-downloaded
                    // in-`open()` blob (Iroh) is not a fresh download: hold the bar
                    // at 100% and show "Verifying…" instead of letting it reset.
                    // `fold_download_progress` already returns 0 for this phase, so
                    // it never reaches the session/throughput counters.
                    let is_verifying = phase == "verifying";
                    if let Some(a) = &mut self.active {
                        // Count only *newly downloaded* bytes into the session total
                        // that drives the throughput graph. Resumes, the pre-transfer
                        // phases, and the in-`open()` transports' disk-speed verify
                        // sweep all fold to zero here, so they never register as
                        // instant spikes; the monotonic guard still covers genuine
                        let delta = fold_download_progress(
                            &mut a.prev_done,
                            &mut a.dl_baselined,
                            done,
                            &phase,
                            effective_start,
                        );
                        self.cumulative_dl += delta;
                        if is_verifying {
                            a.verifying = true;
                            a.verify_done = done;
                            // Leave a.done/a.total at the completed-download values
                            // so the progress bar stays full.
                        } else {
                            a.verifying = false;
                            a.done = done;
                            a.total = total;
                        }
                        toast = note_route_progress(
                            a,
                            source.as_deref(),
                            &phase,
                            failover_reason,
                            effective_start,
                        );
                        // Attribute the newly downloaded bytes to the reporting
                        // source for the per-source breakdown.
                        if delta > 0 {
                            if let Some(sid) = &source {
                                *a.by_source.entry(sid.clone()).or_insert(0) += delta;
                            }
                        }
                    }
                    if let Some(text) = toast {
                        self.push_toast(text, ToastKind::Info);
                    }
                    let (frac, label) = if is_verifying {
                        (
                            1.0,
                            format!("Verifying… {} / {}", human(done), human(total)),
                        )
                    } else {
                        let frac = if total > 0 {
                            done as f32 / total as f32
                        } else {
                            0.0
                        };
                        (
                            frac,
                            format!("{phase} · {} / {}", human(done), human(total)),
                        )
                    };
                    self.progress = Some((frac, label));
                }
                Msg::Done(end) => {
                    self.busy = false;
                    self.progress = None;
                    match end {
                        DownloadEnd::Ok(manifest_id, _bytes) => {
                            self.on_download_complete(&manifest_id)
                        }
                        DownloadEnd::Stopped => {
                            self.status = "Download stopped — partial progress discarded.".into();
                            self.push_toast(
                                "Download stopped — progress discarded",
                                ToastKind::Warning,
                            );
                        }
                        DownloadEnd::Paused => {
                            self.status =
                                "Download paused — progress saved; download again to resume."
                                    .into();
                            self.push_toast("Download paused — progress saved", ToastKind::Info);
                        }
                        DownloadEnd::Failed(e) => {
                            let msg = friendly_error(&e);
                            self.status = msg.clone();
                            self.push_toast(msg, ToastKind::Error);
                        }
                    }
                    self.active = None;
                    self.cur_dl_bps = 0.0;
                    self.refresh();
                    // A freshly-downloaded model becomes live to peers
                    // immediately (not after the 10-min background loop).
                    self.refresh_worldwide();
                }
                Msg::Catalog(res) => {
                    self.network_loading = false;
                    match res {
                        Ok(rows) => {
                            self.network = rows;
                            self.last_network_error = None;
                        }
                        Err(e) => {
                            self.last_network_error = Some(e.clone());
                            self.status = format!("Couldn't load the network: {e}");
                        }
                    }
                }
                Msg::PeerStateChanged => self.refresh_after_peer_state_change(),
            }
        }
    }

    /// Sample download/upload speed at a steady cadence for the graphs.
    fn sample_speeds(&mut self) {
        let dt = self.last_sample.elapsed().as_secs_f64();
        if dt < 0.5 {
            return;
        }
        self.last_sample = Instant::now();

        let dl_delta = self.cumulative_dl.saturating_sub(self.last_dl_mark);
        self.last_dl_mark = self.cumulative_dl;
        self.cur_dl_bps = dl_delta as f64 / dt;
        push_cap(&mut self.dl_samples, self.cur_dl_bps, 120);

        // Upload speed comes from the worldwide Iroh seeder's byte counter.
        let iroh_uploaded = self
            .worldwide
            .as_ref()
            .map(|w| w.metrics().uploaded())
            .unwrap_or(0);
        let ul_delta = counter_delta(iroh_uploaded, &mut self.last_iroh_ul_mark);
        self.session_uploaded = self.session_uploaded.saturating_add(ul_delta);
        self.cur_ul_bps = ul_delta as f64 / dt;
        push_cap(&mut self.ul_samples, self.cur_ul_bps, 120);
    }

    fn push_toast(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.toasts.push(Toast {
            text: text.into(),
            kind,
            shown_at: Instant::now(),
        });
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
    }

    fn on_download_complete(&mut self, manifest_id: &str) {
        let name = self
            .active
            .as_ref()
            .map(|a| a.name.clone())
            .unwrap_or_default();
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let dir = PathBuf::from(&self.settings.models_dir);
        match engine.materialize_install(manifest_id, &dir) {
            Ok(_) => {
                self.last_saved = Some(dir.clone());
                let provenance = self
                    .active
                    .as_ref()
                    .map(route_summary)
                    .filter(|s| !s.is_empty());
                self.status = if let Some(p) = provenance {
                    format!("Downloaded {name} — {p}; saved to {}", dir.display())
                } else {
                    format!("Downloaded {name} — saved to {}", dir.display())
                };
                self.push_toast(format!("Downloaded {name} — verified"), ToastKind::Success);
            }
            Err(e) => self.status = format!("Downloaded {name}, but saving failed: {e}"),
        }
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::Search => self.start_search(),
            Action::Suggest(q) => {
                self.query = q;
                self.start_search();
            }
            Action::OpenModel(id) => self.start_open_model(id),
            Action::Back => {
                self.detail = None;
                self.loading_detail = false;
                self.status = format!("{} result(s)", self.results.len());
            }
            Action::Download(f) => self.start_download(f),
            Action::DownloadBundle => self.start_download_bundle(),
            Action::PauseDownload => {
                if let Some(engine) = &self.engine {
                    engine.request_pause();
                    self.status = "Pausing download…".into();
                }
            }
            Action::StopDownload => {
                if let Some(engine) = &self.engine {
                    engine.request_stop();
                    self.status = "Stopping download…".into();
                }
            }
            Action::OpenComposer => self.open_composer_picker(),
            Action::ComposerConfirm => self.confirm_composer(),
            Action::ComposerCancel => {
                self.composer = None;
                self.status = "Cancelled.".into();
            }
            Action::EditModel(id) => self.open_composer_edit(&id),
            Action::Reveal(p) => reveal(&p),
            Action::SaveToken => {
                if let Some(engine) = &self.engine {
                    let t = self.token_input.trim();
                    if !t.is_empty() {
                        match engine.set_token("huggingface", t) {
                            Ok(()) => {
                                self.has_token = true;
                                self.token_input.clear();
                                self.show_token = false;
                                self.status = "Hugging Face token saved.".into();
                            }
                            Err(e) => self.status = format!("Couldn't save token: {e}"),
                        }
                    }
                }
            }
            Action::ToggleWorldwide => {
                if self.settings.share_worldwide {
                    // Turning OFF. If peers are actively pulling from us, confirm
                    // first — stopping hard-disconnects them mid-download.
                    let active = self
                        .worldwide
                        .as_ref()
                        .map(|w| w.active_uploads())
                        .unwrap_or(0);
                    if active > 0 {
                        self.pending_share_off = Some(PendingShareOff {
                            active_uploads: active,
                        });
                    } else {
                        self.settings.share_worldwide = false;
                        self.stop_worldwide();
                        self.clear_network_after_stop();
                        self.status = "Worldwide sharing stopped.".into();
                        self.save_settings();
                    }
                } else {
                    self.settings.share_worldwide = true;
                    self.start_worldwide();
                    self.save_settings();
                }
            }
            Action::ConfirmStopWorldwide => {
                self.pending_share_off = None;
                self.settings.share_worldwide = false;
                self.stop_worldwide();
                self.clear_network_after_stop();
                self.status = "Worldwide sharing stopped — peers disconnected.".into();
                self.push_toast("Stopped sharing — peers disconnected", ToastKind::Warning);
                self.save_settings();
            }
            Action::CancelStopWorldwide => {
                self.pending_share_off = None;
                self.status = "Still sharing worldwide.".into();
            }
            Action::OpenQuantDetail(q) => {
                if let Some(detail) = self.detail.clone() {
                    if self.last_peer_check.is_none() {
                        self.start_seeder_check(&detail);
                    }
                }
                self.quant_detail = Some(q);
            }
            Action::CloseQuantDetail => self.quant_detail = None,
            Action::ShareModel { blake3, sha256, on } => {
                if on {
                    // Turning a file's share ON: announce + serve it right away.
                    if let Some(engine) = &self.engine {
                        match engine.set_model_shared(&blake3, &sha256, true) {
                            Ok(()) => {
                                self.status = "Sharing this model with everyone.".into();
                                self.last_network_fetch = None;
                                self.refresh();
                                self.refresh_worldwide();
                            }
                            Err(e) => self.status = format!("Couldn't update sharing: {e}"),
                        }
                    }
                } else {
                    // Turning a file's share OFF. If peers are mid-transfer of THIS
                    // file, confirm first — stopping severs them — and defer the
                    // actual change until they confirm. Otherwise stop immediately.
                    let active = self
                        .worldwide
                        .as_ref()
                        .map(|w| w.active_uploads_for(&blake3))
                        .unwrap_or(0);
                    if active > 0 {
                        let name = self
                            .installed
                            .iter()
                            .find(|m| m.blake3 == blake3)
                            .map(|m| m.name.clone())
                            .unwrap_or_else(|| "this file".into());
                        self.pending_file_share_off = Some(PendingFileShareOff {
                            blake3,
                            sha256,
                            name,
                            active_uploads: active,
                        });
                    } else {
                        self.apply_share_off(&blake3, &sha256);
                    }
                }
            }
            Action::ConfirmStopFileShare => {
                if let Some(p) = self.pending_file_share_off.take() {
                    self.apply_share_off(&p.blake3, &p.sha256);
                    self.push_toast(
                        format!("Stopped sharing {} — peers disconnected", p.name),
                        ToastKind::Warning,
                    );
                }
            }
            Action::CancelStopFileShare => {
                self.pending_file_share_off = None;
                self.status = "Still sharing this file.".into();
            }
            Action::CopyShareLink(s) => {
                self.egui_ctx.output_mut(|o| o.copied_text = s);
                self.status =
                    "Share link copied — on your other device, paste it in Discover > “Add by Content ID”.".into();
                self.push_toast("Share link copied", ToastKind::Success);
            }
            Action::CopyText { text, what } => {
                self.egui_ctx.output_mut(|o| o.copied_text = text);
                self.status = format!("{what} copied ✓");
                self.push_toast(format!("{what} copied"), ToastKind::Success);
            }
            Action::DismissIntro(open_settings) => {
                self.settings.seen_intro = true;
                self.save_settings();
                if open_settings {
                    self.tab = Tab::Settings;
                }
            }
            Action::AddByLink => self.start_add_by_link(),
            Action::ConfirmDownload => {
                if let Some(p) = self.pending_download.take() {
                    self.download_share_target(p.target);
                }
            }
            Action::CancelDownload => self.pending_download = None,
            Action::RequestDelete(p) => self.pending_delete = Some(p),
            Action::ConfirmDelete => {
                if let Some(p) = self.pending_delete.take() {
                    if let Some(engine) = self.engine.clone() {
                        match engine.evict_cache(EvictPolicy::Blob(p.blake3.clone())) {
                            Ok(r) => {
                                let removed = r.removed.clone();
                                self.status =
                                    format!("Deleted {} from disk and updated peers.", p.name);
                                self.push_toast(format!("Deleted {}", p.name), ToastKind::Success);
                                if !removed.is_empty() {
                                    for b3 in &removed {
                                        self.forget_network_share(b3);
                                        self.unseed_blob(b3);
                                    }
                                    self.withdraw_from_tracker(removed);
                                }
                                self.refresh();
                                self.refresh_worldwide();
                            }
                            Err(e) => {
                                self.status = format!("Couldn't delete {}: {e}", p.name);
                                self.push_toast(
                                    format!("Couldn't delete {}", p.name),
                                    ToastKind::Error,
                                );
                            }
                        }
                    }
                }
            }
            Action::CancelDelete => self.pending_delete = None,
            Action::RefreshNetwork => self.start_network_fetch(),
            Action::OpenSettings => {
                self.tab = Tab::Settings;
                self.status = "Hugging Face downloads are in Settings > Connection.".into();
            }
            Action::AddFromNetwork(m) => self.start_add_from_network(&m),
            Action::ApplyIdentity => {
                let now = (
                    self.settings.device_name.trim().to_string(),
                    self.settings.group_code.trim().to_string(),
                );
                if now != self.applied_identity {
                    self.applied_identity = now;
                    self.save_settings();
                    self.apply_identity_live();
                    self.status = "Updated this device — re-announcing to the network…".into();
                }
            }
            Action::CreateGroup => {
                self.settings.group_code = noema_core::identity::new_group_code();
                self.applied_identity = (
                    self.settings.device_name.trim().to_string(),
                    self.settings.group_code.trim().to_string(),
                );
                self.save_settings();
                self.apply_identity_live();
                self.status =
                    "Created a device group. Enter this code on your other devices to link them."
                        .into();
            }
            Action::ApplySpeedCap => {
                self.apply_speed_cap();
                self.save_settings();
                self.status = if self.settings.download_cap_mbps == 0 {
                    "Download speed: unlimited".into()
                } else {
                    format!(
                        "Download speed capped at {} Mbps",
                        self.settings.download_cap_mbps
                    )
                };
            }
            Action::ApplyDownloadConnections => {
                let n = self.settings.download_connections.max(1);
                self.settings.download_connections = n;
                if let Some(engine) = &self.engine {
                    engine.set_max_download_connections(n as usize);
                }
                self.save_settings();
                self.status = if n <= 1 {
                    "Downloads: single connection".into()
                } else {
                    format!("Downloads: up to {n} parallel connections")
                };
            }
            Action::SaveSettings => self.save_settings(),
            Action::SetHfDownload(on) => {
                // Apply immediately to the running engine and keep the startup
                // snapshot in sync so this never shows up as a "needs restart"
                // change. Catalog search is unaffected.
                if let Some(engine) = &self.engine {
                    engine.set_hf_download_enabled(on);
                }
                self.applied_connection.allow_hf_download = on;
                self.settings.allow_hf_download = on;
                self.save_settings();
                self.status = if on {
                    "Hugging Face downloads enabled.".into()
                } else {
                    "Hugging Face downloads disabled — peer routes only.".into()
                };
            }
            Action::SetShareGated(on) => {
                let Some(engine) = self.engine.clone() else {
                    return;
                };
                if on {
                    engine.set_share_gated_enabled(true);
                    self.settings.share_gated = true;
                    self.save_settings();
                    if self.settings.share_worldwide {
                        self.refresh_worldwide();
                    }
                    self.status = "Now also sharing gated/licensed models you download.".into();
                } else {
                    // Opting out must sever promptly (consent): snapshot what's
                    // shared now, flip the flag, then withdraw + unseed the blobs
                    // that just dropped out — don't wait for the ~5-min background
                    // reconcile. Mirrors the per-model `apply_share_off` path.
                    let blobs = |e: &Engine| -> std::collections::HashSet<String> {
                        e.share_announce_items()
                            .map(|v| v.into_iter().map(|(_, a)| a.blake3).collect())
                            .unwrap_or_default()
                    };
                    let before = blobs(&engine);
                    engine.set_share_gated_enabled(false);
                    self.settings.share_gated = false;
                    self.save_settings();
                    let after = blobs(&engine);
                    let dropped: Vec<String> = before.difference(&after).cloned().collect();
                    for b3 in &dropped {
                        self.forget_network_share(b3);
                        self.unseed_blob(b3);
                    }
                    if !dropped.is_empty() {
                        self.withdraw_from_tracker(dropped.clone());
                    }
                    self.refresh();
                    if self.settings.share_worldwide {
                        self.refresh_worldwide();
                    }
                    self.status = if dropped.is_empty() {
                        "Gated/licensed models are no longer auto-shared.".into()
                    } else {
                        format!(
                            "Stopped auto-sharing {} gated/licensed model{} — withdrawn from the network.",
                            dropped.len(),
                            if dropped.len() == 1 { "" } else { "s" }
                        )
                    };
                }
            }
            Action::SetTheme(mode) => {
                if self.settings.theme != mode {
                    self.settings.theme = mode;
                    apply_theme(&self.egui_ctx, mode.is_dark());
                    self.save_settings();
                }
            }
            Action::Refresh => {
                self.refresh();
                self.refresh_visible_peer_counts();
                self.last_network_fetch = None;
                if self.tab == Tab::Network {
                    self.start_network_fetch();
                }
                self.status = "Refreshed local models and peer availability.".into();
            }
            Action::Evict(p) => {
                if let Some(engine) = &self.engine {
                    if let Ok(r) = engine.evict_cache(p) {
                        let removed = r.removed.clone();
                        self.status = format!(
                            "Freed {} ({} item(s))",
                            human(r.freed_bytes),
                            r.removed.len()
                        );
                        // Stop announcing the deleted blobs so they leave Explore
                        // at once instead of lingering for their TTL.
                        if !removed.is_empty() {
                            for b3 in &removed {
                                self.forget_network_share(b3);
                            }
                            self.withdraw_from_tracker(removed);
                            self.refresh_worldwide();
                        }
                    }
                    self.refresh();
                }
            }
        }
    }

    fn save_settings(&self) {
        let root = noema_core::paths::default_root();
        if let Ok(json) = serde_json::to_vec_pretty(&self.settings) {
            let _ = std::fs::write(root.join("ui-settings.json"), json);
        }
    }
}

impl eframe::App for App {
    /// Closing the app (window close or quit) withdraws this device's announces
    /// from the tracker and stops the seeder, so it stops showing as a peer in
    /// others' Explore right away instead of lingering until the announce TTL.
    /// Bounded so a slow or unreachable tracker can't hang the quit.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(engine) = self.engine.clone() {
            let _ = self.rt.block_on(async move {
                tokio::time::timeout(Duration::from_secs(3), engine.withdraw_from_tracker(&[]))
                    .await
            });
        }
        if let Some(w) = self.worldwide.take() {
            let _ = self
                .rt
                .block_on(async { tokio::time::timeout(Duration::from_secs(3), w.stop()).await });
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        self.sample_speeds();
        self.toasts
            .retain(|t| t.shown_at.elapsed() < Duration::from_secs(4));
        if self.busy || self.worldwide.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        if !self.toasts.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        let mut actions: Vec<Action> = Vec::new();
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
            self.tab = Tab::Discover;
            self.detail = None;
            self.focus_search = true;
        }
        if self.tab == Tab::Discover
            && (self.detail.is_some() || self.loading_detail)
            && !self.show_token
            && self.pending_download.is_none()
            && self.pending_delete.is_none()
            && self.pending_share_off.is_none()
            && self.pending_file_share_off.is_none()
            && self.quant_detail.is_none()
            && ctx.input(|i| i.key_pressed(egui::Key::Escape))
        {
            self.detail = None;
            self.loading_detail = false;
        }

        // Drag-and-drop a model file anywhere onto the window to share it — the
        // most natural "I have this file" gesture. Ignored while busy or when a
        // modal already owns the screen.
        if self.composer.is_none()
            && self.pending_download.is_none()
            && self.pending_delete.is_none()
            && self.pending_share_off.is_none()
            && self.pending_file_share_off.is_none()
            && self.quant_detail.is_none()
            && !self.busy
        {
            let dropped: Vec<PathBuf> = ctx.input(|i| {
                i.raw
                    .dropped_files
                    .iter()
                    .filter_map(|f| f.path.clone())
                    .collect()
            });
            if let Some(path) = dropped.into_iter().find(|p| is_model_file(p)) {
                self.open_composer_path(path);
            }
        }

        top_bar(self, ctx, &mut actions);
        bottom_bar(self, ctx);
        token_modal(self, ctx, &mut actions);
        confirm_download_modal(self, ctx, &mut actions);
        confirm_delete_modal(self, ctx, &mut actions);
        confirm_stop_share_modal(self, ctx, &mut actions);
        confirm_stop_file_share_modal(self, ctx, &mut actions);
        quant_detail_modal(self, ctx, &mut actions);
        composer_modal(self, ctx, &mut actions);
        drop_overlay(self, ctx);
        draw_toasts(self, ctx);

        if let Some(err) = self.init_error.clone() {
            egui::CentralPanel::default().show(ctx, |ui| {
                let pal = pal_of(ui);
                ui.add_space(40.0);
                ui.vertical_centered(|ui| ui.colored_label(pal.red, err));
            });
            return;
        }
        if self.tab == Tab::Network {
            let stale = self
                .last_network_fetch
                .map(|t| t.elapsed().as_secs() >= 20)
                .unwrap_or(true);
            if stale && !self.network_loading {
                self.start_network_fetch();
            }
        }

        // While a model's detail is open, re-sample its peer availability on a
        // timer so a remote peer that deleted/withdrew the file (or just went
        // offline) stops lingering as a phantom seeder, and a newly-online peer
        // shows up — without the user having to hit Refresh. Repaint to keep the
        // timer ticking even when the window is otherwise idle.
        if self.tab == Tab::Discover && self.detail.is_some() && !self.busy {
            let stale = self
                .last_peer_check
                .map(|t| t.elapsed().as_secs() >= 15)
                .unwrap_or(true);
            if stale {
                if let Some(detail) = self.detail.clone() {
                    self.start_seeder_check(&detail);
                }
            }
            ctx.request_repaint_after(Duration::from_secs(2));
        }

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Discover => draw_discover(ui, self, &mut actions),
            Tab::Network => draw_network(ui, self, &mut actions),
            Tab::Transfers => draw_transfers(ui, self, &mut actions),
            Tab::Library => draw_library(ui, self, &mut actions),
            Tab::Settings => draw_settings(ui, self, &mut actions),
        });

        for a in actions {
            self.apply(a);
        }
    }
}

fn top_bar(app: &mut App, ctx: &egui::Context, actions: &mut Vec<Action>) {
    egui::TopBottomPanel::top("top").show(ctx, |ui| {
        let pal = pal_of(ui);
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if let Some(logo) = &app.logo {
                ui.add(
                    egui::Image::new(logo)
                        .fit_to_exact_size(egui::vec2(26.0, 26.0))
                        .rounding(4.0),
                );
                ui.add_space(2.0);
            }
            ui.heading("Noema Atlas");
            ui.add_space(12.0);
            ui.selectable_value(&mut app.tab, Tab::Discover, "Discover");
            ui.selectable_value(&mut app.tab, Tab::Network, "Explore");
            ui.selectable_value(&mut app.tab, Tab::Transfers, "Transfers");
            ui.selectable_value(&mut app.tab, Tab::Library, "Library");
            ui.selectable_value(&mut app.tab, Tab::Settings, "Settings");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                theme_toggle(ui, app, actions);
                ui.add_space(2.0);
                network_status_pill(ui, app);
                let tok = if app.has_token {
                    "HF connected"
                } else {
                    "Sign in"
                };
                if ui.button(tok).clicked() {
                    app.show_token = true;
                }
                // tiny live speed readout in the header (painted arrows so they
                // always render).
                dir_arrow(ui, true, pal.blue_dl);
                ui.label(
                    egui::RichText::new(format!("{}/s", human(app.cur_dl_bps as u64)))
                        .small()
                        .weak(),
                );
                ui.add_space(4.0);
                dir_arrow(ui, false, pal.green);
                ui.label(
                    egui::RichText::new(format!("{}/s", human(app.cur_ul_bps as u64)))
                        .small()
                        .weak(),
                );
            });
        });
        ui.add_space(6.0);
    });
}

/// A small sun/moon button in the header that flips between light and dark. The
/// glyph is painted (not a font emoji) so it always renders regardless of the
/// loaded fonts, matching the painted speed arrows next to it.
fn theme_toggle(ui: &mut egui::Ui, app: &App, actions: &mut Vec<Action>) {
    let dark = app.settings.theme.is_dark();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(28.0, 24.0), egui::Sense::click());
    let resp = resp.on_hover_text(if dark {
        "Switch to light theme"
    } else {
        "Switch to dark theme"
    });
    let panel_fill = ui.visuals().panel_fill;
    let hover_bg = ui.visuals().widgets.hovered.weak_bg_fill;
    let icon = if resp.hovered() {
        ui.visuals().widgets.hovered.fg_stroke.color
    } else {
        ui.visuals().widgets.inactive.fg_stroke.color
    };
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, egui::Rounding::same(7.0), hover_bg);
    }
    let c = rect.center();
    if dark {
        // Moon: a disc with a panel-colored disc punched out to leave a crescent.
        p.circle_filled(c, 7.0, icon);
        p.circle_filled(c + egui::vec2(3.5, -2.6), 7.0, panel_fill);
    } else {
        // Sun: a small disc with eight rays.
        p.circle_filled(c, 4.6, icon);
        for i in 0..8 {
            let a = std::f32::consts::TAU * (i as f32) / 8.0;
            let dir = egui::vec2(a.cos(), a.sin());
            p.line_segment([c + dir * 6.6, c + dir * 9.2], egui::Stroke::new(1.6, icon));
        }
    }
    if resp.clicked() {
        actions.push(Action::SetTheme(app.settings.theme.toggled()));
    }
}

fn network_status_pill(ui: &mut egui::Ui, app: &App) {
    let pal = pal_of(ui);
    if app.worldwide.is_none() {
        return;
    }
    ui.add(
        egui::Button::new(
            egui::RichText::new("P2P: worldwide")
                .small()
                .color(pal.green),
        )
        .min_size(egui::vec2(0.0, 22.0)),
    )
    .on_hover_ui(|ui| {
        ui.label(egui::RichText::new("Sharing verified models").strong());
        if let Some(w) = &app.worldwide {
            ui.label(egui::RichText::new("Worldwide seeding is on.").small());
            let ticket = w.node_ticket();
            ui.monospace(
                egui::RichText::new(format!("node {}", &ticket[..ticket.len().min(44)])).small(),
            );
        }
    });
}

fn bottom_bar(app: &App, ctx: &egui::Context) {
    egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
        ui.add_space(3.0);
        if let Some((frac, label)) = &app.progress {
            ui.add(
                egui::ProgressBar::new(*frac)
                    .text(label.clone())
                    .animate(true),
            );
        }
        route_status_strip(ui, app);
        ui.horizontal(|ui| ui.label(egui::RichText::new(&app.status).small().weak()));
        ui.add_space(3.0);
    });
}

fn route_status_strip(ui: &mut egui::Ui, app: &App) {
    ui.horizontal_wrapped(|ui| {
        if let Some(active) = &app.active {
            if let Some(source) = &active.source {
                let kind = TransportKind::from_source_id(source);
                transport_chip(ui, kind.display_name(), Some(kind), false);
                ui.label(
                    egui::RichText::new(format!(
                        "Downloading via {} · {}/s",
                        source_label(Some(source)),
                        human(app.cur_dl_bps as u64)
                    ))
                    .small(),
                );
            } else {
                transport_chip(ui, "Resolving", Some(TransportKind::Iroh), false);
                ui.label(egui::RichText::new("Choosing a verified route").small());
            }
        }

        let iroh_uploads = app
            .worldwide
            .as_ref()
            .map(|w| w.metrics().active_uploads())
            .unwrap_or(0);
        if iroh_uploads > 0 {
            transport_chip(ui, "Iroh upload", Some(TransportKind::Iroh), false);
            ui.label(
                egui::RichText::new(format!(
                    "{} active {} · {}/s",
                    iroh_uploads,
                    plural(iroh_uploads as usize, "peer"),
                    human(app.cur_ul_bps as u64)
                ))
                .small(),
            );
        } else if app.active.is_none() {
            if app.worldwide.is_some() {
                transport_chip(ui, "Iroh ready", Some(TransportKind::Iroh), false);
            }
            if hf_download_pending(app) {
                transport_chip(
                    ui,
                    "HF pending restart",
                    Some(TransportKind::HuggingFace),
                    true,
                );
            } else if hf_download_live(app) {
                transport_chip(ui, "HF fallback", Some(TransportKind::HuggingFace), false);
            } else {
                transport_chip(ui, "HF off", Some(TransportKind::HuggingFace), true);
            }
        }
    });
}

fn draw_toasts(app: &App, ctx: &egui::Context) {
    if app.toasts.is_empty() {
        return;
    }
    egui::Area::new("toasts".into())
        .anchor(egui::Align2::RIGHT_TOP, [-18.0, 58.0])
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            let pal = pal_of(ui);
            ui.set_max_width(360.0);
            for toast in app.toasts.iter().rev() {
                let age = toast.shown_at.elapsed().as_secs_f32();
                let alpha = if age > 3.2 {
                    (1.0 - (age - 3.2) / 0.8).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                let color = toast_color(pal, toast.kind);
                let bg = pal.toast_bg;
                let fill = egui::Color32::from_rgba_unmultiplied(
                    bg.r(),
                    bg.g(),
                    bg.b(),
                    (235.0 * alpha) as u8,
                );
                let stroke = egui::Stroke::new(
                    1.0,
                    egui::Color32::from_rgba_unmultiplied(
                        color.r(),
                        color.g(),
                        color.b(),
                        (210.0 * alpha) as u8,
                    ),
                );
                let txt = pal.toast_text;
                egui::Frame::none()
                    .fill(fill)
                    .stroke(stroke)
                    .rounding(8.0)
                    .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(&toast.text).small().color(
                            egui::Color32::from_rgba_unmultiplied(
                                txt.r(),
                                txt.g(),
                                txt.b(),
                                (255.0 * alpha) as u8,
                            ),
                        ));
                    });
                ui.add_space(6.0);
            }
        });
}

fn toast_color(pal: Palette, kind: ToastKind) -> egui::Color32 {
    match kind {
        ToastKind::Info => pal.blue,
        ToastKind::Success => pal.green,
        ToastKind::Warning => pal.amber,
        ToastKind::Error => pal.red,
    }
}

fn token_modal(app: &mut App, ctx: &egui::Context, actions: &mut Vec<Action>) {
    let mut open = app.show_token;
    egui::Window::new("Hugging Face sign-in")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label("Gated models need a free Hugging Face access token.");
            ui.hyperlink_to("Get a token", "https://huggingface.co/settings/tokens");
            ui.add_space(6.0);
            ui.add(
                egui::TextEdit::singleline(&mut app.token_input)
                    .password(true)
                    .hint_text("hf_…")
                    .desired_width(320.0),
            );
            ui.add_space(6.0);
            if ui.button("Save securely").clicked() {
                actions.push(Action::SaveToken);
            }
            ui.label(
                egui::RichText::new("Stored in your OS keychain — never on disk in plaintext.")
                    .small()
                    .weak(),
            );
        });
    if app.show_token && !open {
        app.show_token = false;
    }
}

/// Confirmation before fetching a model from a share link or Explore. Shows what
/// you're about to pull (name, size, content id, peers) so a click is a decision,
/// not a surprise. A "don't ask again" toggle mirrors the Settings option.
fn confirm_download_modal(app: &mut App, ctx: &egui::Context, actions: &mut Vec<Action>) {
    let Some(pending) = app.pending_download.clone() else {
        return;
    };
    let target = &pending.target;
    let mut open = true;
    let mut skip = app.settings.skip_download_confirm;
    egui::Window::new("Download this model?")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(430.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let pal = pal_of(ui);
            ui.set_width(406.0);
            // The sender's human title. A bare-content-id link substitutes the
            // literal "shared-model.gguf" — flag that as a placeholder, not a
            // confident name.
            let title = target.display_title();
            let is_placeholder =
                title.is_empty() || title == "shared-model.gguf" || title == "shared model";
            if is_placeholder {
                ui.label(
                    egui::RichText::new("Unnamed model")
                        .strong()
                        .size(15.0)
                        .color(pal.muted),
                );
            } else {
                ui.label(egui::RichText::new(title).strong().size(15.0));
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if target.size > 0 {
                    ui.label(egui::RichText::new(human(target.size)).weak());
                }
                if !target.quant.trim().is_empty() {
                    badge(ui, target.quant.trim(), pal.muted);
                }
                if let Some(p) = pending.peers.filter(|n| *n > 0) {
                    let peers = if p == 1 {
                        "1 peer".to_string()
                    } else {
                        format!("{p} peers")
                    };
                    transport_badge(ui, &peers, Some(TransportKind::Iroh));
                }
            });
            ui.add_space(6.0);
            // Plain wrapped label (no inline chip — mixing an exact-sized chip with
            // wrapping text overlaps them). The peer badge above already shows the
            // route.
            let route_text = match pending.peers {
                Some(1) => "1 worldwide peer available — Atlas picks the fastest route and switches automatically if it stalls.",
                Some(n) if n > 1 => "Worldwide peers available — Atlas picks the fastest route and switches automatically if one stalls.",
                _ => "P2P-first — Atlas looks for worldwide peers and verifies every byte against the content ID.",
            };
            ui.label(egui::RichText::new(route_text).small().weak());
            ui.add_space(8.0);

            // ── Guaranteed by Atlas (the content hash) ──────────────────────
            let cid = if target.sha256.len() == 64 {
                &target.sha256
            } else {
                &target.blake3
            };
            egui::Frame::none()
                .fill(pal.green_bg)
                .rounding(6.0)
                .inner_margin(egui::Margin::symmetric(10.0, 7.0))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new("✓ Guaranteed")
                            .small()
                            .strong()
                            .color(pal.green_text),
                    );
                    if cid.len() == 64 {
                        ui.label(
                            egui::RichText::new(format!(
                                "Every byte is checked against content ID {}.",
                                short_hash(cid)
                            ))
                            .small()
                            .color(pal.green_text2),
                        );
                    }
                    let fname = if target.name.trim().is_empty() {
                        "model.gguf"
                    } else {
                        target.name.trim()
                    };
                    let (safety, _) = noema_core::classify_file_safety(fname);
                    let safe_line = match safety {
                        noema_core::FileSafety::Safe => {
                            "Pure-data model file — no executable code."
                        }
                        noema_core::FileSafety::Warn => "Ambiguous file type — treat with caution.",
                        noema_core::FileSafety::Blocked => {
                            "Contains an executable/pickle type — blocked by default."
                        }
                    };
                    ui.label(
                        egui::RichText::new(safe_line)
                            .small()
                            .color(pal.green_text2),
                    );
                });
            ui.add_space(6.0);

            // ── Sender says (unverified metadata) ───────────────────────────
            let license = target.license.trim();
            let has_meta = !license.is_empty()
                || !target.origin.trim().is_empty()
                || !target.desc.trim().is_empty();
            egui::Frame::none()
                .fill(pal.amber_bg)
                .rounding(6.0)
                .inner_margin(egui::Margin::symmetric(10.0, 7.0))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new("Sender says — not verified")
                            .small()
                            .strong()
                            .color(pal.amber),
                    );
                    if has_meta {
                        if !license.is_empty() {
                            let unknown = license.eq_ignore_ascii_case("unknown");
                            ui.label(
                                egui::RichText::new(format!(
                                    "License: {}{}",
                                    license,
                                    if unknown { " — you won't auto-reshare it" } else { "" }
                                ))
                                .small()
                                .color(if unknown {
                                    pal.amber_dim
                                } else {
                                    pal.amber_text
                                }),
                            );
                        }
                        if !target.origin.trim().is_empty() {
                            ui.label(
                                egui::RichText::new(format!("From: {}", target.origin.trim()))
                                    .small()
                                    .color(pal.amber_text),
                            );
                        }
                        if !target.desc.trim().is_empty() {
                            ui.label(
                                egui::RichText::new(format!("“{}”", target.desc.trim()))
                                    .small()
                                    .italics()
                                    .color(pal.amber_text),
                            );
                        }
                    }
                    ui.label(
                        egui::RichText::new(
                            "The name and license were typed by the sender — Atlas can't confirm them. Only the content fingerprint above is guaranteed.",
                        )
                        .small()
                        .color(pal.amber_faint),
                    );
                });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(egui::RichText::new("Download").strong()))
                    .clicked()
                {
                    actions.push(Action::ConfirmDownload);
                }
                if ui.button("Cancel").clicked() {
                    actions.push(Action::CancelDownload);
                }
            });
            ui.add_space(4.0);
            if ui
                .checkbox(&mut skip, "Don't ask again — download links immediately")
                .on_hover_text("You can re-enable this under Settings › Downloads.")
                .changed()
            {
                app.settings.skip_download_confirm = skip;
                actions.push(Action::SaveSettings);
            }
        });
    if !open && app.pending_download.is_some() {
        actions.push(Action::CancelDownload);
    }
}

/// Confirmation before removing a Library model. This is destructive because it
/// unlinks the materialized file and the cache blob, then withdraws the content
/// from peers.
fn confirm_delete_modal(app: &mut App, ctx: &egui::Context, actions: &mut Vec<Action>) {
    let Some(pending) = app.pending_delete.clone() else {
        return;
    };
    let mut open = true;
    egui::Window::new("Delete this model?")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(430.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let pal = pal_of(ui);
            ui.label(egui::RichText::new(&pending.name).strong().size(15.0));
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                if pending.size_bytes > 0 {
                    ui.label(egui::RichText::new(human(pending.size_bytes)).weak());
                }
                ui.label(
                    egui::RichText::new(format!("Content ID {}", short_hash(&pending.blake3)))
                        .small()
                        .weak(),
                );
            });
            if let Some(path) = pending.install_path.as_deref() {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(path).small().monospace().weak());
            }
            ui.add_space(8.0);
            egui::Frame::none()
                .fill(pal.red_bg)
                .stroke(egui::Stroke::new(1.0, pal.red_border))
                .rounding(6.0)
                .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new("This deletes the model file and cache from this Mac.")
                            .small()
                            .color(pal.red_text),
                    );
                    ui.label(
                        egui::RichText::new(
                            "Atlas also stops announcing this content to peers immediately.",
                        )
                        .small()
                        .color(pal.red_text),
                    );
                    if pending.shareable {
                        ui.label(
                            egui::RichText::new(
                                "Active peer downloads may fail over to another source.",
                            )
                            .small()
                            .color(pal.red_text2),
                        );
                    }
                });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("DELETE").strong().color(pal.red_strong),
                    ))
                    .clicked()
                {
                    actions.push(Action::ConfirmDelete);
                }
                if ui.button("Cancel").clicked() {
                    actions.push(Action::CancelDelete);
                }
            });
        });
    if !open && app.pending_delete.is_some() {
        actions.push(Action::CancelDelete);
    }
}

/// Confirmation before turning worldwide sharing off while peers are mid-transfer.
/// Stopping severs their connections, so we ask first (torrent clients do the
/// same when you quit with active uploads).
fn confirm_stop_share_modal(app: &mut App, ctx: &egui::Context, actions: &mut Vec<Action>) {
    let Some(pending) = app.pending_share_off.clone() else {
        return;
    };
    let n = pending.active_uploads;
    let mut open = true;
    egui::Window::new("Stop sharing now?")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(430.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let pal = pal_of(ui);
            ui.horizontal(|ui| {
                transport_chip(ui, "Iroh", Some(TransportKind::Iroh), false);
                ui.label(
                    egui::RichText::new(format!(
                        "{n} peer{} downloading from you right now",
                        if n == 1 { " is" } else { "s are" }
                    ))
                    .strong(),
                );
            });
            ui.add_space(6.0);
            egui::Frame::none()
                .fill(pal.amber_bg)
                .stroke(egui::Stroke::new(
                    1.0,
                    pal.amber_border,
                ))
                .rounding(6.0)
                .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(
                            "Stopping disconnects them immediately. Like a torrent swarm, they'll fail over to other peers if any are available — but their transfer from you is cut.",
                        )
                        .small()
                        .color(pal.amber_text),
                    );
                });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Stop & disconnect")
                            .strong()
                            .color(pal.amber_text2),
                    ))
                    .clicked()
                {
                    actions.push(Action::ConfirmStopWorldwide);
                }
                if ui.button("Keep sharing").clicked() {
                    actions.push(Action::CancelStopWorldwide);
                }
            });
        });
    if !open && app.pending_share_off.is_some() {
        actions.push(Action::CancelStopWorldwide);
    }
}

/// Confirmation before turning a *single file's* open-mesh share off while peers
/// are mid-transfer of that file. Stopping severs them from it, so we ask first —
/// the per-file counterpart of [`confirm_stop_share_modal`].
fn confirm_stop_file_share_modal(app: &mut App, ctx: &egui::Context, actions: &mut Vec<Action>) {
    let Some(pending) = app.pending_file_share_off.clone() else {
        return;
    };
    let n = pending.active_uploads;
    let mut open = true;
    egui::Window::new("Stop sharing this file?")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(430.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let pal = pal_of(ui);
            ui.label(
                egui::RichText::new(&pending.name)
                    .strong()
                    .color(pal.strong),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                transport_chip(ui, "Iroh", Some(TransportKind::Iroh), false);
                ui.label(
                    egui::RichText::new(format!(
                        "{n} peer{} pulling this file right now",
                        if n == 1 { " is" } else { "s are" }
                    ))
                    .strong(),
                );
            });
            ui.add_space(6.0);
            egui::Frame::none()
                .fill(pal.amber_bg)
                .stroke(egui::Stroke::new(
                    1.0,
                    pal.amber_border,
                ))
                .rounding(6.0)
                .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(
                            "Stopping removes this file from Explore and disconnects those peers from it immediately. Like a torrent swarm, they'll fail over to other peers if any are available — but their transfer from you is cut. Other files you share stay up.",
                        )
                        .small()
                        .color(pal.amber_text),
                    );
                });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Stop & disconnect")
                            .strong()
                            .color(pal.amber_text2),
                    ))
                    .clicked()
                {
                    actions.push(Action::ConfirmStopFileShare);
                }
                if ui.button("Keep sharing").clicked() {
                    actions.push(Action::CancelStopFileShare);
                }
            });
        });
    if !open && app.pending_file_share_off.is_some() {
        actions.push(Action::CancelStopFileShare);
    }
}

/// One transport row inside the routes popup: glyph + name + a live status, with
/// a soft tint when the route is currently usable. This is the visual heart of
fn route_row(ui: &mut egui::Ui, kind: TransportKind, status: &str, detail: &str, live: bool) {
    let pal = pal_of(ui);
    let base = kind.color_on(ui.visuals().dark_mode);
    let color = if live { base } else { pal.faint };
    let fill = if live {
        egui::Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), 26)
    } else {
        ui.style().visuals.faint_bg_color
    };
    egui::Frame::none()
        .fill(fill)
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
                paint_transport_glyph(ui.painter(), rect, kind, color);
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(kind.display_name())
                            .strong()
                            .color(color),
                    );
                    ui.label(egui::RichText::new(detail).small().weak());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(status).strong().color(if live {
                        base
                    } else {
                        pal.faint
                    }));
                });
            });
        });
    ui.add_space(5.0);
}

/// Per-quant routes & peers popup — opened by tapping a quant in Discover or a
/// model in Explore. Shows every transport Atlas can pull from, with live peer
/// counts where it has them (worldwide Iroh) and failover status for the
/// rest, so the resilience story is visible instead of a bare "Iroh live" badge.
fn quant_detail_modal(app: &mut App, ctx: &egui::Context, actions: &mut Vec<Action>) {
    let Some(q) = app.quant_detail.clone() else {
        return;
    };
    let sha = q.sha256.clone();
    // Live worldwide peer count from the tracker. A network-catalog row already
    // carries its own count.
    let worldwide = match &q.download {
        QuantDownload::Network(m) => m.peers,
        _ if sha.is_empty() => 0,
        _ => app.worldwide_peers.get(&sha).copied().unwrap_or(0),
    };
    let hf = hf_download_live(app);
    let mut open = true;
    egui::Window::new("Routes & peers")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(450.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let pal = pal_of(ui);
            ui.label(egui::RichText::new(&q.title).strong().size(16.0));
            ui.horizontal_wrapped(|ui| {
                if q.size > 0 {
                    ui.label(egui::RichText::new(human(q.size)).weak());
                }
                if !q.subtitle.trim().is_empty() {
                    ui.label(egui::RichText::new(&q.subtitle).small().weak());
                }
                if q.cached {
                    badge(ui, "Downloaded", pal.muted);
                }
            });

            ui.add_space(8.0);
            let green = pal.green;
            if worldwide > 0 {
                ui.label(
                    egui::RichText::new(format!(
                        "Live now: {worldwide} worldwide {}",
                        plural(worldwide, "peer")
                    ))
                    .strong()
                    .color(green),
                );
            } else if hf {
                ui.label(
                    egui::RichText::new(
                        "No P2P peers right now — Atlas will fetch from Hugging Face.",
                    )
                    .small()
                    .weak(),
                );
            } else {
                ui.label(
                    egui::RichText::new(
                        "No P2P peers right now, and Hugging Face is off — turn it on in \
                         Settings, or wait for a peer to come online.",
                    )
                    .small()
                    .weak(),
                );
            }

            ui.add_space(8.0);
            ui.label(egui::RichText::new("Where you can get it").strong());
            ui.add_space(4.0);

            route_row(
                ui,
                TransportKind::Iroh,
                &peer_count_label(worldwide),
                if worldwide > 0 {
                    "Worldwide P2P over Iroh — NAT-traversing, no ports to open"
                } else {
                    "No worldwide peers announced yet"
                },
                worldwide > 0,
            );
            route_row(
                ui,
                TransportKind::HuggingFace,
                if hf { "On" } else { "Off" },
                if hf {
                    "Origin download, verified against the hash"
                } else {
                    "Off — turn on in Settings to allow as a fallback"
                },
                hf,
            );

            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "Atlas tries routes top-to-bottom and switches automatically if one stalls.",
                )
                .small()
                .weak(),
            );

            ui.add_space(8.0);
            // Prefer the sha256 content id; fall back to the BLAKE3 (Iroh's
            // address) when the sha isn't known yet (e.g. a network-catalog row).
            let (id_label, id_value) = if !sha.is_empty() {
                ("Content ID (sha256)", sha.clone())
            } else if !q.blake3.is_empty() {
                ("Content ID (BLAKE3)", q.blake3.clone())
            } else {
                ("", String::new())
            };
            if !id_value.is_empty() {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(id_label).small().weak());
                    ui.monospace(egui::RichText::new(short_hash(&id_value)).small());
                    if ui.small_button("copy").clicked() {
                        actions.push(Action::CopyText {
                            text: id_value.clone(),
                            what: "Content ID".into(),
                        });
                    }
                });
            }
            ui.label(
                egui::RichText::new(
                    "Every byte is verified against this hash — whoever serves it.",
                )
                .small()
                .weak()
                .color(green),
            );

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if !q.cached {
                    let dl = match &q.download {
                        QuantDownload::Hf(rfilename) => Some(Action::Download(rfilename.clone())),
                        QuantDownload::Bundle => Some(Action::DownloadBundle),
                        QuantDownload::Network(m) => Some(Action::AddFromNetwork(m.clone())),
                        QuantDownload::None => None,
                    };
                    if let Some(dl) = dl {
                        if ui
                            .add_enabled(
                                !app.busy,
                                egui::Button::new(egui::RichText::new("Download").strong()),
                            )
                            .on_hover_text(
                                "Fetches from the fastest available route and verifies every byte.",
                            )
                            .clicked()
                        {
                            actions.push(dl);
                            actions.push(Action::CloseQuantDetail);
                        }
                    }
                }
                if ui.button("Close").clicked() {
                    actions.push(Action::CloseQuantDetail);
                }
            });
        });
    if !open && app.quant_detail.is_some() {
        actions.push(Action::CloseQuantDetail);
    }
}
fn peer_count_label(n: usize) -> String {
    match n {
        0 => "None".to_string(),
        1 => "1 peer".to_string(),
        n => format!("{n} peers"),
    }
}

/// Whether a path looks like a model weights file we can share.
fn is_model_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("gguf") | Some("safetensors") | Some("bin")
    )
}

/// A soft full-window hint while a model file is being dragged over the window.
fn drop_overlay(app: &App, ctx: &egui::Context) {
    if app.composer.is_some()
        || app.pending_download.is_some()
        || app.pending_delete.is_some()
        || app.pending_share_off.is_some()
        || app.pending_file_share_off.is_some()
        || app.quant_detail.is_some()
        || app.busy
    {
        return;
    }
    let hovering = ctx.input(|i| {
        i.raw
            .hovered_files
            .iter()
            .any(|f| f.path.as_deref().map(is_model_file).unwrap_or(true))
    });
    if !hovering {
        return;
    }
    egui::Area::new("drop-overlay".into())
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            let pal = pal_of(ui);
            egui::Frame::none()
                .fill(pal.green_bg_hi)
                .rounding(12.0)
                .inner_margin(egui::Margin::symmetric(28.0, 20.0))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new("Drop to share a model")
                            .size(20.0)
                            .strong()
                            .color(pal.on_green),
                    );
                    ui.label(egui::RichText::new(".gguf or .safetensors").color(pal.green_text2));
                });
        });
}

/// The "Send a model" composer: title / license / describe a file that isn't on
/// Hugging Face, then send it as a private link or publish it to Explore.
fn composer_modal(app: &mut App, ctx: &egui::Context, actions: &mut Vec<Action>) {
    let Some(c) = app.composer.as_mut() else {
        return;
    };
    let editing = c.edit_manifest_id.is_some();
    let mut open = true;
    egui::Window::new(if editing {
        "Edit model details"
    } else {
        "Send a model"
    })
    .open(&mut open)
    .collapsible(false)
    .resizable(false)
    .default_width(460.0)
    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
    .show(ctx, |ui| {
        let pal = pal_of(ui);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("📦").size(18.0));
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(&c.filename).strong());
                ui.horizontal(|ui| {
                    if c.size > 0 {
                        ui.label(egui::RichText::new(human(c.size)).small().weak());
                    }
                    if let Some(f) = &c.format {
                        badge(ui, &f.to_ascii_uppercase(), pal.muted);
                    }
                    if let Some(a) = &c.architecture {
                        ui.label(egui::RichText::new(format!("arch: {a}")).small().weak());
                    }
                });
            });
        });
        if (c.title_from_file || c.quant_from_file) && !editing {
            ui.label(
                egui::RichText::new("✓ Read the name and quant straight from the file.")
                    .small()
                    .color(pal.green),
            );
        }
        ui.separator();

        // Live title preview — the receiver's-eye view.
        ui.label(egui::RichText::new("Title preview").small().weak());
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::symmetric(10.0, 6.0))
            .show(ui, |ui| {
                ui.label(egui::RichText::new(c.preview()).strong().size(15.0));
            });
        ui.add_space(6.0);
        egui::Grid::new("composer-fields")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Title");
                ui.add(
                    egui::TextEdit::singleline(&mut c.title)
                        .hint_text("e.g. Mistral-7B-Instruct-v0.3")
                        .desired_width(280.0),
                );
                ui.end_row();
                ui.label("Family");
                ui.add(
                    egui::TextEdit::singleline(&mut c.family)
                        .hint_text("e.g. Mistral")
                        .desired_width(280.0),
                );
                ui.end_row();
                ui.label("Quantization");
                ui.add(
                    egui::TextEdit::singleline(&mut c.quant)
                        .hint_text("e.g. Q4_K_M")
                        .desired_width(280.0),
                );
                ui.end_row();
            });
        ui.add_space(4.0);

        // License — the field that gates public sharing.
        ui.horizontal(|ui| {
            ui.label("License");
            egui::ComboBox::from_id_source("composer-license")
                .selected_text(LICENSE_OPTIONS[c.license_idx].0)
                .width(300.0)
                .show_ui(ui, |ui| {
                    for (i, (label, _)) in LICENSE_OPTIONS.iter().enumerate() {
                        ui.selectable_value(&mut c.license_idx, i, *label);
                    }
                });
        });
        ui.add_space(4.0);

        ui.label(egui::RichText::new("Description (optional)").small().weak());
        ui.add(
            egui::TextEdit::multiline(&mut c.description)
                .hint_text("What is this model? What was it fine-tuned for?")
                .desired_rows(2)
                .desired_width(f32::INFINITY),
        );
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new("Where is this from? (optional)")
                .small()
                .weak(),
        );
        ui.add(
            egui::TextEdit::singleline(&mut c.origin)
                .hint_text("Paste the old Hugging Face link, if any — helps others trust it")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(8.0);
        let can_publish = c.license_permits_public();
        if !can_publish {
            c.publish = false;
        }
        ui.label(egui::RichText::new("How do you want to send it?").small().weak());
        ui.radio_value(
            &mut c.publish,
            false,
            "Private link — only people you send the link to",
        );
        ui.add_enabled_ui(can_publish, |ui| {
            ui.radio_value(
                &mut c.publish,
                true,
                "Publish to Explore — anyone worldwide can find it",
            );
        });
        if !can_publish {
            ui.label(
                egui::RichText::new(
                    "Pick a known open license to publish on Explore. Otherwise send a private link — it works the same, just isn't searchable.",
                )
                .small()
                .weak()
                .color(pal.amber_dim),
            );
        }
        if !editing {
            ui.add_space(2.0);
            ui.checkbox(
                &mut c.check_hf,
                "Also check Hugging Face for a canonical match",
            )
            .on_hover_text(
                "Off by default — this flow is for models that aren't on Hugging Face.",
            );
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let label = if editing { "Save" } else { "Create link" };
            if ui
                .add(egui::Button::new(egui::RichText::new(label).strong()))
                .clicked()
            {
                actions.push(Action::ComposerConfirm);
            }
            if ui.button("Cancel").clicked() {
                actions.push(Action::ComposerCancel);
            }
        });
    });
    if !open && app.composer.is_some() {
        actions.push(Action::ComposerCancel);
    }
}
/// One-time intro shown on first launch: what makes Atlas different, plus an
/// upfront note that worldwide sharing is on by default (a consent disclosure,
/// not buried in Settings). Dismissed permanently once acknowledged.
fn first_run_card(ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .inner_margin(egui::Margin::symmetric(14.0, 12.0))
        .show(ui, |ui| {
            ui.label(egui::RichText::new("Welcome to Noema Atlas").strong().size(15.0));
            ui.add_space(4.0);
            for (head, body) in [
                ("Verified, byte-for-byte", "every download is checked against a published content hash — corruption and tampering are caught as bytes stream in."),
                ("Reuses what you have", "identical files across models and sources are stored once; an import you already have never re-downloads."),
                ("Worldwide P2P", "Atlas downloads from Hugging Face (the verified origin) and, when peers are seeding a file, fetches it from them over Iroh — then seeds the open-licensed models you download back. The mesh grows as people import and share the GGUFs they already have."),
            ] {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(format!("•  {head} — ")).small().strong());
                    ui.label(egui::RichText::new(body).small().weak());
                });
            }
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "Sharing is ON by default: openly-licensed models you download are re-shared with peers worldwide. Gated/licensed and privately-imported models stay private unless you opt them in. You can also enable sharing gated models, opt individual models in/out, or turn sharing off entirely — all in Settings.",
                )
                .small()
                .weak(),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(egui::RichText::new("Got it").strong()))
                    .clicked()
                {
                    actions.push(Action::DismissIntro(false));
                }
                if ui.button("Review sharing settings").clicked() {
                    actions.push(Action::DismissIntro(true));
                }
            });
        });
    ui.add_space(10.0);
}

fn draw_discover(ui: &mut egui::Ui, app: &mut App, actions: &mut Vec<Action>) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let resp = ui.add_sized(
            [ui.available_width() - 90.0, 28.0],
            egui::TextEdit::singleline(&mut app.query)
                .hint_text("Search for a model — try “Llama”, “Qwen”, “Mistral”…"),
        );
        if app.focus_search {
            resp.request_focus();
            app.focus_search = false;
        }
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            actions.push(Action::Search);
        }
        let label = if app.searching {
            "Searching…"
        } else {
            "Search"
        };
        if ui
            .add_enabled(
                !app.searching,
                egui::Button::new(label).min_size(egui::vec2(80.0, 28.0)),
            )
            .clicked()
        {
            actions.push(Action::Search);
        }
    });
    ui.add_space(4.0);

    egui::CollapsingHeader::new("Add by Content ID / share link")
        .default_open(false)
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(
                    "Paste a share link (or a model's Content ID) from another device to fetch it directly over P2P — even if it isn't on Hugging Face.",
                )
                .small()
                .weak(),
            );
            ui.horizontal(|ui| {
                let resp = ui.add_sized(
                    [ui.available_width() - 70.0, 26.0],
                    egui::TextEdit::singleline(&mut app.add_link_input)
                        .hint_text("atlas1:…  or a 64-character sha256"),
                );
                let submit =
                    resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let clicked = ui
                    .add_sized([60.0, 26.0], egui::Button::new("Add"))
                    .clicked();
                if (clicked || submit) && !app.add_link_input.trim().is_empty() {
                    actions.push(Action::AddByLink);
                }
            });
        });
    ui.add_space(6.0);

    if app.searching {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Searching Hugging Face…");
        });
        return;
    }
    if let Some(detail) = app.detail.clone() {
        draw_model_detail(ui, app, &detail, actions);
        return;
    }
    // A model is being opened: show a spinner instead of flashing the empty-state
    // hero (which reads as "the click did nothing").
    if app.loading_detail {
        ui.add_space(28.0);
        ui.vertical_centered(|ui| {
            ui.spinner();
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Loading model…").weak());
            ui.add_space(8.0);
            if ui.button("Back to results").clicked() {
                actions.push(Action::Back);
            }
        });
        return;
    }
    if app.results.is_empty() {
        // A failed search shows a retry block, not the blank hero.
        if let Some(err) = app.last_search_error.clone() {
            ui.add_space(28.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("Couldn't search Hugging Face")
                        .size(18.0)
                        .strong(),
                );
                ui.add_space(4.0);
                ui.label(egui::RichText::new(err).weak());
                ui.add_space(10.0);
                if ui.button("Try again").clicked() {
                    actions.push(Action::Search);
                }
            });
            return;
        }
        // One-time first-run intro (the app's superpowers + a sharing-consent note).
        if !app.settings.seen_intro {
            first_run_card(ui, actions);
        }
        ui.add_space(28.0);
        ui.vertical_centered(|ui| {
            if let Some(logo) = &app.logo {
                ui.add(egui::Image::new(logo).fit_to_exact_size(egui::vec2(96.0, 96.0)));
                ui.add_space(8.0);
            }
            ui.label(
                egui::RichText::new("Find any open model")
                    .size(22.0)
                    .strong(),
            );
            ui.label(
                egui::RichText::new(
                    "Search by name. Pick a version. Download — verified, peer-to-peer first.",
                )
                .weak(),
            );
            ui.add_space(14.0);
            ui.label(egui::RichText::new("Popular searches").small().weak());
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for s in [
                    "Llama", "Qwen", "Mistral", "Phi", "Gemma", "DeepSeek", "SmolLM",
                ] {
                    if ui.button(s).clicked() {
                        actions.push(Action::Suggest(s.to_string()));
                    }
                }
            });
        });
        return;
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        for m in &app.results {
            draw_model_row(ui, m, actions);
            ui.add_space(6.0);
        }
    });
}

fn draw_model_row(ui: &mut egui::Ui, m: &HfModel, actions: &mut Vec<Action>) {
    let pal = pal_of(ui);
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(12.0, 10.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(m.name()).strong().size(16.0));
                        if m.has_gguf() {
                            badge(ui, "GGUF", pal.blue_dl);
                        }
                        if m.gated {
                            badge(ui, "Gated", pal.amber);
                        }
                    });
                    ui.label(
                        egui::RichText::new(format!("by {}", m.author()))
                            .small()
                            .weak(),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "{} downloads   ·   {} likes   ·   {}{}",
                            compact(m.downloads),
                            compact(m.likes),
                            m.pipeline_tag.clone().unwrap_or_else(|| "—".into()),
                            m.license()
                                .map(|l| format!("   ·   {l}"))
                                .unwrap_or_default(),
                        ))
                        .small()
                        .weak(),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("View").clicked() {
                        actions.push(Action::OpenModel(m.id.clone()));
                    }
                });
            });
        });
}

fn draw_best_route_line(ui: &mut egui::Ui, app: &App, wpeers: usize, actions: &mut Vec<Action>) {
    ui.horizontal_wrapped(|ui| {
        if wpeers > 0 {
            transport_chip(ui, "Worldwide", Some(TransportKind::Iroh), false);
            ui.label(
                egui::RichText::new(format!(
                    "Best now: {wpeers} worldwide {}",
                    plural(wpeers, "peer")
                ))
                .small(),
            );
        } else if hf_download_live(app) {
            transport_chip(ui, "HF fallback", Some(TransportKind::HuggingFace), false);
            ui.label(
                egui::RichText::new(
                    "No peers yet — Hugging Face can fill in after peer routes.",
                )
                .small(),
            );
        } else if hf_download_pending(app) {
            transport_chip(ui, "HF pending restart", Some(TransportKind::HuggingFace), true);
            ui.label(
                egui::RichText::new("No peers yet — restart Atlas to use Hugging Face fallback.")
                    .small()
                    .weak(),
            );
        } else {
            transport_chip(ui, "P2P only", Some(TransportKind::Iroh), true);
            ui.label(
                egui::RichText::new(
                    "No peers yet — turn on Hugging Face downloads in Settings, or import a copy to seed it.",
                )
                .small()
                .weak(),
            );
            if ui.small_button("Settings").clicked() {
                actions.push(Action::OpenSettings);
            }
        }
    });
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        noun.to_string()
    } else {
        format!("{noun}s")
    }
}

fn hf_download_live(app: &App) -> bool {
    app.applied_connection.allow_hf_download
}

fn hf_download_pending(app: &App) -> bool {
    app.settings.allow_hf_download != app.applied_connection.allow_hf_download
}

fn draw_route_list(ui: &mut egui::Ui, app: &App, wpeers: usize, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Routes").small().strong());
        if ui.small_button("Refresh routes").clicked() {
            actions.push(Action::Refresh);
        }
    });
    ui.horizontal_wrapped(|ui| {
        transport_chip(ui, "Worldwide", Some(TransportKind::Iroh), wpeers == 0);
        if wpeers > 0 {
            ui.label(egui::RichText::new(format!("{wpeers} seeding worldwide")).small());
        } else {
            ui.label(
                egui::RichText::new("No worldwide peers announced yet")
                    .small()
                    .weak(),
            );
        }
    });
    ui.horizontal_wrapped(|ui| {
        transport_chip(
            ui,
            "Hugging Face",
            Some(TransportKind::HuggingFace),
            !hf_download_live(app),
        );
        if hf_download_live(app) {
            ui.label(
                egui::RichText::new("Enabled as a last-resort byte source")
                    .small()
                    .weak(),
            );
        } else if hf_download_pending(app) {
            ui.label(
                egui::RichText::new("Pending restart before Hugging Face can serve bytes")
                    .small()
                    .weak(),
            );
        } else {
            ui.label(
                egui::RichText::new(
                    "Hugging Face downloads are off — P2P only (enable in Settings)",
                )
                .small()
                .weak(),
            );
            if ui.small_button("Settings").clicked() {
                actions.push(Action::OpenSettings);
            }
        }
    });
}

fn draw_model_detail(
    ui: &mut egui::Ui,
    app: &App,
    detail: &HfModelDetail,
    actions: &mut Vec<Action>,
) {
    let pal = pal_of(ui);
    if ui.button("Back to results").clicked() {
        actions.push(Action::Back);
    }
    ui.add_space(6.0);
    ui.heading(detail.name());
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new(format!("by {}", detail.id.split('/').next().unwrap_or(""))).weak(),
        );
        if let Some(lic) = &detail.license {
            badge(ui, lic, pal.muted);
        }
        ui.hyperlink_to(
            "View on Hugging Face",
            format!("https://huggingface.co/{}", detail.id),
        );
    });
    if detail.gated {
        ui.add_space(6.0);
        egui::Frame::group(ui.style())
            .fill(pal.amber_bg)
            .show(ui, |ui| {
                ui.label("Gated — click “Sign in” and add your token, then accept the model's terms on its page.");
            });
    } else {
        // Make the sharing implication legible *before* downloading.
        ui.add_space(4.0);
        let redistributable =
            noema_core::RedistributionClass::for_license(detail.license.as_deref())
                .allows_public_redistribution();
        let note = if redistributable {
            "Openly licensed — once downloaded it's shared with peers by default (you can stop this in Settings)."
        } else {
            "Download-only license — Atlas won't reshare this to peers."
        };
        ui.label(egui::RichText::new(note).small().weak());
    }
    ui.add_space(10.0);
    let has_bundle = detail.has_safetensors_bundle();
    // GGUF files become one entry per quant, folding a split quant's shards
    // together. Any other weight file (a standalone `.bin`, say) is still listed
    // on its own. Safetensors shards are folded into the one-click bundle above.
    let quants = detail.gguf_quants();
    let other_files: Vec<&HfFile> = detail
        .weight_files()
        .into_iter()
        .filter(|f| !f.is_safetensors_shard() && !f.is_gguf())
        .collect();
    if !has_bundle && quants.is_empty() && other_files.is_empty() {
        ui.label("No downloadable weight files found.");
        if app.loading_detail {
            ui.spinner();
        }
        return;
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        // The whole-model bundle (safetensors / MLX): one button, no quants.
        if has_bundle {
            draw_bundle_row(ui, app, detail, actions);
            ui.add_space(10.0);
        }
        if !quants.is_empty() || !other_files.is_empty() {
            let heading = if has_bundle {
                "Or pick an individual GGUF quant"
            } else {
                "Choose a version to download"
            };
            ui.label(egui::RichText::new(heading).strong());
            ui.add_space(4.0);
            let sizes: Vec<u64> = quants.iter().map(|q| q.total_size()).collect();
            let rec = recommend_quant_idx(&sizes, app.mem_budget);
            if let Some((_, reason)) = &rec {
                if !reason.is_empty() {
                    ui.label(
                        egui::RichText::new(format!("Recommended pick {reason} on this device."))
                            .small()
                            .weak(),
                    );
                    ui.add_space(2.0);
                }
            }
            let rec_idx = rec.as_ref().map(|(i, _)| *i);
            for (i, q) in quants.iter().enumerate() {
                let is_rec = rec_idx == Some(i);
                if q.files.len() == 1 {
                    draw_file_row(ui, app, &q.files[0], is_rec, actions);
                } else {
                    let refs: Vec<&HfFile> = q.files.iter().collect();
                    draw_quant_row(ui, app, &q.label, &refs, is_rec, actions);
                }
                ui.add_space(6.0);
            }
            for f in &other_files {
                draw_file_row(ui, app, f, false, actions);
                ui.add_space(6.0);
            }
        }
    });
}

/// One card for an entire sharded safetensors/MLX model.
fn draw_bundle_row(
    ui: &mut egui::Ui,
    app: &App,
    detail: &HfModelDetail,
    actions: &mut Vec<Action>,
) {
    let pal = pal_of(ui);
    let shards = detail.safetensors_shards();
    let sidecars = detail.model_sidecars();
    let total = detail.bundle_total_size();
    let file_count = shards.len() + sidecars.len();
    let cached = !shards.is_empty()
        && shards.iter().all(|f| {
            f.sha256
                .as_ref()
                .map(|s| app.cached_sha256.contains(s))
                .unwrap_or(false)
        });
    let shard_shas: Vec<&String> = shards.iter().filter_map(|f| f.sha256.as_ref()).collect();
    let shard_total = shard_shas.len();
    let iroh_covered = shard_shas
        .iter()
        .filter(|s| app.worldwide_peers.get(s.as_str()).copied().unwrap_or(0) > 0)
        .count();
    let wpeers: usize = shard_shas
        .iter()
        .filter_map(|s| app.worldwide_peers.get(*s).copied())
        .max()
        .unwrap_or(0);
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(12.0, 10.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        let rep_sha = shard_shas.first().map(|s| s.to_string()).unwrap_or_default();
                        if ui
                            .add(
                                egui::Label::new(
                                    egui::RichText::new(detail.bundle_variant_label())
                                        .strong()
                                        .size(15.0),
                                )
                                .sense(egui::Sense::click()),
                            )
                            .on_hover_text("See routes & peers")
                            .clicked()
                        {
                            actions.push(Action::OpenQuantDetail(QuantDetail {
                                title: detail.bundle_variant_label(),
                                subtitle: format!("Complete model · {file_count} files"),
                                size: total,
                                sha256: rep_sha,
                                blake3: String::new(),
                                download: QuantDownload::Bundle,
                                cached,
                            }));
                        }
                        badge(ui, "Full model", pal.muted)
                            .on_hover_text("Every shard at full precision — the largest, highest-fidelity option. If a GGUF quant is listed below, that's a much smaller download.");
                        if cached {
                            badge(ui, "Downloaded", pal.muted);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{} · {file_count} files", human(total)))
                                .small(),
                        );
                        badge(
                            ui,
                            "Complete model",
                            pal.blue,
                        );
                        if hf_download_live(app) {
                            transport_badge(ui, "Hugging Face", Some(TransportKind::HuggingFace));
                        } else if hf_download_pending(app) {
                            transport_chip(
                                ui,
                                "HF pending restart",
                                Some(TransportKind::HuggingFace),
                                true,
                            );
                        } else {
                            transport_chip(ui, "HF off", Some(TransportKind::HuggingFace), true);
                        }
                        if iroh_covered > 0 {
                            transport_badge(
                                ui,
                                format!("Iroh {iroh_covered}/{shard_total} files"),
                                Some(TransportKind::Iroh),
                            );
                        }
                    });
                    if shard_total > 0 && iroh_covered > 0 && iroh_covered < shard_total {
                        ui.horizontal_wrapped(|ui| {
                            transport_chip(ui, "Partial P2P", Some(TransportKind::Iroh), false);
                            ui.label(
                                egui::RichText::new(format!(
                                    "Peers cover {iroh_covered}/{shard_total} files; Atlas fills the rest from the next eligible route.",
                                ))
                                .small(),
                            );
                        });
                    } else {
                        draw_best_route_line(ui, app, wpeers, actions);
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if cached {
                        if ui.button("Open").clicked() {
                            actions.push(Action::Reveal(PathBuf::from(&app.settings.models_dir)));
                        }
                    } else if ui
                        .add_enabled(
                            !app.busy,
                            egui::Button::new(egui::RichText::new("Download model").strong()),
                        )
                        .on_hover_text("Fetches from the fastest available peer and verifies every byte — switches automatically if one stalls.")
                        .clicked()
                    {
                        actions.push(Action::DownloadBundle);
                    }
                });
            });
            ui.add_space(2.0);
            egui::CollapsingHeader::new(egui::RichText::new("What's included").small())
                .id_source(format!("bundle::{}", detail.id))
                .show(ui, |ui| {
                    draw_route_list(ui, app, wpeers, actions);
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "All weight shards plus the config & tokenizer needed to load it.",
                        )
                        .small()
                        .weak(),
                    );
                    ui.add_space(2.0);
                    for f in &shards {
                        ui.label(
                            egui::RichText::new(format!(
                                "    {} — {} · verified by sha256",
                                f.rfilename,
                                human(f.size)
                            ))
                            .small(),
                        );
                    }
                    for f in &sidecars {
                        ui.label(
                            egui::RichText::new(format!(
                                "    {} — {} · verified by git blob id",
                                f.rfilename,
                                human(f.size)
                            ))
                            .small()
                            .weak(),
                        );
                    }
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(
                            "Every byte is checked against these hashes, whoever serves it.",
                        )
                        .small()
                        .weak(),
                    );
                });
        });
}

/// Pick the recommended quant index by total size against the memory budget,
/// with a short reason shown only when it confidently fits.
fn recommend_quant_idx(sizes: &[u64], budget: u64) -> Option<(usize, String)> {
    if sizes.is_empty() {
        return None;
    }
    if budget == 0 {
        let mut idx: Vec<usize> = (0..sizes.len()).collect();
        idx.sort_by_key(|&i| sizes[i]);
        return Some((idx[idx.len() / 2], String::new()));
    }
    let cap = (budget as f64 / 1.2) as u64;
    let mut best: Option<usize> = None;
    for (i, &s) in sizes.iter().enumerate() {
        if s <= cap && best.map(|b| sizes[b] < s).unwrap_or(true) {
            best = Some(i);
        }
    }
    match best {
        Some(i) => Some((i, format!("fits your ~{} GB", budget / 1_000_000_000))),
        None => {
            let mut sm = 0;
            for i in 1..sizes.len() {
                if sizes[i] < sizes[sm] {
                    sm = i;
                }
            }
            Some((sm, String::new()))
        }
    }
}

/// One row for a GGUF quant split across several shard files: the combined size,
/// a shard count, and a single Download that fetches every shard as one model.
fn draw_quant_row(
    ui: &mut egui::Ui,
    app: &App,
    label: &str,
    files: &[&HfFile],
    recommended: bool,
    actions: &mut Vec<Action>,
) {
    let pal = pal_of(ui);
    let total: u64 = files.iter().map(|f| f.size).sum();
    let cached = !files.is_empty()
        && files.iter().all(|f| {
            f.sha256
                .as_ref()
                .map(|s| app.cached_sha256.contains(s))
                .unwrap_or(false)
        });
    let first = files[0].rfilename.clone();
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(12.0, 10.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(label).strong().size(15.0));
                        if recommended {
                            badge(ui, "Recommended", pal.blue);
                        }
                        badge(ui, &format!("{} shards", files.len()), pal.muted);
                        if cached {
                            badge(ui, "Downloaded", pal.muted);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(human(total)).small());
                        if hf_download_live(app) {
                            transport_badge(ui, "Hugging Face", Some(TransportKind::HuggingFace));
                        } else {
                            transport_chip(ui, "HF off", Some(TransportKind::HuggingFace), true);
                        }
                    });
                    ui.label(
                        egui::RichText::new("Split across several files, downloaded as one model.")
                            .small()
                            .weak(),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if cached {
                        if ui.button("Open").clicked() {
                            actions.push(Action::Reveal(PathBuf::from(&app.settings.models_dir)));
                        }
                    } else if ui
                        .add_enabled(
                            !app.busy,
                            egui::Button::new(egui::RichText::new("Download").strong()),
                        )
                        .clicked()
                    {
                        actions.push(Action::Download(first.clone()));
                    }
                });
            });
        });
}

fn draw_file_row(
    ui: &mut egui::Ui,
    app: &App,
    f: &HfFile,
    recommended: bool,
    actions: &mut Vec<Action>,
) {
    let pal = pal_of(ui);
    let cached = f
        .sha256
        .as_ref()
        .map(|s| app.cached_sha256.contains(s))
        .unwrap_or(false);
    let wpeers = f
        .sha256
        .as_ref()
        .and_then(|s| app.worldwide_peers.get(s))
        .copied()
        .unwrap_or(0);
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(12.0, 10.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Label::new(
                                    egui::RichText::new(f.variant_label()).strong().size(15.0),
                                )
                                .sense(egui::Sense::click()),
                            )
                            .on_hover_text("See routes & peers")
                            .clicked()
                        {
                            actions.push(Action::OpenQuantDetail(QuantDetail {
                                title: f.variant_label(),
                                subtitle: f
                                    .format()
                                    .map(|s| s.to_uppercase())
                                    .unwrap_or_else(|| "GGUF".into()),
                                size: f.size,
                                sha256: f.sha256.clone().unwrap_or_default(),
                                blake3: String::new(),
                                download: QuantDownload::Hf(f.rfilename.clone()),
                                cached,
                            }));
                        }
                        if recommended {
                            badge(ui, "Recommended", pal.blue)
                            .on_hover_text("Best balance of size and quality for most machines (typically a q4_k_m quant).");
                        }
                        if cached {
                            badge(ui, "Downloaded", pal.muted);
                        }
                        ui.label(egui::RichText::new("· routes").small().weak());
                    });
                    // Availability line: HF fallback state; worldwide peers when present.
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(human(f.size)).small());
                        if hf_download_live(app) {
                            transport_badge(ui, "Hugging Face", Some(TransportKind::HuggingFace));
                        } else if hf_download_pending(app) {
                            transport_chip(
                                ui,
                                "HF pending restart",
                                Some(TransportKind::HuggingFace),
                                true,
                            );
                        } else {
                            transport_chip(ui, "HF off", Some(TransportKind::HuggingFace), true);
                        }
                        if wpeers > 0 {
                            transport_badge(
                                ui,
                                format!("{wpeers} seeding worldwide"),
                                Some(TransportKind::Iroh),
                            );
                        }
                    });
                    draw_best_route_line(ui, app, wpeers, actions);
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if cached {
                        if ui.button("Open").clicked() {
                            actions.push(Action::Reveal(PathBuf::from(&app.settings.models_dir)));
                        }
                    } else if ui
                        .add_enabled(
                            !app.busy,
                            egui::Button::new(egui::RichText::new("Download").strong()),
                        )
                        .on_hover_text("Fetches from the fastest available peer and verifies every byte — switches automatically if one stalls.")
                        .clicked()
                    {
                        actions.push(Action::Download(f.rfilename.clone()));
                    }
                });
            });
            // Collapsible: sources & the content fingerprint (its P2P address).
            ui.add_space(2.0);
            egui::CollapsingHeader::new(egui::RichText::new("Sources & verification").small())
                .id_source(&f.rfilename)
                .show(ui, |ui| {
                    ui.label(egui::RichText::new(&f.rfilename).small().weak());
                    ui.add_space(2.0);
                    draw_route_list(ui, app, wpeers, actions);
                    ui.add_space(4.0);
                    // The content fingerprint *is* the P2P address.
                    if let Some(sha) = &f.sha256 {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Content ID (sha256):").small());
                            ui.monospace(egui::RichText::new(short_hash(sha)).small());
                            if ui.small_button("copy").clicked() {
                                actions.push(Action::CopyText {
                                    text: sha.clone(),
                                    what: "Content ID".into(),
                                });
                            }
                        });
                        ui.label(
                            egui::RichText::new("Every byte is checked against this hash, whoever serves it.")
                                .small()
                                .weak(),
                        );
                    }
                });
        });
}
fn draw_network(ui: &mut egui::Ui, app: &mut App, actions: &mut Vec<Action>) {
    let pal = pal_of(ui);
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.heading("Explore");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Refresh").clicked() {
                actions.push(Action::RefreshNetwork);
            }
            if app.network_loading {
                ui.spinner();
            }
        });
    });
    ui.label(
        egui::RichText::new("Models Noema peers are sharing right now — verified by content hash, one click to fetch over P2P. Pair your devices in Settings to see your own models here.")
            .small()
            .weak(),
    );
    ui.horizontal_wrapped(|ui| {
        transport_chip(
            ui,
            "Iroh mesh",
            Some(TransportKind::Iroh),
            app.worldwide.is_none(),
        );
        if app.last_network_error.is_some() {
            transport_chip(ui, "Tracker error", Some(TransportKind::Https), true);
        } else {
            transport_chip(ui, "Tracker live", Some(TransportKind::Https), false);
        }
        ui.label(
            egui::RichText::new(format!("{} row(s)", app.network.len()))
                .small()
                .weak(),
        );
    });
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let resp = ui.add_sized(
            [ui.available_width() - 90.0, 28.0],
            egui::TextEdit::singleline(&mut app.network_query).hint_text("Filter by name…"),
        );
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            actions.push(Action::RefreshNetwork);
        }
        if ui
            .add_sized([80.0, 28.0], egui::Button::new("Search"))
            .clicked()
        {
            actions.push(Action::RefreshNetwork);
        }
    });
    ui.add_space(8.0);

    if app.network.is_empty() {
        ui.add_space(28.0);
        ui.vertical_centered(|ui| {
            if app.network_loading {
                ui.spinner();
                ui.label(egui::RichText::new("Scanning the network…").weak());
            } else if let Some(err) = &app.last_network_error {
                ui.label(
                    egui::RichText::new("Couldn't reach the network")
                        .strong()
                        .color(pal.orange),
                );
                ui.label(egui::RichText::new(err).small().weak());
                ui.add_space(6.0);
                if ui.button("Try again").clicked() {
                    actions.push(Action::RefreshNetwork);
                }
            } else {
                ui.label(egui::RichText::new("No shared models found right now").strong());
                ui.label(
                    egui::RichText::new("Models you and others share worldwide show up here. Share one from your Library, or pair your devices in Settings > This device.")
                        .weak(),
                );
            }
        });
        return;
    }

    let mine: Vec<NetworkModel> = app.network.iter().filter(|m| m.mine).cloned().collect();
    let others: Vec<NetworkModel> = app.network.iter().filter(|m| !m.mine).cloned().collect();

    egui::ScrollArea::vertical().show(ui, |ui| {
        if !mine.is_empty() {
            ui.label(egui::RichText::new("From your devices").strong());
            ui.add_space(4.0);
            for m in &mine {
                network_row(ui, m, actions);
            }
            ui.add_space(12.0);
        }
        if !others.is_empty() {
            ui.label(egui::RichText::new("Shared on the network").strong());
            ui.add_space(4.0);
            for m in &others {
                network_row(ui, m, actions);
            }
        }
    });
}

fn network_row(ui: &mut egui::Ui, m: &NetworkModel, actions: &mut Vec<Action>) {
    let pal = pal_of(ui);
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    let name = if m.name.trim().is_empty() {
                        short_hash(&m.sha256)
                    } else {
                        m.name.clone()
                    };
                    if ui
                        .add(
                            egui::Label::new(egui::RichText::new(&name).strong())
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_text("See routes & peers")
                        .clicked()
                    {
                        actions.push(Action::OpenQuantDetail(QuantDetail {
                            title: name.clone(),
                            subtitle: if m.quant.trim().is_empty() {
                                String::new()
                            } else {
                                m.quant.trim().to_string()
                            },
                            size: m.size,
                            sha256: m.sha256.clone(),
                            blake3: m.blake3.clone(),
                            download: if m.mine || m.in_library {
                                QuantDownload::None
                            } else {
                                QuantDownload::Network(m.clone())
                            },
                            cached: m.in_library,
                        }));
                    }
                    ui.horizontal(|ui| {
                        if m.size > 0 {
                            ui.label(egui::RichText::new(human(m.size)).small().weak());
                        }
                        if !m.quant.trim().is_empty() {
                            badge(ui, m.quant.trim(), pal.blue);
                        }
                        if !m.license.is_empty() {
                            badge(ui, &m.license, pal.muted);
                        }
                        // Peer count means *other* devices that have this file
                        // (the tracker already excludes you). For your own shares,
                        // say "on your devices" rather than counting yourself.
                        let blue = pal.blue;
                        if m.mine && m.peers == 0 {
                            badge(ui, "On your devices", blue);
                        } else if m.mine {
                            let n = m.peers;
                            let label = if n == 1 {
                                "+1 other peer".to_string()
                            } else {
                                format!("+{n} other peers")
                            };
                            badge(ui, "On your devices", blue);
                            transport_badge(ui, &label, Some(TransportKind::Iroh));
                        } else {
                            let label = if m.peers == 1 {
                                "1 peer".to_string()
                            } else {
                                format!("{} peers", m.peers)
                            };
                            transport_badge(ui, &label, Some(TransportKind::Iroh));
                        }
                        if !m.devices.is_empty() {
                            ui.label(
                                egui::RichText::new(format!("from {}", m.devices.join(", ")))
                                    .small()
                                    .weak(),
                            );
                        }
                    });
                    ui.label(
                        egui::RichText::new(format!("Verified · BLAKE3 {}", short_hash(&m.blake3)))
                            .small()
                            .color(pal.green),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if m.mine {
                        // It's your own share — you already have it; never offer to
                        // "download" a file from yourself.
                        let label = if m.in_library {
                            "In library"
                        } else {
                            "Seeding"
                        };
                        ui.label(egui::RichText::new(label).small().weak());
                    } else if m.in_library {
                        ui.label(egui::RichText::new("In library").small().weak());
                    } else if ui
                        .button("Download")
                        .on_hover_text("Fetches from worldwide peers and verifies every byte.")
                        .clicked()
                    {
                        actions.push(Action::AddFromNetwork(m.clone()));
                    }
                });
            });
        });
    ui.add_space(5.0);
}

fn draw_transfers(ui: &mut egui::Ui, app: &App, actions: &mut Vec<Action>) {
    let pal = pal_of(ui);
    ui.add_space(8.0);
    ui.heading("Transfers");
    ui.add_space(8.0);
    ui.columns(2, |cols| {
        cols[0].horizontal(|ui| {
            dir_arrow(ui, true, pal.blue_dl);
            ui.label(
                egui::RichText::new(format!("Download   {}/s", human(app.cur_dl_bps as u64)))
                    .strong(),
            );
        });
        sparkline(&mut cols[0], &app.dl_samples, pal.blue_dl);
        cols[1].horizontal(|ui| {
            dir_arrow(ui, false, pal.green);
            ui.label(
                egui::RichText::new(format!("Upload   {}/s", human(app.cur_ul_bps as u64)))
                    .strong(),
            );
        });
        sparkline(&mut cols[1], &app.ul_samples, pal.green);
    });
    ui.add_space(12.0);
    ui.label(egui::RichText::new("Active").strong());
    if let Some(a) = &app.active {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&a.name).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // right_to_left lays out from the right, so push Stop first to
                    // keep the visual order "Pause  Stop".
                    if ui
                        .button("Stop")
                        .on_hover_text("Stop this download and discard its progress. The partial download is deleted.")
                        .clicked()
                    {
                        actions.push(Action::StopDownload);
                    }
                    if ui
                        .button("Pause")
                        .on_hover_text("Pause this download. Progress so far is saved — download again to resume.")
                        .clicked()
                    {
                        actions.push(Action::PauseDownload);
                    }
                });
            });
            if a.verifying {
                // The network transfer is done (Iroh fetched the whole blob); the
                // engine is re-reading it locally to verify integrity. Keep the bar
                // full so it doesn't look like a second download, and show the
                // verify pass's own progress as the caption.
                let vfrac = if a.total > 0 {
                    (a.verify_done as f32 / a.total as f32).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                ui.add(
                    egui::ProgressBar::new(1.0)
                        .text(format!("Verifying… {}%", (vfrac * 100.0) as u32)),
                );
                ui.label(
                    egui::RichText::new(format!(
                        "Verifying integrity   ·   {} / {}",
                        human(a.verify_done),
                        human(a.total),
                    ))
                    .small()
                    .weak(),
                );
            } else {
                let frac = if a.total > 0 {
                    a.done as f32 / a.total as f32
                } else {
                    0.0
                };
                ui.add(egui::ProgressBar::new(frac).show_percentage());
                // ETA from a smoothed rate (the instantaneous sample whipsaws); show
                // "calculating…" for the first few seconds instead of a bogus "—".
                let smooth = smoothed_bps(&app.dl_samples);
                let eta = if smooth > 1.0 {
                    format!(
                        "~{} left",
                        dur((a.total.saturating_sub(a.done)) as f64 / smooth)
                    )
                } else if a.started.elapsed().as_secs() < 4 {
                    "calculating…".into()
                } else {
                    "—".into()
                };
                ui.label(
                    egui::RichText::new(format!(
                        "{} / {}   ·   {}/s   ·   {}",
                        human(a.done),
                        human(a.total),
                        human(app.cur_dl_bps as u64),
                        eta
                    ))
                    .small()
                    .weak(),
                );
            }
            ui.horizontal_wrapped(|ui| {
                if let Some(source) = &a.source {
                    let kind = TransportKind::from_source_id(source);
                    let route_chip = transport_chip(ui, source, Some(kind), false);
                    if let Some(last) = a.route_history.last() {
                        route_chip.on_hover_text(format!(
                            "{} active for {}",
                            transport_source_label(&last.source_id),
                            dur(last.started_at.elapsed().as_secs_f64())
                        ));
                    }
                    let pulse = a
                        .switched_at
                        .map(|t| t.elapsed().as_secs_f32() < 2.0)
                        .unwrap_or(false);
                    let color = if pulse {
                        kind.color_on(ui.visuals().dark_mode)
                    } else {
                        pal.muted
                    };
                    ui.label(
                        egui::RichText::new(format!("via {}", source_label(Some(source))))
                            .small()
                            .color(color),
                    );
                    if let Some(last) = a.route_history.last() {
                        if let Some(reason) = &last.reason {
                            ui.label(egui::RichText::new(reason).small().weak());
                        } else if last.start_offset > 0 {
                            ui.label(
                                egui::RichText::new(format!(
                                    "resumed from {}",
                                    human(last.start_offset)
                                ))
                                .small()
                                .weak(),
                            );
                        }
                    }
                } else {
                    ui.label(egui::RichText::new("choosing route…").small().weak());
                }
            });
            // Per-source breakdown — the multi-source story made visible.
            let mut by_source: Vec<(&String, &u64)> =
                a.by_source.iter().filter(|(_, b)| **b > 0).collect();
            if by_source.len() > 1 {
                by_source.sort_by(|x, y| y.1.cmp(x.1));
                let parts: Vec<String> = by_source
                    .iter()
                    .map(|(sid, b)| format!("{} {}", transport_source_label(sid), human(**b)))
                    .collect();
                ui.label(
                    egui::RichText::new(parts.join("   ·   "))
                        .small()
                        .weak(),
                );
            }
        });
    } else {
        ui.label(egui::RichText::new("No active transfers.").weak());
        let iroh_uploads = app
            .worldwide
            .as_ref()
            .map(|w| w.metrics().active_uploads())
            .unwrap_or(0);
        if iroh_uploads > 0 {
            ui.horizontal_wrapped(|ui| {
                transport_chip(ui, "Iroh upload", Some(TransportKind::Iroh), false);
                ui.label(
                    egui::RichText::new(format!(
                        "{} active {} · {}/s",
                        iroh_uploads,
                        plural(iroh_uploads as usize, "peer"),
                        human(app.cur_ul_bps as u64)
                    ))
                    .small(),
                );
            });
        }
        // Idle real-estate: surface the seeding contribution rather than a blank.
        let shared = app.installed.iter().filter(|m| m.shareable).count();
        if app.worldwide.is_some() && shared > 0 {
            ui.label(
                egui::RichText::new(format!(
                    "Sharing {shared} model{} worldwide · {} uploaded this session",
                    if shared == 1 { "" } else { "s" },
                    human(app.session_uploaded),
                ))
                .small()
                .weak(),
            );
        }
    }

    // Per-model live upload activity: which of your shared models peers are
    // pulling from you right now.
    {
        let rows: Vec<(String, u64)> = if app.worldwide.is_some() {
            app.installed
                .iter()
                .filter(|m| m.shareable)
                .map(|m| {
                    let n = app
                        .worldwide
                        .as_ref()
                        .map(|w| w.active_uploads_for(&m.blake3))
                        .unwrap_or(0);
                    (m.name.clone(), n)
                })
                .collect()
        } else {
            Vec::new()
        };
        if !rows.is_empty() {
            ui.add_space(12.0);
            ui.label(egui::RichText::new("Sharing now").strong());
            let pal = pal_of(ui);
            for (name, n) in rows {
                egui::Frame::group(ui.style())
                    .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(name).small());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if n > 0 {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "↑ {} {} pulling",
                                                n,
                                                plural(n as usize, "peer")
                                            ))
                                            .small()
                                            .strong()
                                            .color(pal.green),
                                        );
                                    } else {
                                        ui.label(egui::RichText::new("idle").small().weak());
                                    }
                                },
                            );
                        });
                    });
                ui.add_space(4.0);
            }
        }
    }

    ui.add_space(12.0);
    ui.label(egui::RichText::new("This session").strong());
    egui::Grid::new("session")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            ui.label("Downloaded");
            ui.label(human(app.cumulative_dl));
            ui.end_row();
            ui.label("Uploaded (shared worldwide)");
            ui.label(human(app.session_uploaded));
            ui.end_row();
            ui.label("Sharing");
            ui.label(if app.worldwide.is_some() {
                "Worldwide (Iroh)"
            } else {
                "off"
            });
            ui.end_row();
        });
}

/// A smoothed download rate (bytes/s) for ETA: the mean of the most recent
/// speed samples. The instantaneous 0.5 s sample whipsaws on any momentary
/// stall, which makes a raw ETA jump around; averaging a short window steadies it.
fn smoothed_bps(samples: &VecDeque<f64>) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let n = samples.len().min(8);
    let sum: f64 = samples.iter().rev().take(n).sum();
    sum / n as f64
}

/// A small painted direction arrow (down = download, up = upload). Drawn rather
/// than typed so it always renders, regardless of the bundled font's glyph set.
fn dir_arrow(ui: &mut egui::Ui, down: bool, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 11.0), egui::Sense::hover());
    let pts = if down {
        vec![
            egui::pos2(rect.center().x, rect.bottom()),
            egui::pos2(rect.left(), rect.top() + 2.0),
            egui::pos2(rect.right(), rect.top() + 2.0),
        ]
    } else {
        vec![
            egui::pos2(rect.center().x, rect.top()),
            egui::pos2(rect.left(), rect.bottom() - 2.0),
            egui::pos2(rect.right(), rect.bottom() - 2.0),
        ]
    };
    ui.painter()
        .add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
}

fn sparkline(ui: &mut egui::Ui, samples: &VecDeque<f64>, color: egui::Color32) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 70.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
    let max = samples.iter().cloned().fold(1.0_f64, f64::max);
    if samples.len() >= 2 {
        let n = samples.len();
        let pts: Vec<egui::Pos2> = samples
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let x = rect.left() + rect.width() * (i as f32 / (n - 1) as f32);
                let y = rect.bottom() - (rect.height() - 4.0) * (*v / max) as f32;
                egui::pos2(x, y)
            })
            .collect();
        painter.add(egui::Shape::line(pts, egui::Stroke::new(1.5, color)));
    }
    painter.text(
        rect.left_top() + egui::vec2(4.0, 2.0),
        egui::Align2::LEFT_TOP,
        format!("peak {}/s", human(max as u64)),
        egui::FontId::proportional(10.0),
        ui.visuals().weak_text_color(),
    );
}

fn draw_library(ui: &mut egui::Ui, app: &App, actions: &mut Vec<Action>) {
    let pal = pal_of(ui);
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.heading("Your models");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Refresh").clicked() {
                actions.push(Action::Refresh);
            }
            if ui
                .add_enabled(
                    !app.busy,
                    egui::Button::new(egui::RichText::new("＋ Share a model…").strong()),
                )
                .on_hover_text("Bring in any GGUF/safetensors file you already have — title it, set a license, and send it to a friend or publish it. Works whether or not it's on Hugging Face.")
                .clicked()
            {
                actions.push(Action::OpenComposer);
            }
        });
    });
    ui.label(
        egui::RichText::new(format!(
            "{} model(s) downloaded · saved in {}",
            app.installed.len(),
            app.settings.models_dir
        ))
        .small()
        .weak(),
    );
    ui.add_space(6.0);
    if app.installed.is_empty() {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            ui.label(egui::RichText::new("No models yet").strong());
            ui.label(
                egui::RichText::new(
                    "Download one from Discover — or drag a model file onto this window to share it. Atlas reads its name and quant straight from the file.",
                )
                .weak(),
            );
            ui.add_space(10.0);
            if ui
                .add_enabled(
                    !app.busy,
                    egui::Button::new(egui::RichText::new("＋ Share a model…").strong()),
                )
                .clicked()
            {
                actions.push(Action::OpenComposer);
            }
        });
        return;
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        for m in &app.installed {
            egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(&m.name).strong());
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(human(m.size_bytes)).small().weak());
                                if m.from_hf {
                                    badge(
                                        ui,
                                        "Hugging Face",
                                        pal.amber,
                                    );
                                } else {
                                    badge(ui, "Local import", pal.muted);
                                }
                                if m.shareable {
                                    if app.worldwide.is_some() {
                                        transport_badge(ui, "Iroh live", Some(TransportKind::Iroh));
                                    } else {
                                        transport_chip(ui, "Shareable", Some(TransportKind::Iroh), true);
                                    }
                                    if ui
                                        .add(
                                            egui::Label::new(
                                                egui::RichText::new("routes ›").small().weak(),
                                            )
                                            .sense(egui::Sense::click()),
                                        )
                                        .on_hover_text("See every protocol this is reachable on, and peers")
                                        .clicked()
                                    {
                                        actions.push(Action::OpenQuantDetail(QuantDetail {
                                            title: m.name.clone(),
                                            subtitle: m.quant.clone().unwrap_or_default(),
                                            size: m.size_bytes,
                                            sha256: m.sha256.clone(),
                                            blake3: m.blake3.clone(),
                                            download: QuantDownload::None,
                                            cached: true,
                                        }));
                                    }
                                } else if m.gated {
                                    badge(ui, "Private (gated)", pal.muted);
                                } else {
                                    badge(ui, "Not shared", pal.muted);
                                }
                            });
                            // Provenance line — the product's reason to exist,
                            // made visible per model: verified, with a copyable
                            // content fingerprint.
                            ui.add_space(1.0);
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "Verified · BLAKE3 {}",
                                        short_hash(&m.blake3)
                                    ))
                                    .small()
                                    .color(pal.green),
                                );
                                if ui.small_button("copy id").clicked() {
                                    let id = if m.sha256.is_empty() {
                                        m.blake3.clone()
                                    } else {
                                        m.sha256.clone()
                                    };
                                    actions.push(Action::CopyText {
                                        text: id,
                                        what: "Content ID".into(),
                                    });
                                }
                                if !m.license.trim().is_empty() {
                                    ui.label(
                                        egui::RichText::new(format!("· {}", m.license))
                                            .small()
                                            .weak(),
                                    );
                                }
                            });
                            ui.add_space(2.0);
                            ui.horizontal(|ui| {
                                let mut shared = m.shareable;
                                let label = if m.gated {
                                    "Share this gated model"
                                } else {
                                    "Share on the open mesh"
                                };
                                if ui
                                    .checkbox(&mut shared, label)
                                    .on_hover_text("When on, anyone can find this model in Explore and fetch it from you over P2P.")
                                    .changed()
                                {
                                    actions.push(Action::ShareModel {
                                        blake3: m.blake3.clone(),
                                        sha256: m.sha256.clone(),
                                        on: shared,
                                    });
                                }
                                if ui
                                    .small_button("Copy link")
                                    .on_hover_text("Copy a direct share link (paste it on another device under Discover > Add by Content ID)")
                                    .clicked()
                                {
                                    let link = noema_core::ShareTarget {
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
                                    .encode();
                                    actions.push(Action::CopyShareLink(link));
                                }
                                if ui
                                    .small_button("Edit / title…")
                                    .on_hover_text("Set a title, license, and description for this model before sharing it.")
                                    .clicked()
                                {
                                    actions.push(Action::EditModel(m.manifest_id.clone()));
                                }
                            });
                            if m.shareable {
                                let line = if app.worldwide.is_some() {
                                    "Live on Explore — anyone can find this and download it from you over Iroh."
                                } else {
                                    "Ready to share — turn on Worldwide P2P in Settings to announce it on Explore."
                                };
                                ui.label(egui::RichText::new(line).small().weak());
                            } else if m.gated {
                                ui.label(
                                    egui::RichText::new(
                                        "Gated model — kept private by default. Tick to share it anyway (this redistributes the weights worldwide).",
                                    )
                                    .small()
                                    .weak()
                                    .color(pal.amber_dim),
                                );
                            } else if m.license.trim().is_empty()
                                || m.license.eq_ignore_ascii_case("unknown")
                            {
                                ui.label(
                                    egui::RichText::new(
                                        "License unknown — set one with “Edit / title…” so it can be reshared. Only publish weights you have the right to redistribute.",
                                    )
                                    .small()
                                    .weak()
                                    .color(pal.amber_dim),
                                );
                            }
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button("DELETE")
                                .on_hover_text("Delete from disk and stop sharing with peers")
                                .clicked()
                            {
                                actions.push(Action::RequestDelete(PendingDelete {
                                    name: m.name.clone(),
                                    blake3: m.blake3.clone(),
                                    size_bytes: m.size_bytes,
                                    install_path: m.install_path.clone(),
                                    shareable: m.shareable,
                                }));
                            }
                            if ui.button("Open").clicked() {
                                let p = m
                                    .install_path
                                    .clone()
                                    .map(|s| {
                                        let pb = PathBuf::from(&s);
                                        pb.parent().map(|x| x.to_path_buf()).unwrap_or(pb)
                                    })
                                    .unwrap_or_else(|| PathBuf::from(&app.settings.models_dir));
                                actions.push(Action::Reveal(p));
                            }
                        });
                    });
                });
            ui.add_space(5.0);
        }
    });
}

fn draw_settings(ui: &mut egui::Ui, app: &mut App, actions: &mut Vec<Action>) {
    let pal = pal_of(ui);
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.add_space(8.0);
        ui.heading("Settings");
        ui.add_space(12.0);
        ui.label(egui::RichText::new("Appearance").strong());
        ui.horizontal(|ui| {
            ui.label("Theme");
            let mut mode = app.settings.theme;
            ui.selectable_value(&mut mode, ThemeMode::Dark, "Dark");
            ui.selectable_value(&mut mode, ThemeMode::Light, "Light");
            if mode != app.settings.theme {
                actions.push(Action::SetTheme(mode));
            }
        });
        ui.label(
            egui::RichText::new("Switches the whole UI instantly and is saved for next time.")
                .small()
                .weak(),
        );

        // Restart-required banner: the engine reads tracker/mirror/proxy/seed
        // settings only at startup, so flag when an edit isn't live yet.
        if conn_snapshot(&app.settings) != app.applied_connection {
            ui.add_space(6.0);
            egui::Frame::none()
                .fill(pal.amber_bg)
                .rounding(6.0)
                .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                .show(ui, |ui| {
                    ui.colored_label(
                        pal.amber,
                        "Some changes below apply after you restart the app.",
                    );
                });
        }
        ui.add_space(12.0);
        ui.label(egui::RichText::new("Worldwide P2P").strong());
        ui.label(
            egui::RichText::new("Openly-licensed models you download are seeded to the open mesh by default, so anyone can find them in Explore and fetch them from you over Iroh (NAT-traversing, no ports to open). Privately-imported files stay local until you opt them in, and you can stop sharing any single model from the Library.")
                .small()
                .weak(),
        );
        let mut ww = app.settings.share_worldwide;
        if ui.checkbox(&mut ww, "Seed my models to the world").changed() {
            actions.push(Action::ToggleWorldwide);
        }
        // Opt-in: also auto-share gated/licensed models (off by default).
        let mut sg = app.settings.share_gated;
        if ui
            .checkbox(&mut sg, "Also share gated / licensed models")
            .on_hover_text("Off by default. When on, gated/token-walled and licensed models you download are seeded too — Atlas verifies content, not licenses, so redistribution compliance is your responsibility. Applies immediately. (Per-model sharing can still be toggled in the Library either way.)")
            .changed()
        {
            actions.push(Action::SetShareGated(sg));
        }
        ui.horizontal(|ui| {
            ui.label("Tracker");
            if ui
                .add(egui::TextEdit::singleline(&mut app.settings.tracker_url).desired_width(320.0))
                .on_hover_text("Restart the app after changing this")
                .lost_focus()
            {
                actions.push(Action::SaveSettings);
            }
        });
        if let Some(w) = &app.worldwide {
            let shared = app.installed.iter().filter(|m| m.shareable).count();
            if shared > 0 {
                ui.colored_label(
                    pal.green,
                    format!(
                        "Sharing {shared} model{} worldwide.",
                        if shared == 1 { "" } else { "s" }
                    ),
                );
            } else {
                ui.colored_label(
                    pal.amber_dim,
                    "Seeding to the world — no models in your Library yet.",
                );
                ui.label(
                    egui::RichText::new(
                        "Download a model from Explore and it's instantly shared with everyone.",
                    )
                    .small()
                    .weak(),
                );
            }
            ui.label(
                egui::RichText::new(format!("your node: {}…", &w.node_ticket()[..w.node_ticket().len().min(40)]))
                    .small()
                    .weak()
                    .monospace(),
            );
        }
        ui.add_space(14.0);
        ui.label(egui::RichText::new("Connection").strong());
        ui.label(
            egui::RichText::new("For restricted or slow networks. Changes apply after restarting the app.")
                .small()
                .weak(),
        );

        let mut allow_hf = app.settings.allow_hf_download;
        if ui
            .checkbox(&mut allow_hf, "Allow Hugging Face downloads (applies immediately)")
            .on_hover_text("Atlas still searches Hugging Face either way. This only allows downloading model bytes from Hugging Face when no peer has them yet. Takes effect right away — no restart.")
            .changed()
        {
            // Applies live — no restart (unlike the mirror/proxy settings below).
            actions.push(Action::SetHfDownload(allow_hf));
        }
        ui.label(
            egui::RichText::new("Atlas is peer-to-peer first. Searching still uses Hugging Face; turn this on to also download weights from Hugging Face when no peer has the file yet.")
                .small()
                .weak(),
        );

        ui.add_space(6.0);
        // Hugging Face mirror — search + downloads behave exactly like the real Hub.
        let mut mirror = app.settings.hf_mirror_enabled;
        if ui
            .checkbox(&mut mirror, "Use a Hugging Face mirror")
            .on_hover_text("Routes model search and downloads through a mirror of huggingface.co")
            .changed()
        {
            app.settings.hf_mirror_enabled = mirror;
            actions.push(Action::SaveSettings);
        }
        ui.horizontal(|ui| {
            ui.add_space(20.0);
            ui.label("Mirror URL");
            let field = ui.add_enabled(
                app.settings.hf_mirror_enabled,
                egui::TextEdit::singleline(&mut app.settings.hf_mirror_url)
                    .desired_width(300.0)
                    .hint_text("https://hf-mirror.com"),
            );
            if field.lost_focus() {
                actions.push(Action::SaveSettings);
            }
        });

        // Proxy / "VPN tunnel" — routes all internet HTTP traffic (HF, tracker, IPFS).
        ui.add_space(6.0);
        let mut proxy_on = app.settings.proxy_enabled;
        if ui
            .checkbox(&mut proxy_on, "Route traffic through a proxy (VPN tunnel)")
            .on_hover_text("Tunnels Hugging Face, tracker, and IPFS traffic. Local-network sharing is not tunneled.")
            .changed()
        {
            app.settings.proxy_enabled = proxy_on;
            actions.push(Action::SaveSettings);
        }
        ui.horizontal(|ui| {
            ui.add_space(20.0);
            ui.label("Proxy URL");
            let field = ui.add_enabled(
                app.settings.proxy_enabled,
                egui::TextEdit::singleline(&mut app.settings.proxy_url)
                    .desired_width(300.0)
                    .hint_text("socks5://127.0.0.1:1080"),
            );
            if field.lost_focus() {
                actions.push(Action::SaveSettings);
            }
        });
        ui.horizontal(|ui| {
            ui.add_space(20.0);
            ui.label(
                egui::RichText::new("Supports http://, https://, socks5:// and socks5h:// (socks5h tunnels DNS too).")
                    .small()
                    .weak(),
            );
        });
        ui.add_space(12.0);
        ui.label(egui::RichText::new("This device").strong());
        ui.horizontal(|ui| {
            ui.label("Device name");
            if ui
                .add(
                    egui::TextEdit::singleline(&mut app.settings.device_name)
                        .desired_width(220.0)
                        .hint_text("e.g. Armin's MacBook"),
                )
                .on_hover_text("Shown next to models you're seeding, so people see who's sharing")
                .lost_focus()
            {
                actions.push(Action::ApplyIdentity);
            }
        });

        // Optional, de-emphasized: link your own devices to highlight them.
        egui::CollapsingHeader::new("Link my own devices (optional)")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new("Sharing is public by default, so your other devices already see your models in Explore. A group code just adds a “From your devices” shortcut on the Explore tab. Use the same code on each of your devices.")
                        .small()
                        .weak(),
                );
                ui.horizontal(|ui| {
                    ui.label("Group code");
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut app.settings.group_code)
                                .desired_width(200.0)
                                .hint_text("paste a code, or create one"),
                        )
                        .lost_focus()
                    {
                        actions.push(Action::ApplyIdentity);
                    }
                    if !app.settings.group_code.trim().is_empty()
                        && ui.small_button("Copy").clicked()
                    {
                        actions.push(Action::CopyText {
                            text: app.settings.group_code.clone(),
                            what: "Group code".into(),
                        });
                    }
                    if ui.small_button("Create").clicked() {
                        actions.push(Action::CreateGroup);
                    }
                });
            });
        ui.add_space(14.0);
        ui.label(egui::RichText::new("Speed").strong());
        ui.horizontal(|ui| {
            ui.label("Download limit (Mbps, 0 = unlimited)");
            let mut v = app.settings.download_cap_mbps as f64;
            if ui.add(egui::DragValue::new(&mut v).range(0.0..=10_000.0).speed(1.0)).changed() {
                app.settings.download_cap_mbps = v as u32;
            }
            if ui.button("Apply").clicked() {
                actions.push(Action::ApplySpeedCap);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Parallel connections (1 = off)")
                .on_hover_text("Splits a large download into this many simultaneous range requests (aria2-style) for faster Hugging Face / HTTPS fetches. Applies to the next download.");
            let mut c = app.settings.download_connections as f64;
            if ui
                .add(egui::DragValue::new(&mut c).range(1.0..=8.0).speed(0.1))
                .changed()
            {
                app.settings.download_connections = c as u32;
            }
            if ui.button("Apply").clicked() {
                actions.push(Action::ApplyDownloadConnections);
            }
        });
        ui.add_space(14.0);
        ui.label(egui::RichText::new("Downloads").strong());
        let mut skip = app.settings.skip_download_confirm;
        if ui
            .checkbox(
                &mut skip,
                "Download share links & Explore models immediately",
            )
            .changed()
        {
            app.settings.skip_download_confirm = skip;
            actions.push(Action::SaveSettings);
        }
        ui.label(
            egui::RichText::new("When off (default), opening a share link or an Explore result shows a quick confirmation first. Turn on to skip it and fetch in one click.")
                .small()
                .weak(),
        );
        ui.add_space(14.0);
        ui.label(egui::RichText::new("Storage").strong());
        ui.horizontal(|ui| {
            ui.label("Save models to");
            if ui
                .add(egui::TextEdit::singleline(&mut app.settings.models_dir).desired_width(300.0))
                .lost_focus()
            {
                actions.push(Action::SaveSettings);
            }
            if ui.button("Browse…").clicked() {
                if let Some(dir) = rfd::FileDialog::new()
                    .set_directory(&app.settings.models_dir)
                    .pick_folder()
                {
                    app.settings.models_dir = dir.to_string_lossy().into_owned();
                    actions.push(Action::SaveSettings);
                }
            }
        });
        let total: u64 = app.cache.iter().map(|b| b.size_bytes).sum();
        ui.label(
            egui::RichText::new(format!(
                "Cache: {} across {} file(s) (identical files stored once)",
                human(total),
                app.cache.len()
            ))
            .small()
            .weak(),
        );
        ui.horizontal(|ui| {
            if ui
                .button("Clear unused cache")
                .on_hover_text("Removes cached blobs not referenced by any installed model. Your downloaded models are not affected; anything cleared is re-fetched on demand.")
                .clicked()
            {
                actions.push(Action::Evict(EvictPolicy::Unreferenced));
            }
        });
        ui.add_space(14.0);
        ui.label(egui::RichText::new("Hugging Face account").strong());
        ui.horizontal(|ui| {
            ui.label(if app.has_token { "Signed in" } else { "Not signed in" });
            if ui.button(if app.has_token { "Change token" } else { "Add token" }).clicked() {
                app.show_token = true;
            }
        });
        ui.add_space(14.0);
        ui.label(egui::RichText::new("About").strong());
        ui.label(
            egui::RichText::new(format!(
                "Noema Atlas v{} · verified multi-source model downloader · BLAKE3 + SHA-256 content addressing",
                env!("CARGO_PKG_VERSION")
            ))
            .small()
            .weak(),
        );
    });
}

fn badge(ui: &mut egui::Ui, text: &str, color: egui::Color32) -> egui::Response {
    egui::Frame::none()
        .stroke(egui::Stroke::new(1.0, color))
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(6.0, 1.0))
        .show(ui, |ui| {
            ui.colored_label(color, egui::RichText::new(text).small());
        })
        .response
}

fn note_route_progress(
    active: &mut ActiveDownload,
    source: Option<&str>,
    phase: &str,
    failover_reason: Option<String>,
    effective_start: Option<u64>,
) -> Option<String> {
    if let Some(reason) = failover_reason {
        active.pending_failover_reason = Some(reason);
    }
    let sid = source?;
    if phase == "source-failed" {
        return None;
    }

    let now = Instant::now();
    let sid = sid.to_string();
    let start_offset = effective_start.unwrap_or(0);
    match active.source.clone() {
        None => {
            active.source = Some(sid.clone());
            let reason = active.pending_failover_reason.take();
            active.route_history.push(RouteLeg {
                source_id: sid,
                started_at: now,
                reason: reason.clone(),
                start_offset,
            });
            if let Some(reason) = reason {
                Some(format!(
                    "{} — now from {}",
                    reason,
                    source_label(active.source.as_deref())
                ))
            } else if start_offset > 0 {
                Some(format!(
                    "Resuming — {} already verified",
                    human(start_offset)
                ))
            } else {
                None
            }
        }
        Some(prev) if prev != sid => {
            let pair = (prev.clone(), sid.clone());
            let duplicate = !active.seen_switch_pairs.insert(pair);
            active.source = Some(sid.clone());
            let reason = active.pending_failover_reason.take();
            active.route_history.push(RouteLeg {
                source_id: sid.clone(),
                started_at: now,
                reason: reason.clone(),
                start_offset,
            });
            active.switched_at = Some(now);
            if duplicate {
                None
            } else if let Some(reason) = reason {
                Some(format!(
                    "{} — now from {}",
                    reason,
                    source_label(Some(&sid))
                ))
            } else {
                Some(format!(
                    "Switched from {} to {}",
                    source_label(Some(&prev)),
                    source_label(Some(&sid))
                ))
            }
        }
        _ => None,
    }
}

fn route_summary(active: &ActiveDownload) -> String {
    let mut by_source: Vec<(&String, &u64)> =
        active.by_source.iter().filter(|(_, b)| **b > 0).collect();
    by_source.sort_by(|a, b| b.1.cmp(a.1));
    if by_source.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = by_source
        .iter()
        .map(|(sid, b)| format!("{} {}", transport_source_label(sid), human(**b)))
        .collect();
    format!("fetched from {} — verified", parts.join(" + "))
}

fn transport_source_label(source_id: &str) -> &'static str {
    match TransportKind::from_source_id(source_id) {
        TransportKind::Iroh => "worldwide peer",
        TransportKind::Ipfs => "IPFS",
        TransportKind::Https => "HTTPS mirror",
        TransportKind::HuggingFace => "Hugging Face",
        TransportKind::File => "local file",
        TransportKind::Unknown => "source",
    }
}

fn source_label(source: Option<&str>) -> String {
    match source {
        Some(s) => match TransportKind::from_source_id(s) {
            TransportKind::HuggingFace => "Hugging Face".into(),
            TransportKind::Iroh => "a worldwide peer".into(),
            TransportKind::Ipfs => "IPFS".into(),
            TransportKind::Https => "an HTTPS mirror".into(),
            TransportKind::File => "a local file".into(),
            TransportKind::Unknown => s.to_string(),
        },
        None => "—".into(),
    }
}

fn friendly_error(e: &str) -> String {
    let low = e.to_lowercase();
    if low.contains("auth") || low.contains("401") || low.contains("403") {
        "This model is gated — add your Hugging Face token (top-right) and accept its terms.".into()
    } else if low.contains("no eligible source") && low.contains("hugging face downloads are off") {
        "No peers have this yet — turn on Hugging Face downloads in Settings (applies right away), or import a copy to seed it.".into()
    } else if (low.contains("iroh")
        || low.contains("not found")
        || low.contains("no eligible source"))
        && !low.contains("hugging face")
    {
        // A content-id / share-link fetch with no live seeders surfaces as an
        // iroh "not found" / "no eligible source" — say it in plain language.
        "No one is seeding this yet — no peers currently have this file. Try again later, or ask whoever shared it to keep their app open.".into()
    } else if low.contains("dns") || low.contains("connect") || low.contains("timeout") {
        "Couldn't reach the network — check your internet connection.".into()
    } else {
        format!("Something went wrong: {e}")
    }
}

fn push_cap(buf: &mut VecDeque<f64>, v: f64, cap: usize) {
    buf.push_back(v);
    while buf.len() > cap {
        buf.pop_front();
    }
}

fn counter_delta(current: u64, mark: &mut u64) -> u64 {
    let delta = current.saturating_sub(*mark);
    *mark = current;
    delta
}

/// Fold one progress update into the session byte counter without counting
/// cache hits, peer discovery, verification re-reads, or resumed prefixes.
fn fold_download_progress(
    prev_done: &mut u64,
    baselined: &mut bool,
    done: u64,
    phase: &str,
    effective_start: Option<u64>,
) -> u64 {
    if let Some(start) = effective_start {
        // Attempt (re)start. A nonzero start is a trustworthy resume offset from an
        // engine-temp transport (e.g. HTTPS); zero is also the in-`open()`
        // transports' placeholder, so their true baseline is deferred to the first
        // transfer event below.
        *prev_done = start;
        *baselined = false;
        return 0;
    }
    if !matches!(phase, "downloading" | "verified") {
        // cache-hit / discovering peers / source-failed: no bytes moved.
        return 0;
    }
    if !*baselined {
        *baselined = true;
        // The connecting baseline was zero (a fresh transfer, or an in-`open()`
        // transport that resumes opaquely): rebaseline to where bytes actually
        // resume so the first delta isn't the entire resumed prefix.
        if *prev_done == 0 {
            *prev_done = done;
        }
    }
    if done > *prev_done {
        let delta = done - *prev_done;
        *prev_done = done;
        delta
    } else {
        0
    }
}

fn short_hash(s: &str) -> String {
    if s.len() > 20 {
        format!("{}…{}", &s[..10], &s[s.len() - 6..])
    } else {
        s.to_string()
    }
}

fn dur(secs: f64) -> String {
    let s = secs as u64;
    if s >= 3600 {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

fn human(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn default_models_dir() -> PathBuf {
    if let Ok(h) = std::env::var("HOME") {
        if !h.is_empty() {
            return PathBuf::from(h).join("Noema Models");
        }
    }
    if let Ok(h) = std::env::var("USERPROFILE") {
        if !h.is_empty() {
            return PathBuf::from(h).join("Noema Models");
        }
    }
    PathBuf::from("Noema Models")
}

fn load_settings(root: &Path) -> Settings {
    match std::fs::read(root.join("ui-settings.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

fn reveal(path: &Path) {
    let _ = std::fs::create_dir_all(path);
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

#[cfg(test)]
mod download_progress_tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;
    const CHUNK: u64 = 4 * MIB; // the engine emits "downloading" about every 4 MiB

    /// Replay `(phase, done, effective_start)` events through
    /// `fold_download_progress`, returning `(total_counted, max_single_delta)`.
    /// `max_single_delta` is what would drive the graph — a spike is one huge value.
    fn replay(events: &[(&str, u64, Option<u64>)]) -> (u64, u64) {
        let mut prev = 0u64;
        let mut baselined = false;
        let mut total = 0u64;
        let mut max_delta = 0u64;
        for (phase, done, eff) in events {
            let d = fold_download_progress(&mut prev, &mut baselined, *done, phase, *eff);
            total += d;
            max_delta = max_delta.max(d);
        }
        (total, max_delta)
    }

    /// A 4 MiB-granular "downloading" sweep from `start` (exclusive) to `end`.
    fn sweep(start: u64, end: u64) -> Vec<(&'static str, u64, Option<u64>)> {
        let mut v = Vec::new();
        let mut off = start;
        while off < end {
            off = (off + CHUNK).min(end);
            v.push(("downloading", off, None));
        }
        v
    }

    /// A `verifying`-phase sweep: the engine re-reading an already-downloaded
    /// in-`open()` blob (Iroh) to check integrity. It reports its own 0→total
    /// progress at disk speed but moves no *new* bytes.
    fn verify_sweep(total: u64) -> Vec<(&'static str, u64, Option<u64>)> {
        let mut v = Vec::new();
        let mut off = 0u64;
        while off < total {
            off = (off + CHUNK).min(total);
            v.push(("verifying", off, None));
        }
        v
    }

    /// Regression (the phantom second download): an in-`open()` transfer (Iroh)
    /// reports the network download live as "downloading", and the engine then
    /// re-reads the local file as "verifying" to check integrity. That verify
    /// sweep runs at disk speed (~250 MB/s) but must not be counted — it would
    /// otherwise double the session total — and must not spike the graph. Only
    /// the real network download counts, less one baseline chunk.
    #[test]
    fn iroh_verify_sweep_counts_nothing_and_does_not_spike() {
        let total = 512 * MIB;
        let mut events = vec![("connecting", 0, Some(0))]; // before open()
        events.extend(sweep(0, total)); // live network download
        events.extend(verify_sweep(total)); // engine's disk-speed verify re-read
        events.push(("verified", total, None));

        let (counted, max_delta) = replay(&events);
        assert_eq!(
            counted,
            total - CHUNK,
            "the network download counts once, not twice"
        );
        assert!(
            max_delta <= CHUNK,
            "no disk-speed verify spike (max delta was {max_delta})"
        );
    }

    /// Resuming an engine-temp transport (HTTPS): "connecting" carries the true
    /// resume offset, so the tail is counted exactly with no lost first chunk.
    #[test]
    fn resume_http_counts_the_tail_exactly() {
        let resume_from = 300 * MIB;
        let total = 512 * MIB;
        let mut events = vec![("connecting", resume_from, Some(resume_from))];
        events.extend(sweep(resume_from, total));
        events.push(("verified", total, None));

        let (counted, max_delta) = replay(&events);
        assert_eq!(counted, total - resume_from);
        assert!(max_delta <= CHUNK, "no spike (max delta was {max_delta})");
    }

    /// A fresh download counts the whole file apart from the first ~chunk consumed
    /// to set the baseline (immaterial to a session total) — and never spikes.
    #[test]
    fn fresh_download_counts_almost_everything_no_spike() {
        let total = 512 * MIB;
        let mut events = vec![("connecting", 0, Some(0))];
        events.extend(sweep(0, total));
        events.push(("verified", total, None));

        let (counted, max_delta) = replay(&events);
        assert_eq!(counted, total - CHUNK, "lose only the first baseline chunk");
        assert!(max_delta <= CHUNK);
    }

    /// The pre-transfer phases report `done = full size` but move no bytes — they
    /// must not be counted (this was a full-size spike on every download/cache hit).
    #[test]
    fn cache_hit_and_discovery_count_nothing() {
        let total = 512 * MIB;
        let (cache, _) = replay(&[("cache-hit", total, None)]);
        assert_eq!(cache, 0);
        let (discover, _) = replay(&[("discovering peers", total, None)]);
        assert_eq!(discover, 0);
    }

    /// Failover: source A reaches partway, fails, source B resumes from the engine
    /// temp. The two legs sum to the real bytes pulled with no spike at the seam.
    #[test]
    fn failover_resume_is_continuous_no_spike() {
        let mid = 200 * MIB;
        let total = 512 * MIB;
        let mut events = vec![("connecting", 0, Some(0))];
        events.extend(sweep(0, mid)); // source A
        events.push(("source-failed", mid, None));
        events.push(("connecting", mid, Some(mid))); // source B resumes
        events.extend(sweep(mid, total));
        events.push(("verified", total, None));

        let (counted, max_delta) = replay(&events);
        // A loses its first baseline chunk; B resumes losslessly from `mid`.
        assert_eq!(counted, total - CHUNK);
        assert!(max_delta <= CHUNK, "no spike at the failover seam");
    }
}
