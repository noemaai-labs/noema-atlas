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
    /// Master switch for the Iroh transport: off disables both download and seed.
    pub iroh_enabled: bool,
    /// Download sub-switch for Iroh: off disables it as a download route (seeding still allowed).
    pub iroh_download: bool,
    /// Iroh seed sub-switch: share openly-licensed downloads worldwide. On by default.
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
    /// Whether the first-run intro / sharing-consent card has been dismissed.
    pub seen_intro: bool,
    /// Master switch for the BitTorrent transport (download + seed). On by default.
    pub bt_enabled: bool,
    /// Download sub-switch for BitTorrent: off disables it as a download route (seeding continues).
    pub bt_download: bool,
    /// Add public BitTorrent trackers (beyond the DHT) for more peers. PRIVACY: announces your IP + info-hash to third parties; off by default.
    pub bt_use_public_trackers: bool,
    /// Join the mainline DHT to find more peers. Applies on next launch.
    pub bt_dht: bool,
    /// Find LAN peers via Local Peer Discovery (multicast). Applies on next launch.
    pub bt_lsd: bool,
    /// UPnP port forwarding for inbound BitTorrent connectivity. Applies on next launch.
    pub bt_upnp: bool,
    /// Peer connection protocol, as [`noema_core::BtPeerProtocol`]'s stable `u8`
    /// (0 = TCP and µTP, 1 = TCP, 2 = µTP). Applies on next launch.
    pub bt_protocol: u8,
    /// Max connected peers per torrent (`0` = unlimited). Applies on next launch.
    pub bt_max_peers: u32,
    /// Anonymous mode: hide the BT client identity (blank client string + unbranded peer id); IP still visible to the swarm. Applies on next launch.
    pub bt_anonymous: bool,
    /// Seed completed, publicly-redistributable blobs back over BitTorrent.
    pub bt_seed: bool,
    /// Preferred inbound listen port; range `[port, port+10)` is tried (`0` = default range).
    pub bt_port: u16,
    /// Per-direction BitTorrent rate caps in Mbps (`0` = unlimited).
    pub bt_up_cap_mbps: u32,
    pub bt_down_cap_mbps: u32,
    /// Max simultaneous active transfers (download + seed) the UI runs at once.
    pub bt_max_concurrent: u32,
    /// Stop seeding a blob once its upload/size ratio reaches this value (`0` = never). Applies on next launch.
    pub bt_max_ratio: f64,
    /// Download-routing preference as [`noema_core::DownloadPreference`]'s stable `u8`
    /// (0 = Auto, 1 = Prefer P2P, 2 = Prefer BitTorrent, 3 = Save data). Applied live.
    pub download_preference: u8,
    /// Fetch BitTorrent pieces sequentially rather than rarest-first. Applies live.
    #[serde(default)]
    pub bt_sequential: bool,
    /// Bandwidth schedule: alternative caps apply inside the daily window on matching weekdays.
    #[serde(default)]
    pub bt_schedule_enabled: bool,
    /// Window start/end as minutes since local midnight (`0..=1439`).
    #[serde(default)]
    pub bt_schedule_from_min: u16,
    #[serde(default)]
    pub bt_schedule_to_min: u16,
    /// Weekday bitmask: bit 0 = Mon … bit 6 = Sun. `0` = every day.
    #[serde(default)]
    pub bt_schedule_days: u8,
    /// Alternative caps used inside the window (Mbps, `0` = unlimited).
    #[serde(default)]
    pub bt_alt_up_cap_mbps: u32,
    #[serde(default)]
    pub bt_alt_down_cap_mbps: u32,
    #[serde(default)]
    pub alt_download_cap_mbps: u32,
    /// Check for app updates on launch (anonymous). Mirrors the egui app's `auto_update`.
    #[serde(default = "default_true")]
    pub auto_update: bool,
    /// Skip the confirmation popup before share-link / Explore downloads. Mirrors the egui `skip_download_confirm`.
    #[serde(default)]
    pub skip_download_confirm: bool,
}

fn default_true() -> bool {
    true
}

/// `~/Noema Models` (platform home equivalent) — the egui app's default, so files share one folder.
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
            iroh_enabled: true,
            iroh_download: true,
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
            seen_intro: false,
            bt_enabled: true,
            bt_download: true,
            // Off by default (matching Atlas): public trackers expose your IP + info-hash.
            bt_use_public_trackers: false,
            bt_dht: true,
            bt_lsd: true,
            bt_upnp: true,
            bt_protocol: 0,
            bt_max_peers: 0,
            bt_anonymous: false,
            bt_seed: true,
            bt_port: 6881,
            bt_up_cap_mbps: 0,
            bt_down_cap_mbps: 0,
            bt_max_concurrent: 3,
            bt_max_ratio: 0.0,
            download_preference: 0,
            bt_sequential: false,
            bt_schedule_enabled: false,
            bt_schedule_from_min: 0,
            bt_schedule_to_min: 0,
            bt_schedule_days: 0,
            bt_alt_up_cap_mbps: 0,
            bt_alt_down_cap_mbps: 0,
            alt_download_cap_mbps: 0,
            auto_update: true,
            skip_download_confirm: false,
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

    /// BitTorrent upload cap in bytes/sec; `0` = unlimited.
    pub fn bt_up_bps(&self) -> u64 {
        (self.bt_up_cap_mbps as u64) * 125_000
    }

    /// BitTorrent download cap in bytes/sec; `0` = unlimited.
    pub fn bt_down_bps(&self) -> u64 {
        (self.bt_down_cap_mbps as u64) * 125_000
    }

    /// Build the time-of-day bandwidth schedule from the normal (existing) caps and
    /// the alternative-cap fields.
    pub fn bandwidth_schedule(&self) -> noema_core::BandwidthSchedule {
        noema_core::BandwidthSchedule {
            enabled: self.bt_schedule_enabled,
            from_min: self.bt_schedule_from_min,
            to_min: self.bt_schedule_to_min,
            days: self.bt_schedule_days,
            bt_up_bps: self.bt_up_bps(),
            bt_down_bps: self.bt_down_bps(),
            http_down_bps: self.cap_bps(),
            alt_bt_up_bps: (self.bt_alt_up_cap_mbps as u64) * 125_000,
            alt_bt_down_bps: (self.bt_alt_down_cap_mbps as u64) * 125_000,
            alt_http_down_bps: (self.alt_download_cap_mbps as u64) * 125_000,
        }
    }

    /// Inbound listen-port range derived from the preferred port (`[port, port+10)`).
    /// `0` falls back to the engine default range.
    pub fn bt_listen_range(&self) -> Option<std::ops::Range<u16>> {
        if self.bt_port == 0 {
            None
        } else {
            Some(self.bt_port..self.bt_port.saturating_add(10))
        }
    }

    /// The worldwide tracker URL, falling back to the hosted default.
    pub fn tracker(&self) -> String {
        if self.tracker_url.trim().is_empty() {
            noema_core::DEFAULT_TRACKER.to_string()
        } else {
            self.tracker_url.trim().to_string()
        }
    }

    /// The tracker identity (friendly device name) for announces.
    pub fn identity(&self) -> noema_core::tracker::Identity {
        let device = if self.device_name.trim().is_empty() {
            noema_core::identity::default_device_name()
        } else {
            self.device_name.trim().to_string()
        };
        noema_core::tracker::Identity { device }
    }
}
