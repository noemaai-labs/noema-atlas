use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioSettings {
    /// Default install directory (where `materialize_install` lands files).
    /// Matches the desktop app's `~/Noema Models` so installs are unified.
    pub models_dir: String,
    /// Download speed cap in Mbps. `0` = unlimited.
    pub download_cap_mbps: u32,
    /// Max parallel connections for a single large HTTP(S) download.
    pub download_connections: u32,
    /// Worldwide P2P tracker base URL.
    pub tracker_url: String,
    /// Auto-share openly-licensed downloads to peers (ON by default, like
    /// desktop — starts the worldwide seeder at launch).
    pub share_worldwide: bool,
    /// Also auto-share gated/licensed public models (off by default).
    pub share_gated: bool,
    /// Allow Hugging Face as a byte-download fallback source.
    pub allow_hf_download: bool,
    /// Route Hub traffic through a mirror instead of the real Hub.
    pub hf_mirror_enabled: bool,
    pub hf_mirror_url: String,
    /// Route internet traffic through a proxy.
    pub proxy_enabled: bool,
    pub proxy_url: String,
    /// `"system" | "light" | "dark"`.
    pub theme: String,
    /// Stable per-install identifier (auto-generated on first launch).
    pub device_id: String,
    /// Friendly device name shown to peers (auto-filled on first launch).
    pub device_name: String,
    /// Shared "My Devices" pairing code; the raw code never leaves the device
    pub group_code: String,
    /// Skip the download-confirmation modal.
    pub skip_download_confirm: bool,
    /// Whether the first-run intro / sharing-consent card has been dismissed.
    pub seen_intro: bool,
}

/// `~/Noema Models` (or the platform home equivalent) — identical to the egui
/// desktop app's default, so installed files land in one shared folder.
pub fn default_models_dir() -> String {
    let home = std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok().filter(|h| !h.is_empty()));
    match home {
        Some(h) => Path::new(&h)
            .join("Noema Models")
            .to_string_lossy()
            .into_owned(),
        None => "Noema Models".to_string(),
    }
}

impl Default for StudioSettings {
    fn default() -> Self {
        Self {
            models_dir: default_models_dir(),
            download_cap_mbps: 0,
            download_connections: 4,
            tracker_url: noema_core::DEFAULT_TRACKER.to_string(),
            share_worldwide: true,
            share_gated: false,
            allow_hf_download: true,
            hf_mirror_enabled: false,
            hf_mirror_url: "https://hf-mirror.com".to_string(),
            proxy_enabled: false,
            proxy_url: String::new(),
            theme: "system".to_string(),
            device_id: String::new(),
            device_name: String::new(),
            group_code: String::new(),
            skip_download_confirm: false,
            seen_intro: false,
        }
    }
}

impl StudioSettings {
    pub fn path(root: &Path) -> PathBuf {
        root.join("studio-settings.json")
    }

    /// Load from disk, falling back to defaults on any error.
    pub fn load(root: &Path) -> Self {
        std::fs::read(Self::path(root))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, root: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(root).ok();
        std::fs::write(Self::path(root), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    /// Speed cap in bytes/sec (1 Mbps = 125,000 B/s); `0` = unlimited.
    pub fn cap_bps(&self) -> u64 {
        (self.download_cap_mbps as u64) * 125_000
    }

    /// The worldwide tracker URL, falling back to the hosted default.
    pub fn tracker(&self) -> String {
        if self.tracker_url.trim().is_empty() {
            noema_core::DEFAULT_TRACKER.to_string()
        } else {
            self.tracker_url.trim().to_string()
        }
    }

    /// The tracker identity (friendly name + derived group id) for announces.
    pub fn identity(&self) -> noema_core::tracker::Identity {
        let device = if self.device_name.trim().is_empty() {
            noema_core::identity::default_device_name()
        } else {
            self.device_name.trim().to_string()
        };
        noema_core::tracker::Identity {
            device,
            group: noema_core::identity::group_id(&self.group_code),
        }
    }
}
