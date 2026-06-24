mod manifestcmd;
mod ui;
mod updatecmd;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use manifestcmd::ManifestCmd;
use noema_core::engine::{Engine, EngineConfig, EvictPolicy, Progress};
use noema_core::platform::PlatformProfile;
use noema_core::policy::PolicyConfig;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "noema",
    version,
    about = "Verified multi-source distribution for local LLM weights",
    long_about = None
)]
struct Cli {
    /// Store root (default: per-user app data dir, or $NOEMA_HOME).
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    /// Trusted signing key ids (repeatable).
    #[arg(long = "trusted", global = true)]
    trusted: Vec<String>,
    /// Allow otherwise-blocked unsafe file types (e.g. pickle).
    #[arg(long, global = true)]
    allow_unsafe: bool,
    /// Require any accepted manifest to be signed by a trusted key.
    #[arg(long, global = true)]
    require_trusted: bool,
    /// Override platform profile.
    #[arg(long, global = true, value_enum)]
    platform: Option<PlatformArg>,
    /// Worldwide P2P content tracker URL (hash → peers, for discovery beyond a
    /// single host).
    #[arg(long, global = true)]
    tracker: Option<String>,
    /// Hugging Face Hub endpoint for search + downloads. Set to a mirror (e.g.
    /// https://hf-mirror.com) to use it exactly like the real Hub. Honors the
    /// standard `HF_ENDPOINT` environment variable.
    #[arg(long, global = true, env = "HF_ENDPOINT")]
    hf_endpoint: Option<String>,
    /// Route the app's internet traffic through a proxy ("VPN tunnel"): accepts
    /// http://, https://, socks5:// or socks5h://.
    #[arg(long, global = true, env = "NOEMA_PROXY")]
    proxy: Option<String>,
    /// Verbose logging.
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Copy, Clone, ValueEnum)]
enum PlatformArg {
    Desktop,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate an Ed25519 signing key (stored in the OS keystore).
    Keygen(KeygenArgs),
    /// Build, sign, verify, or show a manifest.
    #[command(subcommand)]
    Manifest(ManifestCmd),
    /// Mint or inspect the signed auto-update manifest (offline release tooling).
    #[command(subcommand)]
    Update(updatecmd::UpdateCmd),
    /// Import a manifest into the local store.
    Import { manifest: PathBuf },
    /// List imported manifests.
    Ls,
    /// Search files across sources (local cache + optional registry).
    Search(SearchArgs),
    /// Search and download from Hugging Face — no manifests needed.
    #[command(subcommand)]
    Hf(HfCmd),
    /// Show the source plan for a manifest.
    Plan { manifest_id: String },
    /// Download and verify all artifacts of a manifest.
    Download { manifest_id: String },
    /// Materialize an install of a cached manifest into a directory.
    Install {
        manifest_id: String,
        target: PathBuf,
    },
    /// Import a local file as an artifact (avoids re-downloading).
    ImportFile {
        manifest_id: String,
        artifact_path: String,
        file: PathBuf,
    },
    /// Import a local model file and optionally share it. Titles it from the
    /// file's header + filename; the flags override. Off-HF by default — pass
    /// `--check-hf` to also try matching it on Hugging Face.
    ImportLocal {
        file: PathBuf,
        /// Display title (defaults to the file's embedded name or filename).
        #[arg(long)]
        name: Option<String>,
        /// License tag (SPDX / open-weight family), e.g. `apache-2.0`. An unknown
        /// license stays download-only and won't auto-reshare.
        #[arg(long)]
        license: Option<String>,
        /// Quantization label, e.g. `Q4_K_M`.
        #[arg(long)]
        quant: Option<String>,
        /// Model family, e.g. `Mistral`.
        #[arg(long)]
        family: Option<String>,
        /// A short description / note shown to receivers.
        #[arg(long)]
        description: Option<String>,
        /// Where it came from (e.g. the old Hugging Face URL).
        #[arg(long)]
        origin: Option<String>,
        /// Publish it to the worldwide Explore mesh right away.
        #[arg(long)]
        share: bool,
        /// Also try to match it on Hugging Face (off by default).
        #[arg(long)]
        check_hf: bool,
    },
    /// Share a Library model on the mesh (by manifest id, name, or hash prefix).
    Share { model: String },
    /// Stop sharing a Library model (by manifest id, name, or hash prefix).
    Unshare { model: String },
    /// Print a copy-pasteable share link (atlas1:…) for a Library model.
    ShareLink { model: String },
    /// Bundle several local files (a whole sharded model — weight shards +
    /// config/tokenizer) into ONE share link. Each file is imported + hashed;
    /// the printed `atlasb1:` link fetches them all on another device.
    ShareBundle {
        /// Display name for the whole model, e.g. "Llama-3.1-70B-Instruct".
        #[arg(long)]
        name: String,
        /// License applied to every file (a known open license auto-reseeds).
        #[arg(long)]
        license: Option<String>,
        /// The model's files (weight shards + config.json + tokenizer.json …).
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Receive a model from a share link (`atlas1:…`) or a bare content id
    /// every byte against the content id. The sender's title/license are
    /// advisory ("sender says") — only the content hash is trusted.
    Add {
        /// An `atlas1:…` link or a 64-hex sha256 content id.
        link: String,
        /// Optionally also install the verified file to this path.
        #[arg(long)]
        into: Option<PathBuf>,
    },
    /// List downloaded models.
    Installed,
    /// Import model files you already have from other tools (LM Studio, llama.cpp,
    /// GPT4All, the Hugging Face cache, …) — no re-download. Scans directories
    /// recursively for `.gguf` files (defaults to common locations). Note: Ollama
    /// stores blobs without a `.gguf` extension, so its store isn't auto-detected.
    ScanImport {
        /// Directories to scan recursively (defaults to common tool folders).
        dirs: Vec<PathBuf>,
        /// Also match each file on Hugging Face for provenance + license.
        #[arg(long)]
        check_hf: bool,
    },
    /// Prune the index to match what's on disk (drop deleted models).
    Reconcile,
    /// Inspect or evict the content-addressed cache.
    #[command(subcommand)]
    Cache(CacheCmd),
    /// Show source health / reputation.
    Health,
    /// Export a JSON diagnostics bundle.
    Diagnostics,
    /// Manage access tokens in the OS keystore.
    #[command(subcommand)]
    Token(TokenCmd),
    /// Provide a file over iroh (prints a ticket to share). Requires `--features iroh`.
    #[cfg(feature = "iroh")]
    IrohServe { file: PathBuf },
    /// Fetch a file over iroh by ticket. Requires `--features iroh`.
    #[cfg(feature = "iroh")]
    IrohFetch { ticket: String, out: PathBuf },
    /// Share your models WORLDWIDE: seed over iroh + announce to the tracker.
    /// Requires `--features iroh` and `--tracker <url>`. Runs until Ctrl-C.
    #[cfg(feature = "iroh")]
    P2pShare,
    /// Launch the local web dashboard (loopback only).
    Ui(UiArgs),
}

#[derive(Args)]
struct KeygenArgs {
    /// Also write the secret seed (hex) to this file (use with care).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Do not store the secret in the OS keystore.
    #[arg(long)]
    no_store: bool,
}

#[derive(Subcommand)]
enum CacheCmd {
    /// List cached blobs.
    Ls,
    /// Evict cache entries.
    Evict {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        unreferenced: bool,
        #[arg(long)]
        blob: Option<String>,
    },
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Store a token for a service (e.g. `huggingface`).
    Set {
        service: String,
        /// Token value (omit to read from stdin).
        #[arg(long)]
        value: Option<String>,
    },
    /// Remove a stored token.
    Rm { service: String },
}

#[derive(Subcommand)]
enum HfCmd {
    /// Search the Hub for models.
    Search {
        query: String,
        #[arg(long, default_value_t = 15)]
        limit: usize,
    },
    /// List a model's downloadable weight files.
    Files { model_id: String },
    /// Download a model file (verifies sha256); optionally install into a dir.
    ///
    /// Omit `file` for a safetensors/MLX repo to fetch the whole model in one go
    Get {
        model_id: String,
        /// File name (or a substring to match, e.g. `q4_k_m`). Omit to bundle a
        /// sharded safetensors/MLX model.
        file: Option<String>,
        #[arg(long)]
        into: Option<PathBuf>,
    },
}

#[derive(Args)]
struct SearchArgs {
    /// Search query (matches model name, file path, or hash). Empty = list all.
    query: Option<String>,
    /// Also query a remote registry (e.g. http://localhost:8077).
    #[arg(long)]
    registry: Option<String>,
}

#[derive(Args)]
struct UiArgs {
    /// Bind address (loopback only by default — this is an admin surface).
    #[arg(long, default_value = "127.0.0.1:8090")]
    addr: String,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let level = if cli.verbose { "debug" } else { "info" };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| format!("noema={level},noema_core={level}").into()),
        )
        .with_target(false)
        .try_init();

    if let Err(e) = run(cli).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// Global flags extracted from `Cli` so the command `cmd` can be matched by value.
