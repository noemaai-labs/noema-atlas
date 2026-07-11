//! In-app auto-update for Atlas: verify a signed release manifest against the baked-in
//! release key, then download + SHA-256-re-verify the asset before applying in place.
//! The VPS only points; executed bytes must match a hash the offline release key signed.
//! `UPDATE_RELEASE_PUBKEYS` is empty until shipped, so [`check`] returns `Ok(None)` — fail-closed.

use noema_core::update::{PlatformAsset, ReleaseManifest, UPDATE_RELEASE_PUBKEYS};
use std::path::{Path, PathBuf};

/// How the running copy was installed — decides the apply mechanism (per-platform; see [`detect_flavor`]).
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    /// macOS `.app` bundle (the asset is a universal `.zip`).
    MacApp,
    /// Windows NSIS-installed build under Program Files (re-run `Setup.exe`).
    WinInstaller,
    /// Windows portable `.exe` (swap the binary in place).
    WinPortable,
    /// Linux AppImage (replace the single `$APPIMAGE` file).
    LinuxAppImage,
    /// Linux raw binary from the tarball (swap the binary in place).
    LinuxPortable,
    /// Managed by the OS package manager (deb/rpm) or an unknown layout — we must
    /// not self-update; point the user at the release page instead.
    Managed,
}

impl Flavor {
    /// The manifest `flavor` token to match an asset against (empty = "the default
    /// asset for this os/arch", which is how Atlas labels its mac/linux artifacts).
    fn asset_flavor(self) -> Option<&'static str> {
        match self {
            Flavor::MacApp => None,
            Flavor::WinInstaller => Some("installer"),
            Flavor::WinPortable => Some("portable"),
            Flavor::LinuxAppImage => Some("appimage"),
            Flavor::LinuxPortable => Some("tarball"),
            Flavor::Managed => None,
        }
    }
}

/// A confirmed-available update for this platform.
#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub version: String,
    pub notes_url: String,
    pub asset: PlatformAsset,
    pub forced: bool,
    pub flavor: Flavor,
}

/// UI-side state for the updater, held on the `App`.
#[derive(Default)]
pub struct UpdateUi {
    /// Set when a newer version is available — drives the banner.
    pub available: Option<UpdateInfo>,
    /// A check is in flight.
    pub checking: bool,
    /// The in-flight check is the silent startup ping (vs a user-clicked "Check
    /// now"), so its "up to date"/error result isn't surfaced in the status line.
    pub silent: bool,
    /// A download/apply is in flight.
    pub applying: bool,
    /// Download progress (0.0..=1.0) while applying.
    pub progress: Option<f32>,
    /// A version the user dismissed — hide its banner until something newer shows.
    pub dismissed: Option<String>,
    /// Outcome of the most recent finished check: `None` = never checked this
    /// session, `Some(Ok(()))` = confirmed current, `Some(Err(_))` = check
    /// failed. The Settings row must not claim "up to date" without evidence.
    pub last_check: Option<Result<(), String>>,
}

impl UpdateUi {
    /// Should the banner show? When an update is available and either it's forced
    /// (a security floor — can't be dismissed) or the user hasn't dismissed it.
    pub fn banner(&self) -> Option<&UpdateInfo> {
        let info = self.available.as_ref()?;
        if info.forced {
            return Some(info);
        }
        match &self.dismissed {
            Some(v) if v == &info.version => None,
            _ => Some(info),
        }
    }
}

/// What [`apply`] did, so the caller knows whether to relaunch/exit. Variants are
/// platform-specific (`LaunchedInstaller` is Windows-only).
#[allow(dead_code)]
pub enum ApplyOutcome {
    /// Binary/bundle swapped AND the new copy spawned — the caller should exit now.
    Relaunching,
    /// Handed the verified installer to the OS (Windows `Setup.exe`) — the caller
    /// should exit so the running exe unlocks and the installer can proceed.
    LaunchedInstaller,
    /// The swap succeeded but spawning the new copy failed — the caller must NOT
    /// exit (that would leave nothing running); tell the user to reopen the app.
    SwappedButRelaunchFailed { reason: String },
    /// Couldn't self-apply (managed install, unwritable target). The verified file
    /// is at `path`; tell the user how to finish.
    Manual { path: PathBuf, reason: String },
}

