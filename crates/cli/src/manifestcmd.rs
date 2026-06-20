use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use noema_core::hash::{hash_file, ChunkTree, DEFAULT_LEAF_SIZE};
use noema_core::manifest::{
    Access, Artifact, AuthPolicy, Chunking, License, Manifest, Model, Publisher,
    RedistributionClass, Source, SourceClass, SCHEMA_VERSION,
};
use noema_core::sign::{verify_manifest, KeyPair};
use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum ManifestCmd {
    /// Build a manifest from local files, computing hashes + a Merkle chunk tree.
    Build(BuildArgs),
    /// Sign a manifest with an Ed25519 key.
    Sign(SignArgs),
    /// Verify a manifest's structure and signatures.
    Verify(VerifyArgs),
    /// Print a human summary of a manifest.
    Show(ShowArgs),
}

#[derive(Copy, Clone, ValueEnum)]
pub enum RedistArg {
    PublicP2p,
    PublicDownloadOnly,
    GatedNoRedistribution,
    EnterprisePrivate,
}

impl From<RedistArg> for RedistributionClass {
    fn from(r: RedistArg) -> Self {
        match r {
            RedistArg::PublicP2p => RedistributionClass::PublicP2pAllowed,
            RedistArg::PublicDownloadOnly => RedistributionClass::PublicDownloadOnly,
            RedistArg::GatedNoRedistribution => RedistributionClass::GatedNoRedistribution,
            RedistArg::EnterprisePrivate => RedistributionClass::EnterprisePrivate,
        }
    }
}

#[derive(Args)]
pub struct BuildArgs {
    /// Model display name.
    #[arg(long)]
    pub name: String,
    /// Publisher id (e.g. `hf:org/model` or your own namespace).
    #[arg(long, default_value = "local")]
    pub publisher_id: String,
    /// SPDX license id.
    #[arg(long, default_value = "apache-2.0")]
    pub license: String,
    /// Redistribution policy class.
    #[arg(long, value_enum, default_value = "public-p2p")]
    pub redistribution: RedistArg,
    #[arg(long)]
    pub revision: Option<String>,
    #[arg(long)]
    pub quant: Option<String>,
    /// Mark this model as gated (forces signed manifest, no public reseeding).
    #[arg(long)]
    pub gated: bool,
    /// An artifact mapping `install_path=local_file` (repeatable).
    #[arg(long = "artifact", value_name = "INSTALL=FILE")]
    pub artifacts: Vec<String>,
    /// A source `install_path:type:locator` (repeatable). Types: http, hf, lan,
    /// ipfs, file, iroh. hf locator is `repo@revision/path`.
    #[arg(long = "source", value_name = "PATH:TYPE:LOCATOR")]
    pub sources: Vec<String>,
    /// Merkle leaf size in bytes.
    #[arg(long, default_value_t = DEFAULT_LEAF_SIZE)]
    pub leaf_size: u64,
    /// Omit the chunk tree (full-file verification only).
    #[arg(long)]
    pub no_chunking: bool,
    /// Output path (default: stdout).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Sign with this key id after building (looked up in the OS keystore).
    #[arg(long)]
    pub sign: Option<String>,
    /// Provide the signing secret as hex directly (instead of the keystore).
    #[arg(long)]
    pub secret_hex: Option<String>,
}

