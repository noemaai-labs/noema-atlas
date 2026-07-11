use crate::manifest::SourceClass;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Desktop,
}

impl Platform {
    /// Best-effort detection from the compile target. Atlas is desktop-only.
    pub fn detect() -> Self {
        Platform::Desktop
    }
}

/// Best-effort memory budget (bytes) for picking a runnable quant: ~85% of GPU
/// VRAM if a discrete accelerator is detected, else ~70% of system RAM, or `None`.
pub fn detect_memory_budget_bytes() -> Option<u64> {
    if let Some(vram) = detect_vram_bytes() {
        return Some((vram as f64 * 0.85) as u64);
    }
    total_physical_memory_bytes().map(|t| (t as f64 * 0.70) as u64)
}

/// Total dedicated GPU VRAM in bytes (largest single GPU), or `None`. `None` on
/// macOS by design: unified memory is covered by the RAM path.
fn detect_vram_bytes() -> Option<u64> {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if let Some(v) = nvidia_vram_bytes() {
        return Some(v);
    }
    #[cfg(target_os = "linux")]
    if let Some(v) = amd_vram_bytes_sysfs() {
        return Some(v);
    }
    None
}

/// Largest NVIDIA GPU's total VRAM (a model loads on one device). `nvidia-smi`
/// reports per-GPU `memory.total` in MiB; absent driver/CLI ⇒ `None`.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn nvidia_vram_bytes() -> Option<u64> {
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .max()
        .map(|mib| mib * 1024 * 1024)
}

/// Largest AMD GPU's total VRAM via sysfs (`mem_info_vram_total`, bytes), or
/// `None`. Ignores values below 3 GiB: an APU reports a small UMA carve-out
/// here, not the usable budget.
#[cfg(target_os = "linux")]
fn amd_vram_bytes_sysfs() -> Option<u64> {
    const MIN_DISCRETE_VRAM: u64 = 3 * 1024 * 1024 * 1024;
    let mut best: Option<u64> = None;
    for entry in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let path = entry.path().join("device/mem_info_vram_total");
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(v) = s.trim().parse::<u64>() {
                best = Some(best.map_or(v, |b| b.max(v)));
            }
        }
    }
    best.filter(|&v| v >= MIN_DISCRETE_VRAM)
}

#[cfg(target_os = "macos")]
fn total_physical_memory_bytes() -> Option<u64> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}

#[cfg(target_os = "linux")]
fn total_physical_memory_bytes() -> Option<u64> {
    let txt = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in txt.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn total_physical_memory_bytes() -> Option<u64> {
    None
}

/// Runtime knobs influencing source selection.
#[derive(Debug, Clone)]
pub struct PlatformProfile {
    pub platform: Platform,
    /// Whether public peer seeding/advertising is permitted.
    pub allow_public_seeding: bool,
    /// Whether Hugging Face may be used as a byte-download transport (catalog
    /// search stays separate; this only gates fetching `SourceClass::Huggingface`).
    pub huggingface_download: bool,
    /// Network is metered (avoid large opportunistic transfers).
    pub metered: bool,
    /// Conserve battery (deprioritize CPU/radio-heavy transports).
    pub battery_saver: bool,
}

impl PlatformProfile {
    pub fn desktop() -> Self {
        PlatformProfile {
            platform: Platform::Desktop,
            allow_public_seeding: true,
            huggingface_download: false,
            metered: false,
            battery_saver: false,
        }
    }

    pub fn detect() -> Self {
        match Platform::detect() {
            Platform::Desktop => Self::desktop(),
        }
    }

    /// Whether a source class is permitted to *fetch* from.
    pub fn fetch_enabled(&self, class: SourceClass) -> bool {
        match class {
            SourceClass::Huggingface => self.huggingface_download,
            // LAN peering removed; variant kept only so old manifests deserialize.
            SourceClass::LanPeer => false,
            // Local + HTTP-family + Iroh peers work everywhere.
            SourceClass::LocalFile | SourceClass::HttpsMirror | SourceClass::Iroh => true,
            // BitTorrent fetches from swarms when the adapter is compiled in;
            // otherwise the variant is kept only for back-compat deser.
            #[cfg(feature = "bittorrent")]
            SourceClass::BittorrentV2 => true,
            #[cfg(not(feature = "bittorrent"))]
            SourceClass::BittorrentV2 => false,
        }
    }

    /// Base priority for a source class (higher = preferred). Local cache hits are
    /// handled before the planner runs, so `LocalFile` sources rank highest here.
    pub fn class_priority(&self, class: SourceClass) -> f64 {
        match class {
            SourceClass::LocalFile => 120.0,
            SourceClass::LanPeer => 100.0,
            SourceClass::Iroh => 90.0,
            SourceClass::BittorrentV2 => 80.0,
            SourceClass::HttpsMirror => 55.0,
            SourceClass::Huggingface => 45.0,
        }
    }
}

impl Default for PlatformProfile {
    fn default() -> Self {
        Self::detect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hugging_face_downloads_are_opt_in() {
        let mut profile = PlatformProfile::desktop();
        assert!(!profile.fetch_enabled(SourceClass::Huggingface));

        profile.huggingface_download = true;
        assert!(profile.fetch_enabled(SourceClass::Huggingface));
    }

    #[test]
    fn hugging_face_is_last_resort_when_enabled() {
        let mut profile = PlatformProfile::desktop();
        profile.huggingface_download = true;

        assert!(
            profile.class_priority(SourceClass::LanPeer)
                > profile.class_priority(SourceClass::Huggingface)
        );
        assert!(
            profile.class_priority(SourceClass::Iroh)
                > profile.class_priority(SourceClass::Huggingface)
        );
        assert!(
            profile.class_priority(SourceClass::HttpsMirror)
                > profile.class_priority(SourceClass::Huggingface)
        );
    }
}