/// Build a proxy-aware rustls HTTP client, mirroring how the engine routes traffic.
/// `read_timeout` bounds the inter-byte gap so a stalled stream errors out. Not
/// `https_only`: a self-hosted registry may be plain http, and the signature + SHA-256
/// gate already stops a downgraded transport delivering attacker-chosen bytes.
fn http_client(proxy: Option<&str>) -> Result<reqwest::Client, String> {
    let mut b = reqwest::Client::builder()
        .user_agent(concat!("noema-atlas/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(60));
    if let Some(p) = proxy.map(str::trim).filter(|p| !p.is_empty()) {
        let proxy = reqwest::Proxy::all(p).map_err(|e| format!("bad proxy: {e}"))?;
        b = b.proxy(proxy);
    }
    b.build().map_err(|e| e.to_string())
}

/// Ping the VPS for the signed manifest and decide whether an update applies.
/// `Ok(None)` = nothing to do (untrusted/expired/not-newer/no asset); `Err` = network/parse
/// failure (surfaced only on a manual check, swallowed on the silent startup ping).
pub async fn check(
    tracker_url: &str,
    current: &str,
    proxy: Option<&str>,
) -> Result<Option<UpdateInfo>, String> {
    check_with_trust(tracker_url, current, proxy, UPDATE_RELEASE_PUBKEYS).await
}

/// [`check`] with an explicit trust set, so tests can inject a key instead of the
/// (empty until shipped) baked-in [`UPDATE_RELEASE_PUBKEYS`].
async fn check_with_trust(
    tracker_url: &str,
    current: &str,
    proxy: Option<&str>,
    trusted: &[&str],
) -> Result<Option<UpdateInfo>, String> {
    let url = format!("{}/update/latest", tracker_url.trim_end_matches('/'));
    let client = http_client(proxy)?;
    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("update check: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("update check: HTTP {}", resp.status()));
    }
    // Bound the manifest body: the endpoint is untrusted (and user-overridable),
    // so a hostile/compromised server must not be able to stream an unbounded body.
    // The real manifest is a few KB; 4 MiB is ample headroom.
    let bytes = {
        let mut resp = resp;
        let mut buf = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            if buf.len() + chunk.len() > 4 * 1024 * 1024 {
                return Err("update manifest exceeds maximum size".to_string());
            }
            buf.extend_from_slice(&chunk);
        }
        buf
    };
    let manifest = ReleaseManifest::from_json(&bytes).map_err(|e| format!("bad manifest: {e}"))?;

    // Trust gate: the signature must be from a key compiled into THIS binary.
    if !manifest.is_signed_by_trusted(trusted) {
        tracing::debug!("update manifest not signed by a trusted release key — ignoring");
        return Ok(None);
    }
    let now = noema_core::util::now_unix_millis();
    if manifest.is_expired(now) {
        tracing::warn!("update manifest is expired — ignoring (will retry later)");
        return Ok(None);
    }
    let Some(app) = manifest.app("atlas") else {
        return Ok(None);
    };
    if !app.is_newer_than(current) {
        return Ok(None);
    }
    let flavor = detect_flavor();
    let (os, arch) = noema_core::update::host_os_arch();
    let Some(asset) = app.select_asset(&os, &arch, flavor.asset_flavor()) else {
        tracing::info!(
            "update {} available but no asset for {os}/{arch} — skipping",
            app.version
        );
        return Ok(None);
    };
    Ok(Some(UpdateInfo {
        version: app.version.clone(),
        notes_url: app.notes_url.clone(),
        asset: asset.clone(),
        forced: app.is_forced_for(current),
        flavor,
    }))
}

/// Download `asset` to `dest_dir`, streaming + hashing as we go, and reject it unless
/// the SHA-256 matches the signed manifest. `progress(done, total)` is called as bytes
/// arrive. The returned path is a verified file safe to apply.
pub async fn download_verified(
    asset: &PlatformAsset,
    dest_dir: &Path,
    proxy: Option<&str>,
    progress: impl Fn(u64, u64),
) -> Result<PathBuf, String> {
    use futures_util::StreamExt;
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncWriteExt;

    let client = http_client(proxy)?;
    let resp = client
        .get(&asset.url)
        .send()
        .await
        .map_err(|e| format!("download: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download: HTTP {}", resp.status()));
    }
    let total = resp.content_length().unwrap_or(asset.size);

    // Stage under a sanitized BARE file name — `asset.name` rides inside the signed
    // manifest, but never let it carry a path component (`..`, absolute, separators).
    let safe_name = Path::new(&asset.name)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| format!("unsafe asset name: {}", asset.name))?;

    tokio::fs::create_dir_all(dest_dir)
        .await
        .map_err(|e| e.to_string())?;
    let out_path = dest_dir.join(safe_name);
    let tmp_path = dest_dir.join(format!("{safe_name}.part"));

    // Cap the write so a lying Content-Length / endless stream can't fill the disk.
    // The manifest pins the exact size; allow a little slack, with a hard ceiling
    // when the size is unknown.
    let max_bytes: u64 = if asset.size > 0 {
        asset.size + 1024
    } else {
        8 * 1024 * 1024 * 1024
    };

    let result = async {
        let mut file = tokio::fs::File::create(&tmp_path)
            .await
            .map_err(|e| e.to_string())?;
        let mut hasher = Sha256::new();
        let mut done: u64 = 0;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("download: {e}"))?;
            done += chunk.len() as u64;
            if done > max_bytes {
                return Err("download exceeded the expected size — aborting".to_string());
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            progress(done, total.max(done));
        }
        file.flush().await.map_err(|e| e.to_string())?;
        let got = hex::encode(hasher.finalize());
        if !got.eq_ignore_ascii_case(asset.sha256.trim()) {
            return Err(format!(
                "integrity check failed: expected {}, got {got} — refusing to apply",
                asset.sha256
            ));
        }
        Ok(())
    }
    .await;

    match result {
        Ok(()) => {
            tokio::fs::rename(&tmp_path, &out_path)
                .await
                .map_err(|e| e.to_string())?;
            Ok(out_path)
        }
        // Never leave a half-written/failed partial behind, whatever went wrong.
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            Err(e)
        }
    }
}