pub fn build(args: BuildArgs) -> Result<()> {
    if args.artifacts.is_empty() {
        bail!("at least one --artifact INSTALL=FILE is required");
    }
    let mut artifacts = Vec::new();
    let mut classes: BTreeSet<SourceClass> = BTreeSet::new();

    for spec in &args.artifacts {
        let (install_path, file) = spec
            .split_once('=')
            .with_context(|| format!("bad --artifact spec `{spec}` (want INSTALL=FILE)"))?;
        noema_core::manifest::validate_artifact_path(install_path)
            .with_context(|| format!("unsafe install path `{install_path}`"))?;
        let file_path = PathBuf::from(file);
        let (hashes, size) =
            hash_file(&file_path).with_context(|| format!("hashing {}", file_path.display()))?;

        let chunking = if args.no_chunking {
            None
        } else {
            let tree = ChunkTree::from_file(&file_path, args.leaf_size)?;
            Some(Chunking {
                leaf_size: args.leaf_size,
                leaf_b3_merkle_root: tree.root_hex(),
            })
        };

        let sources = parse_sources_for(&args.sources, install_path)?;
        for s in &sources {
            classes.insert(s.class());
        }

        artifacts.push(Artifact {
            path: install_path.to_string(),
            role: infer_role(install_path),
            size_bytes: size,
            hashes,
            chunking,
            format: infer_format(install_path),
            sources,
        });
    }

    let manifest_id = compute_manifest_id(&args.name, &artifacts);
    let redistribution: RedistributionClass = args.redistribution.into();

    let mut manifest = Manifest {
        schema_version: SCHEMA_VERSION.to_string(),
        manifest_id,
        publisher: Publisher {
            id: args.publisher_id,
            display_name: None,
            public_keys: vec![],
        },
        model: Model {
            name: args.name,
            family: None,
            architecture: None,
            revision: args.revision,
            format: artifacts.first().and_then(|a| a.format.clone()),
            quantization: args.quant,
        },
        license: License {
            spdx: args.license,
            license_url: None,
            redistribution,
        },
        access: Access {
            gated: args.gated,
            require_signed_manifest: args.gated
                || matches!(
                    redistribution,
                    RedistributionClass::GatedNoRedistribution
                        | RedistributionClass::EnterprisePrivate
                ),
            allowed_source_classes: classes.into_iter().collect(),
        },
        artifacts,
        provenance: Some(noema_core::manifest::Provenance {
            origin: Some("noema-cli".into()),
            model_card_ref: None,
            note: None,
            malware_badges_observed: None,
            generated_at: Some(noema_core::util::now_rfc3339()),
        }),
        signatures: vec![],
    };

    if let Some(key_id) = &args.sign {
        let kp = load_keypair(key_id, args.secret_hex.as_deref())?;
        kp.sign_manifest(&mut manifest)?;
    }

    manifest
        .validate()
        .context("built manifest failed validation")?;
    let json = manifest.to_json_pretty()?;
    write_out(args.out.as_ref(), &json)?;
    Ok(())
}

#[derive(Args)]
pub struct SignArgs {
    /// Manifest file to sign in place (or read; use --out to redirect).
    #[arg(long)]
    pub r#in: PathBuf,
    /// Key id (looked up in the OS keystore) unless --secret-hex is given.
    #[arg(long)]
    pub key: Option<String>,
    #[arg(long)]
    pub secret_hex: Option<String>,
    /// Output path (default: overwrite input).
    #[arg(long)]
    pub out: Option<PathBuf>,
}