struct Globals {
    root: Option<PathBuf>,
    trusted: Vec<String>,
    allow_unsafe: bool,
    require_trusted: bool,
    platform: Option<PlatformArg>,
    tracker: Option<String>,
    hf_endpoint: Option<String>,
    proxy: Option<String>,
}

fn build_engine(g: &Globals) -> Result<Engine> {
    let root = g
        .root
        .clone()
        .unwrap_or_else(noema_core::paths::default_root);
    let mut cfg = EngineConfig::new(root);
    cfg.platform = match g.platform {
        Some(PlatformArg::Desktop) => PlatformProfile::desktop(),
        None => PlatformProfile::detect(),
    };
    cfg.policy = PolicyConfig {
        trusted_keys: g.trusted.iter().cloned().collect::<HashSet<_>>(),
        require_signature_always: false,
        allow_unsafe_files: g.allow_unsafe,
        require_trusted_signer: g.require_trusted,
    };
    cfg.tracker_url = g.tracker.clone();
    if let Some(endpoint) = g
        .hf_endpoint
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        cfg.transport.hf_endpoint = endpoint.to_string();
    }
    if let Some(proxy) = g.proxy.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        cfg.transport.proxy = Some(proxy.to_string());
    }
    Engine::open(cfg).context("opening engine")
}