/// Where to stage downloads (a per-user temp dir under the app's data root).
pub fn stage_dir() -> PathBuf {
    noema_core::paths::default_root().join("updates")
}

/// Detect how this copy was installed. Conservative: anything ambiguous falls back to
/// [`Flavor::Managed`] so we never swap a binary we don't own.
pub fn detect_flavor() -> Flavor {
    let exe = std::env::current_exe().unwrap_or_default();
    let _ = &exe; // used on every supported target; silences the fallback arm

    #[cfg(target_os = "macos")]
    let flavor = if exe
        .components()
        .any(|c| c.as_os_str().to_string_lossy().ends_with(".app"))
    {
        Flavor::MacApp
    } else {
        Flavor::Managed
    };

    #[cfg(target_os = "windows")]
    let flavor = {
        let lossy = exe.to_string_lossy().to_lowercase();
        let in_program_files = ["programfiles", "programfiles(x86)", "programw6432"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .any(|d| lossy.starts_with(&d.to_lowercase()));
        if in_program_files {
            Flavor::WinInstaller
        } else {
            Flavor::WinPortable
        }
    };

    #[cfg(target_os = "linux")]
    let flavor = {
        if std::env::var_os("APPIMAGE").is_some() {
            Flavor::LinuxAppImage
        } else {
            let lossy = exe.to_string_lossy();
            // Owned by dpkg/rpm — never self-update a system-managed install.
            if lossy.starts_with("/usr/")
                || lossy.starts_with("/opt/")
                || lossy.starts_with("/bin/")
            {
                Flavor::Managed
            } else {
                Flavor::LinuxPortable
            }
        }
    };

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let flavor = Flavor::Managed;

    flavor
}

/// Apply a verified update. Blocking (filesystem + process work); run on a blocking
/// thread. On success for the swap/installer cases the new copy has already been
/// spawned, so the caller must exit the process.
pub fn apply(info: &UpdateInfo, staged: &Path) -> Result<ApplyOutcome, String> {
    // Re-hash the staged file immediately before acting on it. It was verified at
    // download time, but this closes the verify→apply window (a same-user race on the
    // staging dir) and guarantees we never hand unverified bytes to a swap/installer.
    verify_file_sha256(staged, info.asset.sha256.trim())?;
    match info.flavor {
        Flavor::MacApp => apply_macos(staged),
        Flavor::WinInstaller => apply_windows_installer(staged),
        Flavor::WinPortable | Flavor::LinuxPortable => apply_self_replace(staged),
        Flavor::LinuxAppImage => apply_appimage(staged),
        Flavor::Managed => Ok(ApplyOutcome::Manual {
            path: staged.to_path_buf(),
            reason: "This build is managed by your system package manager.".to_string(),
        }),
    }
}

/// Re-read `path` and confirm its SHA-256 equals `expected` (lowercase hex).
fn verify_file_sha256(path: &Path, expected: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| format!("reopen staged: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = hex::encode(hasher.finalize());
    if got.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err("staged update no longer matches its signed hash — aborting".to_string())
    }
}

/// Outcome after a successful in-place swap: relaunch the new copy, or report that
/// the swap landed but spawning the successor failed (so the caller won't exit into
/// nothing). `path` is what to launch.
fn relaunched_or_failed(path: &Path) -> ApplyOutcome {
    match relaunch(path) {
        Ok(()) => ApplyOutcome::Relaunching,
        Err(e) => ApplyOutcome::SwappedButRelaunchFailed {
            reason: format!("couldn't start the updated app ({e})"),
        },
    }
}

/// Swap the running binary in place (portable builds) and relaunch.
///
/// The portable/tarball assets are archives, so the binary matching this executable is
/// extracted first — swapping raw archive bytes over the running exe would destroy it.
fn apply_self_replace(staged: &Path) -> Result<ApplyOutcome, String> {
    // Resolve our own path BEFORE the swap: on Linux `/proc/self/exe` reads
    // "<path> (deleted)" once the old inode is unlinked, which can never be spawned.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;

    let name = staged
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let work = staged
        .parent()
        .unwrap_or(Path::new("."))
        .join("portable-extract");
    let binary = if name.ends_with(".zip") || name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        extract_archive(staged, &work)?;
        let wanted = exe.file_name().ok_or("bad executable path")?;
        let found = find_file_named(&work, wanted).ok_or_else(|| {
            format!(
                "the downloaded archive has no `{}` inside",
                wanted.to_string_lossy()
            )
        })?;
        set_executable(&found)?;
        found
    } else {
        staged.to_path_buf()
    };

    let swapped = self_replace::self_replace(&binary).map_err(|e| format!("replace: {e}"));
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(staged);
    swapped?;
    Ok(relaunched_or_failed(&exe))
}