pub fn sign(args: SignArgs) -> Result<()> {
    let bytes = std::fs::read(&args.r#in)?;
    let mut manifest = Manifest::from_json(&bytes)?;
    let kp = match (&args.key, &args.secret_hex) {
        (_, Some(hex)) => KeyPair::from_secret_hex(hex)?,
        (Some(key_id), None) => load_keypair(key_id, None)?,
        (None, None) => bail!("provide --key <key_id> or --secret-hex"),
    };
    kp.sign_manifest(&mut manifest)?;
    manifest.validate()?;
    let json = manifest.to_json_pretty()?;
    let out = args.out.unwrap_or(args.r#in);
    std::fs::write(&out, json)?;
    println!("signed with {} -> {}", kp.key_id(), out.display());
    Ok(())
}

#[derive(Args)]
pub struct VerifyArgs {
    #[arg(long)]
    pub r#in: PathBuf,
    /// Trusted key ids; if given, require at least one valid trusted signature.
    #[arg(long = "trusted")]
    pub trusted: Vec<String>,
}

pub fn verify(args: VerifyArgs) -> Result<()> {
    let bytes = std::fs::read(&args.r#in)?;
    let manifest = Manifest::from_json(&bytes)?;
    manifest.validate().context("manifest structure invalid")?;
    let report = verify_manifest(&manifest)?;

    println!("manifest_id : {}", manifest.manifest_id);
    println!("model       : {}", manifest.model.name);
    println!("signatures  : {} total", report.total_signatures);
    for k in &report.valid_signatures {
        println!("  valid     : {k}");
    }
    for k in &report.invalid_signatures {
        println!("  INVALID   : {k}");
    }
    if !args.trusted.is_empty() {
        let trusted: std::collections::HashSet<String> = args.trusted.into_iter().collect();
        if report.is_trusted_by(&trusted) {
            println!("trust       : OK (signed by a trusted key)");
        } else {
            bail!("trust check failed: no valid signature from a trusted key");
        }
    } else if !report.is_signed() {
        println!("trust       : (unsigned or no valid signatures)");
    }
    Ok(())
}

#[derive(Args)]
pub struct ShowArgs {
    #[arg(long)]
    pub r#in: PathBuf,
}

pub fn show(args: ShowArgs) -> Result<()> {
    let bytes = std::fs::read(&args.r#in)?;
    let m = Manifest::from_json(&bytes)?;
    println!("Model:        {}", m.model.name);
    println!("Manifest id:  {}", m.manifest_id);
    println!("Publisher:    {}", m.publisher.id);
    println!(
        "License:      {} ({})",
        m.license.spdx,
        m.license.redistribution.as_str()
    );
    println!("Gated:        {}", m.access.gated);
    if let Some(rev) = &m.model.revision {
        println!("Revision:     {rev}");
    }
    println!("Artifacts:    {}", m.artifacts.len());
    for a in &m.artifacts {
        println!(
            "  - {} [{}] {} bytes  blake3:{}…",
            a.path,
            a.role,
            a.size_bytes,
            &a.hashes.blake3[..16.min(a.hashes.blake3.len())]
        );
        for s in &a.sources {
            println!("      source: {:?} -> {}", s.class(), s.source_id());
        }
    }
    Ok(())
}

fn parse_sources_for(specs: &[String], install_path: &str) -> Result<Vec<Source>> {
    let mut out = Vec::new();
    for spec in specs {
        let mut parts = spec.splitn(3, ':');
        let path = parts.next().unwrap_or("");
        let ty = parts.next().unwrap_or("");
        let locator = parts.next().unwrap_or("");
        if path != install_path {
            continue;
        }
        if ty.is_empty() || locator.is_empty() {
            bail!("bad --source spec `{spec}` (want PATH:TYPE:LOCATOR)");
        }
        out.push(make_source(ty, locator)?);
    }
    Ok(out)
}

fn make_source(ty: &str, locator: &str) -> Result<Source> {
    Ok(match ty {
        "http" | "https" => Source::HttpsMirror {
            url: locator.to_string(),
            auth: AuthPolicy::None,
        },
        // `lan` is intentionally not authorable: LAN peering was removed and the
        // engine refuses to fetch a LanPeer source, so minting one is a dead end.
        "ipfs" => Source::Ipfs {
            cid: locator.to_string(),
            retrieval: vec!["gateway".into()],
            auth: AuthPolicy::None,
        },
        "file" => Source::LocalFile {
            path: locator.to_string(),
        },
        "iroh" => Source::Iroh {
            blob_hash: locator.to_string(),
            tickets: vec![],
            auth: AuthPolicy::None,
        },
        "hf" => {
            // repo@revision/path
            let (repo, rest) = locator
                .split_once('@')
                .with_context(|| format!("hf locator `{locator}` must be repo@revision/path"))?;
            let (revision, path) = rest
                .split_once('/')
                .with_context(|| format!("hf locator `{locator}` missing /path"))?;
            Source::Huggingface {
                repo_id: repo.to_string(),
                revision: revision.to_string(),
                path: path.to_string(),
                auth: AuthPolicy::None,
            }
        }
        other => bail!("unknown source type `{other}`"),
    })
}

fn infer_format(path: &str) -> Option<String> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    match ext.as_str() {
        "gguf" => Some("gguf".into()),
        "safetensors" => Some("safetensors".into()),
        "json" => Some("json".into()),
        _ => None,
    }
}

fn infer_role(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".gguf") || lower.ends_with(".safetensors") {
        "weights".into()
    } else if lower.contains("tokenizer") {
        "tokenizer".into()
    } else if lower.ends_with("config.json") {
        "config".into()
    } else {
        "data".into()
    }
}

fn compute_manifest_id(name: &str, artifacts: &[Artifact]) -> String {
    let mut buf = Vec::new();
    buf.extend_from_slice(name.as_bytes());
    for a in artifacts {
        buf.extend_from_slice(a.hashes.blake3.as_bytes());
    }
    let h = noema_core::hash::hash_bytes(&buf);
    format!("mdl_b3_{}", &h.blake3[..24])
}

fn load_keypair(key_id: &str, secret_hex: Option<&str>) -> Result<KeyPair> {
    if let Some(hex) = secret_hex {
        return Ok(KeyPair::from_secret_hex(hex)?);
    }
    let store = noema_core::secret::default_store();
    let secret = store.get("signing", key_id)?.with_context(|| {
        format!("no secret for key `{key_id}` in keystore (run `noema keygen`?)")
    })?;
    Ok(KeyPair::from_secret_hex(&secret)?)
}

fn write_out(out: Option<&PathBuf>, json: &str) -> Result<()> {
    match out {
        Some(path) => {
            std::fs::write(path, json)?;
            println!("wrote {}", path.display());
        }
        None => println!("{json}"),
    }
    Ok(())
}