async fn run(cli: Cli) -> Result<()> {
    let Cli {
        root,
        trusted,
        allow_unsafe,
        require_trusted,
        platform,
        tracker,
        hf_endpoint,
        proxy,
        verbose: _,
        cmd,
    } = cli;
    let g = Globals {
        root,
        trusted,
        allow_unsafe,
        require_trusted,
        platform,
        tracker,
        hf_endpoint,
        proxy,
    };
    match cmd {
        Cmd::Keygen(args) => keygen(&args),
        Cmd::Manifest(cmd) => match cmd {
            ManifestCmd::Build(a) => manifestcmd::build(a),
            ManifestCmd::Sign(a) => manifestcmd::sign(a),
            ManifestCmd::Verify(a) => manifestcmd::verify(a),
            ManifestCmd::Show(a) => manifestcmd::show(a),
        },
        Cmd::Update(cmd) => updatecmd::run(cmd),
        Cmd::Import { manifest } => {
            let engine = build_engine(&g)?;
            let res = engine.import_manifest_path(&manifest)?;
            println!("imported  : {}", res.manifest_id);
            println!(
                "signed    : {} ({} valid signature(s))",
                res.report.is_signed(),
                res.report.valid_signatures.len()
            );
            println!(
                "policy    : {}",
                if res.policy.allowed { "allow" } else { "DENY" }
            );
            println!("           {}", res.policy.reason);
            for w in &res.policy.warnings {
                println!("warning   : {w}");
            }
            Ok(())
        }
        Cmd::Ls => {
            let engine = build_engine(&g)?;
            for m in engine.list_manifests()? {
                println!(
                    "{}  {:<28} {} {}{}",
                    m.manifest_id,
                    m.model_name,
                    m.license_spdx,
                    if m.signed { "signed" } else { "unsigned" },
                    if m.gated { " gated" } else { "" },
                );
            }
            Ok(())
        }
        Cmd::Search(args) => {
            let engine = build_engine(&g)?;
            let q = args.query.unwrap_or_default();
            let cached = engine.cached_hashes()?;
            let mut manifests = engine.all_manifests()?;
            if let Some(reg) = &args.registry {
                let remote = engine.registry_search(reg, &q).await?;
                println!("registry {reg}: {} manifest(s)", remote.len());
                manifests.extend(remote);
            }
            let results = noema_core::aggregate_results(&manifests, &cached, &q);
            if results.is_empty() {
                println!("no files found");
            }
            for r in &results {
                println!(
                    "\n{}  ({} bytes){}",
                    r.display_name,
                    r.size_bytes,
                    if r.cached { "  [cached]" } else { "" }
                );
                println!("  blake3 {}", r.blake3);
                if !r.models.is_empty() {
                    println!("  models: {}", r.models.join(", "));
                }
                println!("  {} source(s):", r.sources.len());
                for s in &r.sources {
                    println!(
                        "    - {:<13} {}{}",
                        format!("{:?}", s.class),
                        s.locator,
                        if s.requires_auth { " (auth)" } else { "" }
                    );
                }
            }
            Ok(())
        }
        Cmd::Hf(cmd) => {
            let engine = build_engine(&g)?;
            match cmd {
                HfCmd::Search { query, limit } => {
                    let models = engine.hf_search(&query, limit).await?;
                    if models.is_empty() {
                        println!("no models found for '{query}'");
                    }
                    for m in &models {
                        println!(
                            "\n{}\n  ↓ {} downloads · ♥ {} · {}{}",
                            m.id,
                            m.downloads,
                            m.likes,
                            m.pipeline_tag.clone().unwrap_or_else(|| "—".into()),
                            if m.gated { " · 🔒 gated" } else { "" },
                        );
                        if m.has_gguf() {
                            println!("  GGUF available");
                        }
                    }
                    Ok(())
                }
                HfCmd::Files { model_id } => {
                    let detail = engine.hf_model_detail(&model_id).await?;
                    println!(
                        "{} @ {}",
                        detail.id,
                        &detail.revision[..detail.revision.len().min(12)]
                    );
                    if detail.gated {
                        println!(
                            "  🔒 gated — needs a Hugging Face token (noema token set huggingface)"
                        );
                    }
                    for f in detail.weight_files() {
                        println!(
                            "  {:<48} {:>12}  sha256:{}…",
                            f.rfilename,
                            human(f.size),
                            &f.sha256.clone().unwrap_or_default()
                                [..12.min(f.sha256.as_ref().map(|s| s.len()).unwrap_or(0))],
                        );
                    }
                    Ok(())
                }
                HfCmd::Get {
                    model_id,
                    file,
                    into,
                } => {
                    let detail = engine.hf_model_detail(&model_id).await?;
                    // No file given: bundle the whole safetensors/MLX model.
                    let imported = match file {
                        None => {
                            if !detail.has_safetensors_bundle() {
                                anyhow::bail!(
                                    "no `file` given and {model_id} has no safetensors model to \
                                     bundle — specify a file (try `noema hf files {model_id}`)"
                                );
                            }
                            let shards = detail.safetensors_shards().len();
                            let sidecars = detail.model_sidecars().len();
                            println!(
                                "resolving {} · {} ({shards} shard(s) + {sidecars} sidecar(s), {})",
                                detail.id,
                                detail.bundle_variant_label(),
                                human(detail.bundle_total_size()),
                            );
                            engine.hf_import_bundle(&detail)?
                        }
                        Some(file) => {
                            let chosen = detail
                                .weight_files()
                                .into_iter()
                                .find(|f| {
                                    f.rfilename == file
                                        || f.rfilename.to_lowercase().contains(&file.to_lowercase())
                                })
                                .cloned()
                                .with_context(|| {
                                    format!("no weight file matching '{file}' in {model_id}")
                                })?;
                            println!("resolving {} / {}", detail.id, chosen.rfilename);
                            engine.hf_import_file(&detail, &chosen)?
                        }
                    };
                    if !imported.policy.allowed {
                        anyhow::bail!("policy denied: {}", imported.policy.reason);
                    }
                    let progress = make_progress();
                    let outcome = engine
                        .download(&imported.manifest_id, Some(progress))
                        .await?;
                    println!();
                    for a in &outcome.artifacts {
                        println!(
                            "✓ {} ({}) from {}",
                            a.artifact_path,
                            human(a.size_bytes),
                            a.source_id.clone().unwrap_or_default()
                        );
                    }
                    if let Some(dir) = into {
                        let views = engine.materialize_install(&imported.manifest_id, &dir)?;
                        for v in views {
                            println!(
                                "→ installed {} ({})",
                                v.dest.display(),
                                v.link_kind.as_str()
                            );
                        }
                    }
                    Ok(())
                }
            }
        }
        Cmd::Plan { manifest_id } => {
            let engine = build_engine(&g)?;
            for (path, plan) in engine.plan_download(&manifest_id)? {
                println!("artifact: {path}");
                for s in &plan.eligible {
                    println!("  [{:>6.1}] {}", s.score, s.source_id);
                }
                for x in &plan.excluded {
                    println!("  [  skip] {} — {}", x.source_id, x.reason);
                }
            }
            Ok(())
        }
        Cmd::Download { manifest_id } => {
            let engine = build_engine(&g)?;
            let progress = make_progress();
            // Race the download against Ctrl-C so an interrupted CLI download isn't
            // just killed mid-write: the first Ctrl-C pauses (keeps the partial so
            // re-running resumes); a second stops and discards it. Both leave a
            // clean state instead of an orphaned `.part`.
            let dl = engine.download(&manifest_id, Some(progress));
            tokio::pin!(dl);
            let mut interrupts = 0u8;
            let outcome = loop {
                tokio::select! {
                    res = &mut dl => break res,
                    _ = tokio::signal::ctrl_c() => {
                        interrupts += 1;
                        if interrupts == 1 {
                            eprintln!("\nPausing — partial saved (Ctrl-C again to stop & discard)…");
                            engine.request_pause();
                        } else {
                            eprintln!("\nStopping — discarding partial…");
                            engine.request_stop();
                        }
                    }
                }
            };
            let outcome = match outcome {
                Ok(o) => o,
                Err(noema_core::Error::Cancelled) => {
                    println!(
                        "\nPaused — partial saved. Re-run `download {manifest_id}` to resume."
                    );
                    return Ok(());
                }
                Err(noema_core::Error::Stopped) => {
                    println!("\nStopped — partial discarded.");
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            };
            println!();
            for a in &outcome.artifacts {
                println!(
                    "✓ {} ({} bytes) {}{}",
                    a.artifact_path,
                    a.size_bytes,
                    if a.from_cache {
                        "already cached".to_string()
                    } else {
                        format!("from {}", a.source_id.clone().unwrap_or_default())
                    },
                    if a.warnings.is_empty() {
                        String::new()
                    } else {
                        format!("  [{} warning(s)]", a.warnings.len())
                    }
                );
            }
            Ok(())
        }
        Cmd::Install {
            manifest_id,
            target,
        } => {
            let engine = build_engine(&g)?;
            let views = engine.materialize_install(&manifest_id, &target)?;
            for v in views {
                println!(
                    "{} -> {} ({})",
                    v.artifact_path,
                    v.dest.display(),
                    v.link_kind.as_str()
                );
            }
            Ok(())
        }
        Cmd::ImportFile {
            manifest_id,
            artifact_path,
            file,
        } => {
            let engine = build_engine(&g)?;
            let out = engine.import_artifact_file(&manifest_id, &artifact_path, &file)?;
            println!(
                "imported {} ({} bytes) into cache",
                out.artifact_path, out.size_bytes
            );
            Ok(())
        }
        Cmd::ImportLocal {
            file,
            name,
            license,
            quant,
            family,
            description,
            origin,
            share,
            check_hf,
        } => {
            let engine = build_engine(&g)?;
            println!("hashing {}…", file.display());
            let meta = noema_core::LocalShareMeta {
                title: name.clone(),
                family: family.clone(),
                quant: quant.clone(),
                architecture: None,
                license: license.clone(),
                description: description.clone(),
                origin_url: origin.clone(),
                skip_hf_match: !check_hf,
                publish: share,
            };
            let out = engine.import_local_file_with_meta(&file, meta).await?;
            println!("imported: {} ({})", out.model_name, human(out.size_bytes));
            if out.matched {
                println!(
                    "✓ matched on Hugging Face: {}",
                    out.matched_model_id.clone().unwrap_or_default()
                );
            } else {
                println!("• titled as a local model (not on Hugging Face)");
            }
            println!(
                "  P2P: {}",
                if out.shareable {
                    "shared to the mesh"
                } else if share {
                    "publishing…"
                } else {
                    "private — pass --share (and a known --license) to publish"
                }
            );
            let link = noema_core::ShareTarget {
                name: out.model_name.clone(),
                size: out.size_bytes,
                sha256: out.sha256.clone(),
                blake3: out.blake3.clone(),
                license: license.unwrap_or_default(),
                title: name.unwrap_or_else(|| out.model_name.clone()),
                family: family.unwrap_or_default(),
                quant: quant.unwrap_or_default(),
                desc: description.unwrap_or_default(),
                origin: origin.unwrap_or_default(),
                magnet: engine.bt_magnet(&out.blake3),
            };
            println!("  share link: {}", link.encode());
            Ok(())
        }
        Cmd::Share { ref model } | Cmd::Unshare { ref model } => {
            let on = matches!(cmd, Cmd::Share { .. });
            // Arc so `set_model_shared` (which seeds over BT on the live runtime)
            // can take `self: &Arc<Self>`.
            let engine = std::sync::Arc::new(build_engine(&g)?);
            let m = find_installed(&engine, model)?;
            engine.set_model_shared(&m.blake3, &m.sha256, on)?;
            println!(
                "{} {}",
                if on { "✓ sharing" } else { "stopped sharing" },
                m.name
            );
            Ok(())
        }
        Cmd::ShareLink { model } => {
            let engine = build_engine(&g)?;
            let m = find_installed(&engine, &model)?;
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
                magnet: engine.bt_magnet(&m.blake3),
            };
            println!("{}", link.encode());
            Ok(())
        }
        Cmd::ShareBundle {
            name,
            license,
            files,
        } => {
            let engine = build_engine(&g)?;
            let mut targets = Vec::new();
            for file in &files {
                println!("hashing {}…", file.display());
                let meta = noema_core::LocalShareMeta {
                    title: None,
                    family: None,
                    quant: None,
                    architecture: None,
                    license: license.clone(),
                    description: None,
                    origin_url: None,
                    skip_hf_match: true,
                    // Always seed the bundle's files — the user explicitly ran
                    // `share-bundle`, so the link they get must be servable from
                    // their own node regardless of whether a license was given.
                    publish: true,
                };
                let out = engine.import_local_file_with_meta(file, meta).await?;
                // Preserve the on-disk filename so the receiver reconstructs the
                // model's exact layout (model-00001-of-…, config.json, …).
                let fname = file
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| out.model_name.clone());
                targets.push(noema_core::ShareTarget {
                    name: fname,
                    size: out.size_bytes,
                    sha256: out.sha256.clone(),
                    blake3: out.blake3.clone(),
                    license: license.clone().unwrap_or_default(),
                    ..Default::default()
                });
            }
            let bundle = noema_core::ShareBundle {
                name: name.clone(),
                files: targets,
            };
            println!("\n✓ bundled {} file(s) as \"{}\"", bundle.files.len(), name);
            println!("  bundle link: {}", bundle.encode());
            Ok(())
        }
        Cmd::Add { link, into } => {
            let engine = build_engine(&g)?;
            // A multi-file bundle link (`atlasb1:`) fetches a whole sharded model.
            if noema_core::is_bundle_link(&link) {
                let bundle = noema_core::ShareBundle::decode(link.trim())
                    .context("not a valid atlas bundle link")?;
                println!(
                    "receiving bundle \"{}\" ({} files)…",
                    bundle.name,
                    bundle.files.len()
                );
                let progress = make_progress();
                let outs = engine.add_bundle(bundle, Some(progress)).await?;
                println!();
                for o in &outs {
                    for a in &o.artifacts {
                        println!("✓ {} ({} bytes)", a.artifact_path, a.size_bytes);
                    }
                    if let Some(dest) = &into {
                        for v in engine.materialize_install(&o.manifest_id, dest)? {
                            println!(
                                "installed -> {} ({})",
                                v.dest.display(),
                                v.link_kind.as_str()
                            );
                        }
                    }
                }
                return Ok(());
            }
            let target = noema_core::ShareTarget::decode(link.trim())
                .context("not a valid atlas1: link or sha256 content id")?;
            if !target.has_content_id() {
                bail!("that link carries no content id to fetch");
            }
            if !target.title.trim().is_empty() {
                println!("receiving (sender says: {})…", target.title.trim());
            }
            let progress = make_progress();
            let outcome = engine.add_by_content(target, Some(progress)).await?;
            println!();
            for a in &outcome.artifacts {
                println!(
                    "✓ {} ({} bytes) {}",
                    a.artifact_path,
                    a.size_bytes,
                    if a.from_cache {
                        "already cached".to_string()
                    } else {
                        format!("from {}", a.source_id.clone().unwrap_or_default())
                    }
                );
            }
            if let Some(dest) = into {
                for v in engine.materialize_install(&outcome.manifest_id, &dest)? {
                    println!(
                        "installed -> {} ({})",
                        v.dest.display(),
                        v.link_kind.as_str()
                    );
                }
            }
            Ok(())
        }
        Cmd::Installed => {
            let engine = build_engine(&g)?;
            let models = engine.installed_models()?;
            if models.is_empty() {
                println!("no models downloaded yet");
            }
            for m in models {
                println!(
                    "{:<48} {:>10}  {}{}",
                    m.name,
                    human(m.size_bytes),
                    if m.from_hf { "🤗" } else { "📁" },
                    if m.shareable { " · shareable" } else { "" },
                );
            }
            Ok(())
        }
        Cmd::ScanImport { dirs, check_hf } => {
            let engine = build_engine(&g)?;
            let scan: Vec<PathBuf> = if dirs.is_empty() {
                common_model_dirs()
            } else {
                dirs
            };
            if scan.is_empty() {
                println!("no known model folders found — pass directories to scan, e.g. `noema scan-import ~/models`");
                return Ok(());
            }
            let mut found = Vec::new();
            for d in &scan {
                find_gguf_files(d, &mut found, 6);
            }
            if found.is_empty() {
                println!(
                    "no .gguf files found under: {}",
                    scan.iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                return Ok(());
            }
            println!("found {} model file(s) to import…", found.len());
            let (mut ok, mut skipped) = (0u32, 0u32);
            for file in &found {
                let meta = noema_core::LocalShareMeta {
                    title: None,
                    family: None,
                    quant: None,
                    architecture: None,
                    license: None,
                    description: None,
                    origin_url: None,
                    skip_hf_match: !check_hf,
                    publish: false,
                };
                match engine.import_local_file_with_meta(file, meta).await {
                    Ok(out) => {
                        ok += 1;
                        let matched = if out.matched {
                            format!("  🤗 {}", out.matched_model_id.clone().unwrap_or_default())
                        } else {
                            String::new()
                        };
                        println!(
                            "  ✓ {} ({}){}",
                            out.model_name,
                            human(out.size_bytes),
                            matched
                        );
                    }
                    Err(e) => {
                        skipped += 1;
                        eprintln!("  • skipped {}: {e}", file.display());
                    }
                }
            }
            println!(
                "imported {ok}, skipped {skipped}. Run `noema installed` to see your library."
            );
            Ok(())
        }
        Cmd::Reconcile => {
            let engine = build_engine(&g)?;
            let r = engine.reconcile()?;
            println!(
                "reconciled: removed {} cache entr(y/ies) and {} install record(s) for deleted files",
                r.removed_blobs, r.removed_installs
            );
            Ok(())
        }
        Cmd::Cache(cmd) => {
            let engine = build_engine(&g)?;
            match cmd {
                CacheCmd::Ls => {
                    let mut total = 0u64;
                    for b in engine.list_cache()? {
                        total += b.size_bytes;
                        println!(
                            "{}  {:>14} bytes  {}",
                            &b.blake3[..16],
                            b.size_bytes,
                            b.state
                        );
                    }
                    println!("total: {total} bytes");
                }
                CacheCmd::Evict {
                    all,
                    unreferenced,
                    blob,
                } => {
                    let policy = if all {
                        EvictPolicy::All
                    } else if unreferenced {
                        EvictPolicy::Unreferenced
                    } else if let Some(b) = blob {
                        EvictPolicy::Blob(b)
                    } else {
                        bail!("specify --all, --unreferenced, or --blob <hash>");
                    };
                    let report = engine.evict_cache(policy)?;
                    println!(
                        "evicted {} blob(s), freed {} bytes",
                        report.removed.len(),
                        report.freed_bytes
                    );
                }
            }
            Ok(())
        }
        Cmd::Health => {
            let engine = build_engine(&g)?;
            for h in engine.report_source_health()? {
                println!(
                    "{}  ok:{} fail:{} integ:{} {}",
                    h.source_id,
                    h.success_count,
                    h.failure_count,
                    h.integrity_failures,
                    if h.banned { "BANNED" } else { "" }
                );
            }
            Ok(())
        }
        Cmd::Diagnostics => {
            let engine = build_engine(&g)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&engine.export_diagnostics()?)?
            );
            Ok(())
        }
        Cmd::Token(cmd) => run_token(&cmd),
        Cmd::Ui(args) => {
            let engine = std::sync::Arc::new(build_engine(&g)?);
            let addr: std::net::SocketAddr = args.addr.parse().context("parsing --addr")?;
            ui::run(engine, addr).await
        }
        #[cfg(feature = "iroh")]
        Cmd::IrohServe { file } => {
            let store =
                std::env::temp_dir().join(format!("noema-iroh-serve-{}", std::process::id()));
            let node = noema_core::iroh_node::IrohNode::spawn(&store).await?;
            let (ticket, hash) = node.provide(&file).await?;
            println!("serving {} over iroh", file.display());
            println!("blake3 : {hash}");
            println!("ticket : {ticket}");
            println!("\nOn another machine: noema iroh-fetch '{ticket}' <out>");
            println!("Press Ctrl-C to stop serving.");
            tokio::signal::ctrl_c().await.ok();
            node.shutdown().await;
            Ok(())
        }
        #[cfg(feature = "iroh")]
        Cmd::P2pShare => {
            let tracker = g
                .tracker
                .clone()
                .context("--tracker <url> is required for worldwide sharing")?;
            // `start_worldwide_share` takes `Arc<Self>` so its background task can
            // re-seed the shareable library over BitTorrent on launch.
            let engine = std::sync::Arc::new(build_engine(&g)?);
            let n = engine.share_announce_items()?.len();
            println!("sharing {n} model file(s) worldwide via the tracker at {tracker}…");
            let identity = noema_core::tracker::Identity {
                device: noema_core::identity::default_device_name(),
            };
            let share = engine.start_worldwide_share(tracker, identity).await?;
            println!("this node: {}", share.node_ticket());
            println!("Press Ctrl-C to stop.");
            tokio::signal::ctrl_c().await.ok();
            // Hard-disconnect peers + stop announcing on the way out.
            share.stop().await;
            Ok(())
        }
        #[cfg(feature = "iroh")]
        Cmd::IrohFetch { ticket, out } => {
            let store =
                std::env::temp_dir().join(format!("noema-iroh-fetch-{}", std::process::id()));
            let node = noema_core::iroh_node::IrohNode::spawn(&store).await?;
            println!("fetching over iroh…");
            node.fetch_to_file(&ticket, &out, None, None).await?;
            let (hashes, size) = noema_core::hash::hash_file(&out)?;
            let want = noema_core::iroh_node::IrohNode::ticket_hash(&ticket)?;
            let ok = hashes.blake3.eq_ignore_ascii_case(&want);
            println!(
                "wrote {} ({size} bytes), blake3 {} — {}",
                out.display(),
                &hashes.blake3[..16],
                if ok {
                    "verified ✓"
                } else {
                    "HASH MISMATCH ✗"
                }
            );
            node.shutdown().await;
            if ok {
                Ok(())
            } else {
                anyhow::bail!("fetched bytes do not match the ticket hash")
            }
        }
    }
}