/// Unpack a verified `.zip` / `.tar.gz` into `dest` with the platform's own tools.
/// Windows' `tar.exe` reads zips too; `Expand-Archive` is the fallback.
fn extract_archive(archive: &Path, dest: &Path) -> Result<(), String> {
    use std::process::Command;
    let name = archive
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let tar_gz = name.ends_with(".tar.gz") || name.ends_with(".tgz");
    let ran = if tar_gz {
        Command::new("tar")
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(dest)
            .status()
            .map(|s| s.success())
    } else if cfg!(windows) {
        Command::new("tar")
            .arg("-xf")
            .arg(archive)
            .arg("-C")
            .arg(dest)
            .status()
            .map(|s| s.success())
            .or_else(|_| {
                Command::new("powershell")
                    .args(["-NoProfile", "-NonInteractive", "-Command"])
                    .arg(format!(
                        "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
                        archive.display(),
                        dest.display()
                    ))
                    .status()
                    .map(|s| s.success())
            })
    } else {
        Command::new("unzip")
            .args(["-q", "-o"])
            .arg(archive)
            .arg("-d")
            .arg(dest)
            .status()
            .map(|s| s.success())
    };
    match ran {
        Ok(true) => Ok(()),
        Ok(false) => Err("couldn't unpack the downloaded archive".to_string()),
        Err(e) => Err(format!("couldn't unpack the downloaded archive: {e}")),
    }
}

