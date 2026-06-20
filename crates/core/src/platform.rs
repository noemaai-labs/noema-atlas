use crate::manifest::SourceClass;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Desktop,
    Ios,
    Android,
}

impl Platform {
    /// Best-effort detection from the compile target.
    pub fn detect() -> Self {
        #[cfg(target_os = "ios")]
        {
            Platform::Ios
        }
        #[cfg(target_os = "android")]
        {
            Platform::Android
        }
        #[cfg(not(any(target_os = "ios", target_os = "android")))]
        {
            Platform::Desktop
        }
    }
}

/// Best-effort memory budget (bytes) for picking a model quant that will actually
/// run. Prefers **dedicated GPU VRAM** when a discrete accelerator is detected
/// budget for GPU offload); otherwise falls back to **system RAM**. On Apple
/// Silicon GPU memory *is* system memory (unified), so the RAM path is already
/// the right number there.
///
/// Returns ~85% of VRAM (dedicated, low OS overhead) or ~70% of RAM (shared with
/// the OS and other apps). `None` where we can't tell — the caller then falls
/// back to a size heuristic rather than guessing. Detection is best-effort and a
/// guide, not a guarantee (it can't know whether the user offloads to GPU).
pub fn detect_memory_budget_bytes() -> Option<u64> {
    if let Some(vram) = detect_vram_bytes() {
        return Some((vram as f64 * 0.85) as u64);
    }
    total_physical_memory_bytes().map(|t| (t as f64 * 0.70) as u64)
}

/// Total dedicated GPU VRAM in bytes (largest single GPU), or `None`. Covers
/// NVIDIA (via `nvidia-smi`) and AMD on Linux (via sysfs). On macOS this is
/// `None` on purpose: Apple Silicon shares unified memory, handled by the RAM
/// path; legacy Intel-Mac discrete GPUs are rare enough to skip.
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

/// Largest AMD GPU's total VRAM via sysfs (`mem_info_vram_total`, bytes). Present
/// on Linux with the amdgpu driver. Ignores values below 3 GiB: an APU /
/// integrated Radeon reports a small UMA carve-out (often 512 MiB–2 GiB) here,
/// which is NOT the usable budget — treating it as one would recommend a
/// needlessly tiny quant on a machine with plenty of RAM. Below the threshold we
/// return `None` so the caller falls back to system RAM.
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
    /// Whether public peer seeding/advertising is permitted (default: desktop only).
    pub allow_public_seeding: bool,
    /// Whether long-lived background peer transfers are permitted.
    pub background_p2p: bool,
    /// Whether Hugging Face may be used as a byte-download transport.
    ///
    /// Catalog search stays separate; this only gates fetching model bytes from
    /// `SourceClass::Huggingface`.
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
            background_p2p: true,
            huggingface_download: false,
            metered: false,
            battery_saver: false,
        }
    }

    pub fn ios() -> Self {
        PlatformProfile {
            platform: Platform::Ios,
            allow_public_seeding: false,
            background_p2p: false,
            huggingface_download: false,
            metered: false,
            battery_saver: false,
        }
    }

    pub fn android() -> Self {
        PlatformProfile {
            platform: Platform::Android,
            allow_public_seeding: false,
            background_p2p: false,
            huggingface_download: false,
            metered: false,
            battery_saver: false,
        }
    }

    pub fn detect() -> Self {
        match Platform::detect() {
            Platform::Desktop => Self::desktop(),
            Platform::Ios => Self::ios(),
            Platform::Android => Self::android(),
        }
    }

    /// Whether a source class is permitted to *fetch* from on this platform.
    pub fn fetch_enabled(&self, class: SourceClass) -> bool {
        match (self.platform, class) {
            (_, SourceClass::Huggingface) => self.huggingface_download,
            // LAN peering was removed — Atlas is a worldwide service. The variant
            // is retained only so older persisted manifests still deserialize; it
            // is never fetched.
            (_, SourceClass::LanPeer) => false,
            // Local + HTTP-family work everywhere.
            (_, SourceClass::LocalFile)
            | (_, SourceClass::HttpsMirror)
            | (_, SourceClass::Ipfs) => true,
            // Iroh: desktop always; mobile only in foreground (modeled as enabled
            // but lower priority — the wrapper decides foreground gating).
            (_, SourceClass::Iroh) => true,
            // BitTorrent has been retired; the variant is retained only so older
            // persisted manifests still deserialize. Never fetched.
            (_, SourceClass::BittorrentV2) => false,
        }
    }

    /// Base priority for a source class (higher = preferred), reflecting the
    /// per-platform recommended transport order. Local cache hits are handled
    /// before the planner runs, so `LocalFile` import sources rank highest here.
    pub fn class_priority(&self, class: SourceClass) -> f64 {
        match self.platform {
            Platform::Desktop => match class {
                SourceClass::LocalFile => 120.0,
                SourceClass::LanPeer => 100.0,
                SourceClass::Iroh => 90.0,
                SourceClass::BittorrentV2 => 80.0,
                SourceClass::Ipfs => 70.0,
                SourceClass::HttpsMirror => 55.0,
                SourceClass::Huggingface => 45.0,
            },
            Platform::Ios => match class {
                SourceClass::LocalFile => 120.0,
                SourceClass::HttpsMirror => 95.0,
                SourceClass::Ipfs => 70.0,
                SourceClass::LanPeer => 60.0,
                SourceClass::Iroh => 50.0,
                SourceClass::Huggingface => 40.0,
                SourceClass::BittorrentV2 => 0.0,
            },
            Platform::Android => match class {
                SourceClass::LocalFile => 120.0,
                SourceClass::HttpsMirror => 95.0,
                SourceClass::LanPeer => 80.0,
                SourceClass::Iroh => 70.0,
                SourceClass::Ipfs => 65.0,
                SourceClass::Huggingface => 40.0,
                SourceClass::BittorrentV2 => 30.0,
            },
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
            profile.class_priority(SourceClass::Ipfs)
                > profile.class_priority(SourceClass::Huggingface)
        );
        assert!(
            profile.class_priority(SourceClass::HttpsMirror)
                > profile.class_priority(SourceClass::Huggingface)
        );
    }
}