fn keygen(args: &KeygenArgs) -> Result<()> {
    let kp = noema_core::sign::KeyPair::generate();
    let key_id = kp.key_id();
    println!("key_id     : {key_id}");
    println!("public_key : {}", kp.public_hex());
    if !args.no_store {
        let store = noema_core::secret::default_store();
        match store.set("signing", &key_id, &kp.secret_hex()) {
            Ok(()) => println!("stored secret in OS keystore (service `noema-atlas:signing`)"),
            Err(e) => eprintln!("warning: could not store in keystore ({e}); use --out to save it"),
        }
    }
    if let Some(out) = &args.out {
        let mut f = std::fs::File::create(out)?;
        f.write_all(kp.secret_hex().as_bytes())?;
        restrict_perms(out);
        println!("wrote secret seed to {} (keep it safe)", out.display());
    }
    Ok(())
}

fn run_token(cmd: &TokenCmd) -> Result<()> {
    let store = noema_core::secret::default_store();
    if !store.is_persistent() {
        bail!("no persistent keystore in this build; set credentials via environment variables");
    }
    match cmd {
        TokenCmd::Set { service, value } => {
            let token = match value {
                Some(v) => v.clone(),
                None => {
                    print!("token for `{service}`: ");
                    std::io::stdout().flush().ok();
                    let mut s = String::new();
                    std::io::stdin().read_line(&mut s)?;
                    s.trim().to_string()
                }
            };
            store.set(service, "default", &token)?;
            println!("stored token for `{service}`");
        }
        TokenCmd::Rm { service } => {
            store.delete(service, "default")?;
            println!("removed token for `{service}`");
        }
    }
    Ok(())
}