/// Depth-first search for a file with exactly `name` under `dir`.
fn find_file_named(dir: &Path, name: &std::ffi::OsStr) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Some(found) = find_file_named(&p, name) {
                return Some(found);
            }
        } else if p.file_name() == Some(name) {
            return Some(p);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn apply_appimage(staged: &Path) -> Result<ApplyOutcome, String> {
    let appimage = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .ok_or("APPIMAGE not set")?;
    // Replace atomically on the same filesystem, then make it executable.
    let parent = appimage.parent().ok_or("bad APPIMAGE path")?;
    if std::fs::metadata(parent)
        .map(|m| m.permissions().readonly())
        .unwrap_or(true)
    {
        return Ok(ApplyOutcome::Manual {
            path: staged.to_path_buf(),
            reason: "The AppImage lives in a read-only location.".to_string(),
        });
    }
    let tmp = parent.join(".noema-atlas-update.tmp");
    std::fs::copy(staged, &tmp).map_err(|e| e.to_string())?;
    set_executable(&tmp)?;
    std::fs::rename(&tmp, &appimage).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(staged);
    Ok(relaunched_or_failed(&appimage))
}

#[cfg(not(target_os = "linux"))]
fn apply_appimage(_staged: &Path) -> Result<ApplyOutcome, String> {
    Err("AppImage apply is Linux-only".to_string())
}

#[cfg(target_os = "macos")]
fn apply_macos(staged: &Path) -> Result<ApplyOutcome, String> {
    use std::process::Command;
    // Find the installed bundle root: <root>.app/Contents/MacOS/<bin>.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let bundle = exe
        .ancestors()
        .find(|p| p.extension().map(|e| e == "app").unwrap_or(false))
        .ok_or("not running from a .app bundle")?
        .to_path_buf();
    let bundle_parent = bundle.parent().ok_or("bad bundle path")?;
    if std::fs::metadata(bundle_parent)
        .map(|m| m.permissions().readonly())
        .unwrap_or(true)
    {
        return Ok(ApplyOutcome::Manual {
            path: staged.to_path_buf(),
            reason:
                "Atlas is installed somewhere it can't update itself (try dragging the new app in)."
                    .to_string(),
        });
    }
    // Unzip the verified archive into a temp dir using the system unzip.
    let work = staged
        .parent()
        .unwrap_or(Path::new("."))
        .join("mac-extract");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let status = Command::new("/usr/bin/unzip")
        .args(["-q", "-o"])
        .arg(staged)
        .arg("-d")
        .arg(&work)
        .status()
        .map_err(|e| format!("unzip: {e}"))?;
    if !status.success() {
        return Err("unzip failed".to_string());
    }
    let new_app = find_dot_app(&work).ok_or("no .app inside the downloaded archive")?;

    // Swap: move the old bundle aside, move the new one in, then drop the backup.
    let backup = bundle.with_extension("app.old");
    let _ = std::fs::remove_dir_all(&backup);
    std::fs::rename(&bundle, &backup).map_err(|e| format!("move old bundle: {e}"))?;
    if let Err(e) = std::fs::rename(&new_app, &bundle) {
        // A cross-volume rename fails with EXDEV — fall back to a recursive copy
        // (ditto preserves the bundle's symlinks + xattrs).
        let copied = Command::new("/usr/bin/ditto")
            .arg(&new_app)
            .arg(&bundle)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !copied {
            // Roll back so the user isn't left without an app.
            let _ = std::fs::rename(&backup, &bundle);
            return Err(format!("install new bundle: {e}"));
        }
    }
    let _ = std::fs::remove_dir_all(&backup);
    // Clear the quarantine flag so Gatekeeper doesn't re-prompt on relaunch.
    let _ = Command::new("/usr/bin/xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(&bundle)
        .status();
    let _ = std::fs::remove_file(staged);
    let _ = std::fs::remove_dir_all(&work);
    // Relaunch the bundle via LaunchServices; if that fails, don't let the caller
    // exit into nothing. `-n` forces a NEW instance — without it, `open` merely
    // activates this still-running (about to exit) process and nothing restarts.
    let relaunched = Command::new("/usr/bin/open")
        .arg("-n")
        .arg(&bundle)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if relaunched {
        Ok(ApplyOutcome::Relaunching)
    } else {
        Ok(ApplyOutcome::SwappedButRelaunchFailed {
            reason: "the update is installed but Atlas couldn't reopen itself".to_string(),
        })
    }
}

#[cfg(not(target_os = "macos"))]
fn apply_macos(_staged: &Path) -> Result<ApplyOutcome, String> {
    Err("macOS apply is macOS-only".to_string())
}

#[cfg(target_os = "windows")]
fn apply_windows_installer(staged: &Path) -> Result<ApplyOutcome, String> {
    use std::process::Command;
    // The NSIS Setup.exe carries its own elevation manifest, so launching it triggers
    // the UAC prompt itself. Spawn it detached and let the caller exit so the running
    // exe unlocks and the installer can overwrite it.
    Command::new(staged)
        .spawn()
        .map_err(|e| format!("launch installer: {e}"))?;
    Ok(ApplyOutcome::LaunchedInstaller)
}

#[cfg(not(target_os = "windows"))]
fn apply_windows_installer(_staged: &Path) -> Result<ApplyOutcome, String> {
    Err("installer apply is Windows-only".to_string())
}

/// Spawn the given executable so the new version comes up as we exit. Returns the
/// spawn result so the caller can avoid exiting into nothing on failure.
fn relaunch(path: &Path) -> std::io::Result<()> {
    std::process::Command::new(path).spawn().map(|_| ())
}

#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn find_dot_app(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let p = e.path();
        if p.extension().map(|x| x == "app").unwrap_or(false) {
            Some(p)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use noema_core::sign::KeyPair;
    use noema_core::update::{AppRelease, PlatformAsset, ReleaseManifest, RELEASE_MANIFEST_SCHEMA};
    use std::io::{Read, Write};

    /// Serve a single canned `200 OK` JSON body on a throwaway loopback port and return
    /// its `http://127.0.0.1:PORT` base, so the real HTTP + parse + decision path runs.
    fn serve_once(body: String) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf); // drain the request line/headers
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}")
    }

    /// A manifest whose only asset matches THIS host exactly, so the test is
    /// deterministic on macOS/Linux/Windows and on x86_64/arm64 runners.
    fn manifest_for_host(version: &str, signer: &KeyPair) -> String {
        let (os, arch) = noema_core::update::host_os_arch();
        let flavor = detect_flavor().asset_flavor().unwrap_or("").to_string();
        let mut m = ReleaseManifest {
            schema: RELEASE_MANIFEST_SCHEMA,
            channel: "stable".into(),
            generated_at: 0,
            expires_at: i64::MAX, // never expired for the test
            apps: vec![AppRelease {
                app: "atlas".into(),
                version: version.into(),
                min_supported: String::new(),
                notes_url: "https://example.com/notes".into(),
                assets: vec![PlatformAsset {
                    os,
                    arch,
                    flavor,
                    name: "atlas-update.bin".into(),
                    url: "https://example.com/atlas-update.bin".into(),
                    sha256: "ab".repeat(32),
                    size: 1,
                    signature: String::new(),
                }],
            }],
            signatures: vec![],
        };
        m.sign(&signer.secret_hex()).unwrap();
        m.to_json_pretty().unwrap()
    }

    #[tokio::test]
    async fn check_offers_newer_version_signed_by_trusted_key() {
        let signer = KeyPair::generate();
        let base = serve_once(manifest_for_host("0.2.0", &signer));
        let got = check_with_trust(&base, "0.1.0", None, &[signer.public_hex().as_str()])
            .await
            .unwrap();
        let info = got.expect("an update should be offered for this host");
        assert_eq!(info.version, "0.2.0");
        assert_eq!(info.notes_url, "https://example.com/notes");
    }

    #[tokio::test]
    async fn check_rejects_untrusted_signer() {
        let signer = KeyPair::generate();
        let stranger = KeyPair::generate();
        let base = serve_once(manifest_for_host("0.2.0", &signer));
        // Signed, newer, right asset — but the signer isn't in our trust set.
        let got = check_with_trust(&base, "0.1.0", None, &[stranger.public_hex().as_str()])
            .await
            .unwrap();
        assert!(
            got.is_none(),
            "an untrusted signature must never offer an update"
        );
    }

    #[tokio::test]
    async fn check_ignores_same_or_older_version() {
        let signer = KeyPair::generate();
        let base = serve_once(manifest_for_host("0.2.0", &signer));
        let got = check_with_trust(&base, "0.2.0", None, &[signer.public_hex().as_str()])
            .await
            .unwrap();
        assert!(got.is_none(), "equal version is not an update");
    }
}
