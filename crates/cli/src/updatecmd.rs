//! `noema update …` — offline tooling to mint the signed auto-update manifest the
//! VPS serves at `/update/latest`; runs locally so the signing key never touches CI.

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use noema_core::update::{AppRelease, PlatformAsset, ReleaseManifest, RELEASE_MANIFEST_SCHEMA};
use noema_core::util::now_unix_millis;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum UpdateCmd {
    /// Generate an Ed25519 release-signing keypair. Print the PUBLIC key to bake
    /// into the client (`UPDATE_RELEASE_PUBKEYS` in crates/core/src/update.rs) and
    /// keep the SECRET offline — it is the trust root for every auto-update.
    Keygen(KeygenArgs),
    /// Build and sign a release manifest from a spec + a directory of built assets.
    Sign(SignArgs),
    /// Verify a release manifest's signature against one or more public keys.
    Verify(VerifyArgs),
    /// Pretty-print a release manifest.
    Show {
        /// Path to a `release-manifest.json`.
        manifest: PathBuf,
    },
}

#[derive(Args)]
pub struct KeygenArgs {
    /// Write the 32-byte secret seed (hex) to this file (0600). Otherwise it's
    /// printed to stdout once — copy it somewhere safe.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Args)]
pub struct SignArgs {
    /// JSON spec describing apps + assets (without hashes/sizes). See module docs.
    #[arg(long)]
    spec: PathBuf,
    /// Directory holding the built asset files, matched by each asset's `name`.
    #[arg(long)]
    assets_dir: PathBuf,
    /// Output path for the signed manifest.
    #[arg(long, default_value = "release-manifest.json")]
    out: PathBuf,
    /// 32-byte ed25519 secret seed (hex). Prefer `--secret-file` or the
    /// `NOEMA_UPDATE_SECRET` env var so the key isn't in your shell history.
    #[arg(long)]
    secret: Option<String>,
    /// File containing the secret seed (hex).
    #[arg(long)]
    secret_file: Option<PathBuf>,
    /// Base URL prefixed to each asset's `name` when the spec omits an explicit
    /// `url`, e.g. `https://github.com/owner/repo/releases/download/v0.2.0`.
    #[arg(long)]
    base_url: Option<String>,
    /// How many days until the manifest is considered stale (anti-freeze). Keep it
    /// short — sized to your release cadence.
    #[arg(long, default_value_t = 14)]
    expires_days: i64,
}

#[derive(Args)]
pub struct VerifyArgs {
    /// Path to a `release-manifest.json`.
    manifest: PathBuf,
    /// Trusted ed25519 public key(s) (64-hex). Repeatable.
    #[arg(long = "pubkey", required = true)]
    pubkeys: Vec<String>,
}

/// The signing spec: apps and their assets, minus the per-asset hash/size the tool
/// fills in by reading the files.
#[derive(Deserialize)]
struct Spec {
    #[serde(default = "default_channel")]
    channel: String,
    apps: Vec<SpecApp>,
}

fn default_channel() -> String {
    "stable".to_string()
}

#[derive(Deserialize)]
struct SpecApp {
    app: String,
    version: String,
    #[serde(default)]
    min_supported: String,
    #[serde(default)]
    notes_url: String,
    assets: Vec<SpecAsset>,
}

#[derive(Deserialize)]
struct SpecAsset {
    os: String,
    arch: String,
    #[serde(default)]
    flavor: String,
    /// File name on the GitHub release (also the file name inside `--assets-dir`).
    name: String,
    /// Explicit download URL. If omitted, `--base-url` + `name` is used.
    #[serde(default)]
    url: Option<String>,
    /// Path (relative to `--assets-dir`) of a detached minisign `.sig` file whose
    /// *contents* get inlined (Studio/Tauri assets). Defaults to `<name>.sig` when
    /// that file exists.
    #[serde(default)]
    sig_file: Option<String>,
}

pub fn run(cmd: UpdateCmd) -> Result<()> {
    match cmd {
        UpdateCmd::Keygen(a) => keygen(&a),
        UpdateCmd::Sign(a) => sign(&a),
        UpdateCmd::Verify(a) => verify(&a),
        UpdateCmd::Show { manifest } => show(&manifest),
    }
}

fn keygen(args: &KeygenArgs) -> Result<()> {
    let kp = noema_core::sign::KeyPair::generate();
    println!("public_key : {}", kp.public_hex());
    println!("key_id     : {}", kp.key_id());
    println!();
    println!("Bake the public key into UPDATE_RELEASE_PUBKEYS (crates/core/src/update.rs).");
    if let Some(out) = &args.out {
        write_secret_file(out, &kp.secret_hex())
            .with_context(|| format!("writing secret to {}", out.display()))?;
        println!(
            "Wrote secret seed to {} (0600) — keep it OFFLINE.",
            out.display()
        );
    } else {
        println!();
        println!(
            "secret_key : {}  <-- store OFFLINE, never commit, never put in CI",
            kp.secret_hex()
        );
    }
    Ok(())
}

/// Write a secret to disk created with 0600 from the start (no brief world-readable
/// window between create and chmod) on Unix; plain write elsewhere.
fn write_secret_file(path: &Path, contents: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(contents.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

fn read_secret(args: &SignArgs) -> Result<String> {
    let raw = if let Some(s) = &args.secret {
        s.trim().to_string()
    } else if let Some(f) = &args.secret_file {
        std::fs::read_to_string(f)
            .with_context(|| format!("reading secret file {}", f.display()))?
            .trim()
            .to_string()
    } else if let Ok(s) = std::env::var("NOEMA_UPDATE_SECRET") {
        s.trim().to_string()
    } else {
        String::new()
    };
    if raw.is_empty() {
        bail!("no signing key: pass --secret-file, --secret, or set NOEMA_UPDATE_SECRET");
    }
    // Validate the shape up front so a malformed key fails fast, before we hash every asset.
    if raw.len() != 64 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("signing key must be a 32-byte ed25519 seed as 64 hex chars");
    }
    Ok(raw)
}

fn sign(args: &SignArgs) -> Result<()> {
    let spec_bytes = std::fs::read(&args.spec)
        .with_context(|| format!("reading spec {}", args.spec.display()))?;
    let spec: Spec = serde_json::from_slice(&spec_bytes).context("parsing spec JSON")?;
    let secret = read_secret(args)?;

    let now = now_unix_millis();
    let mut apps = Vec::with_capacity(spec.apps.len());
    for sa in &spec.apps {
        let mut assets = Vec::with_capacity(sa.assets.len());
        for asset in &sa.assets {
            let file = args.assets_dir.join(&asset.name);
            let bytes = std::fs::read(&file)
                .with_context(|| format!("reading asset {}", file.display()))?;
            let sha256 = hex::encode(Sha256::digest(&bytes));
            let size = bytes.len() as u64;

            let url = match &asset.url {
                Some(u) => u.clone(),
                None => {
                    let base = args.base_url.as_deref().with_context(|| {
                        format!(
                            "asset {} has no `url` and no --base-url was given",
                            asset.name
                        )
                    })?;
                    format!("{}/{}", base.trim_end_matches('/'), asset.name)
                }
            };

            let signature = load_sig(&args.assets_dir, asset)?;

            assets.push(PlatformAsset {
                os: asset.os.clone(),
                arch: asset.arch.clone(),
                flavor: asset.flavor.clone(),
                name: asset.name.clone(),
                url,
                sha256,
                size,
                signature,
            });
        }
        apps.push(AppRelease {
            app: sa.app.clone(),
            version: sa.version.clone(),
            min_supported: sa.min_supported.clone(),
            notes_url: sa.notes_url.clone(),
            assets,
        });
    }

    if args.expires_days <= 0 {
        bail!(
            "--expires-days must be positive (got {})",
            args.expires_days
        );
    }
    let ttl_ms = args.expires_days.saturating_mul(24 * 60 * 60 * 1000);
    let mut manifest = ReleaseManifest {
        schema: RELEASE_MANIFEST_SCHEMA,
        channel: spec.channel.clone(),
        generated_at: now,
        expires_at: now.saturating_add(ttl_ms),
        apps,
        signatures: vec![],
    };
    manifest.sign(&secret).context("signing manifest")?;

    let json = manifest.to_json_pretty().context("encoding manifest")?;
    std::fs::write(&args.out, &json).with_context(|| format!("writing {}", args.out.display()))?;

    // Echo the signing public key (bare hex, matching keygen + UPDATE_RELEASE_PUBKEYS)
    // so the operator can confirm it matches the key baked into the shipped clients.
    let signed_by: Vec<&str> = manifest
        .signatures
        .iter()
        .map(|s| s.key_id.trim_start_matches("ed25519:"))
        .collect();
    println!(
        "Wrote {} ({} app(s))",
        args.out.display(),
        manifest.apps.len()
    );
    println!("Channel    : {}", manifest.channel);
    println!("Expires in : {} day(s)", args.expires_days);
    println!("Signed by  : {}", signed_by.join(", "));
    Ok(())
}

fn load_sig(assets_dir: &Path, asset: &SpecAsset) -> Result<String> {
    let candidate = match &asset.sig_file {
        Some(rel) => assets_dir.join(rel),
        None => assets_dir.join(format!("{}.sig", asset.name)),
    };
    if candidate.exists() {
        let s = std::fs::read_to_string(&candidate)
            .with_context(|| format!("reading sig {}", candidate.display()))?;
        Ok(s.trim().to_string())
    } else if asset.sig_file.is_some() {
        bail!("declared sig_file {} not found", candidate.display());
    } else {
        Ok(String::new())
    }
}

fn verify(args: &VerifyArgs) -> Result<()> {
    let bytes = std::fs::read(&args.manifest)
        .with_context(|| format!("reading {}", args.manifest.display()))?;
    let manifest = ReleaseManifest::from_json(&bytes).context("parsing manifest")?;
    let trust: Vec<&str> = args.pubkeys.iter().map(|s| s.as_str()).collect();
    if manifest.is_signed_by_trusted(&trust) {
        println!("OK: signed by a trusted key");
        Ok(())
    } else {
        bail!("FAIL: manifest is not signed by any of the given public keys");
    }
}

fn show(path: &Path) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let manifest = ReleaseManifest::from_json(&bytes).context("parsing manifest")?;
    println!("{}", manifest.to_json_pretty()?);
    Ok(())
}