/// Resolve a Library model from a selector: an exact manifest id, a content-hash
/// prefix (≥6 hex), or a case-insensitive substring of the model name.
fn find_installed(engine: &Engine, selector: &str) -> Result<noema_core::InstalledModel> {
    let sel = selector.trim();
    let models = engine.installed_models()?;
    if models.is_empty() {
        bail!("no models in the library yet");
    }
    if let Some(m) = models.iter().find(|m| m.manifest_id == sel) {
        return Ok(m.clone());
    }
    let low = sel.to_lowercase();
    if low.len() >= 6 && low.bytes().all(|b| b.is_ascii_hexdigit()) {
        let hits: Vec<_> = models
            .iter()
            .filter(|m| m.blake3.starts_with(&low) || m.sha256.starts_with(&low))
            .collect();
        match hits.len() {
            1 => return Ok(hits[0].clone()),
            n if n > 1 => bail!("`{selector}` matches {n} models — be more specific"),
            _ => {}
        }
    }
    let hits: Vec<_> = models
        .iter()
        .filter(|m| m.name.to_lowercase().contains(&low))
        .collect();
    match hits.len() {
        1 => Ok(hits[0].clone()),
        0 => bail!("no model matches `{selector}` (try `noema installed`)"),
        n => bail!("`{selector}` matches {n} models — use a manifest id or hash prefix"),
    }
}

