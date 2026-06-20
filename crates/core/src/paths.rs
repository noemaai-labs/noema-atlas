use std::path::PathBuf;

/// The default store root, honoring `NOEMA_HOME`, then the OS conventional
/// per-user data directory, then a `.noema-atlas` fallback in `$HOME`.
pub fn default_root() -> PathBuf {
    if let Ok(home) = std::env::var("NOEMA_HOME") {
        if !home.is_empty() {
            return PathBuf::from(home);
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            if !local.is_empty() {
                return PathBuf::from(local).join("NoemaAtlas");
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            if !home.is_empty() {
                return PathBuf::from(home)
                    .join("Library")
                    .join("Application Support")
                    .join("NoemaAtlas");
            }
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            if !xdg.is_empty() {
                return PathBuf::from(xdg).join("noema-atlas");
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            if !home.is_empty() {
                return PathBuf::from(home)
                    .join(".local")
                    .join("share")
                    .join("noema-atlas");
            }
        }
    }

    PathBuf::from(".noema-atlas")
}