/// Common locations other local-LLM tools keep GGUF models, best-effort per OS.
/// Only existing directories are returned.
fn common_model_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let Some(h) = home else {
        return Vec::new();
    };
    let candidates = [
        h.join(".lmstudio").join("models"), // LM Studio (newer)
        h.join(".cache").join("lm-studio").join("models"), // LM Studio (older)
        h.join(".cache").join("llama.cpp"), // llama.cpp -hf cache
        h.join("Library")
            .join("Application Support")
            .join("nomic.ai")
            .join("GPT4All"), // GPT4All (macOS)
        h.join(".cache").join("gpt4all"),   // GPT4All (Linux)
        h.join(".cache").join("huggingface").join("hub"), // HF hub cache
    ];
    candidates.into_iter().filter(|d| d.is_dir()).collect()
}

/// Recursively collect `.gguf` files under `dir`, up to `depth` levels deep.
fn find_gguf_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth == 0 || !dir.is_dir() {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            find_gguf_files(&path, out, depth - 1);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("gguf"))
            .unwrap_or(false)
        {
            out.push(path);
        }
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

fn make_progress() -> Progress {
    Arc::new(|p: noema_core::engine::DownloadProgress| {
        if p.bytes_total > 0 {
            let pct = (p.bytes_done as f64 / p.bytes_total as f64) * 100.0;
            print!(
                "\r{:<8} {:>6.1}%  {}/{} bytes  {}        ",
                p.phase,
                pct,
                p.bytes_done,
                p.bytes_total,
                p.source_id.as_deref().unwrap_or("")
            );
        } else {
            print!("\r{:<8} {}", p.phase, p.artifact_path);
        }
        std::io::stdout().flush().ok();
    })
}

#[cfg(unix)]
fn restrict_perms(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(md) = std::fs::metadata(path) {
        let mut perms = md.permissions();
        perms.set_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn restrict_perms(path: &std::path::Path) {
    // No portable owner-only ACL without extra deps. Mark read-only as partial
    // protection and warn the user that the seed is not OS-locked here.
    if let Ok(md) = std::fs::metadata(path) {
        let mut perms = md.permissions();
        perms.set_readonly(true);
        let _ = std::fs::set_permissions(path, perms);
    }
    eprintln!(
        "warning: {} holds a secret seed and could not be locked to owner-only on this OS; \
         store it somewhere safe or prefer the OS keystore (omit --out).",
        path.display()
    );
}
