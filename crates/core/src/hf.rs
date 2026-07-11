use crate::error::{Error, Result};
use crate::hash::Hashes;
use crate::manifest::{
    Access, Artifact, AuthPolicy, License, Manifest, Model, Provenance, Publisher,
    RedistributionClass, Source, SourceClass, SCHEMA_VERSION,
};
use serde::Deserialize;

/// The canonical Hub origin, used for `license_url` / `model_card_ref` in
/// synthesized manifests so content-addressed manifests stay stable across mirrors.
pub const HF_CANONICAL_ENDPOINT: &str = "https://huggingface.co";

/// Normalize a configured Hub endpoint (trim trailing slashes), falling back to
/// the canonical origin when empty.
fn normalize_endpoint(endpoint: &str) -> &str {
    let e = endpoint.trim().trim_end_matches('/');
    if e.is_empty() {
        HF_CANONICAL_ENDPOINT
    } else {
        e
    }
}

/// A model as it appears in search results.
#[derive(Debug, Clone)]
pub struct HfModel {
    pub id: String,
    pub downloads: u64,
    pub likes: u64,
    pub pipeline_tag: Option<String>,
    pub tags: Vec<String>,
    pub last_modified: Option<String>,
    pub gated: bool,
}

impl HfModel {
    /// The short, human name (the part after `org/`).
    pub fn name(&self) -> &str {
        self.id.rsplit('/').next().unwrap_or(&self.id)
    }
    pub fn author(&self) -> &str {
        self.id.split('/').next().unwrap_or("")
    }
    pub fn license(&self) -> Option<String> {
        license_from_tags(&self.tags)
    }
    pub fn has_gguf(&self) -> bool {
        self.tags.iter().any(|t| t == "gguf")
    }
    /// The weight/serialization formats this repo publishes (from Hub tags), as
    /// canonical ids in display order; deduped, empty when none recognized.
    pub fn model_formats(&self) -> Vec<&'static str> {
        // (Hub tag, canonical format id), in display-priority order; alias
        // spellings (jax/flax, paddle/paddlepaddle) fold to one id.
        const MAP: &[(&str, &str)] = &[
            ("gguf", "gguf"),
            ("mlx", "mlx"),
            ("safetensors", "safetensors"),
            ("onnx", "onnx"),
            ("coreml", "coreml"),
            ("pytorch", "pytorch"),
            ("ggml", "ggml"),
            ("tflite", "tflite"),
            ("keras", "keras"),
            ("flax", "flax"),
            ("jax", "flax"),
            ("paddlepaddle", "paddle"),
            ("paddle", "paddle"),
            ("tensorrt", "tensorrt"),
        ];
        let mut out: Vec<&'static str> = Vec::new();
        for (tag, fmt) in MAP {
            if !out.contains(fmt) && self.tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
                out.push(fmt);
            }
        }
        out
    }
    /// A few human-friendly capability tags for display (deduped, no `prefix:`).
    pub fn display_tags(&self) -> Vec<String> {
        self.tags
            .iter()
            .filter(|t| !t.contains(':') && t.len() < 22)
            .take(6)
            .cloned()
            .collect()
    }
}

/// A downloadable file within a model repo.
#[derive(Debug, Clone)]
pub struct HfFile {
    pub rfilename: String,
    pub size: u64,
    /// LFS sha256 (present for weight files); without it we can't verify.
    pub sha256: Option<String>,
    /// Git blob OID (`blobId`) for *non-LFS* files — the only digest the Hub
    /// publishes for small sidecars (config/tokenizer). `None` for LFS files,
    /// whose `blobId` is the pointer's sha1, not the content's.
    pub blob_id: Option<String>,
}

impl HfFile {
    pub fn format(&self) -> Option<String> {
        // Sidecar config/index files are JSON; otherwise defer to the shared detector.
        if self.rfilename.to_ascii_lowercase().ends_with(".json") {
            return Some("json".into());
        }
        crate::inspect::format_from_name(&self.rfilename)
    }
    /// A weight file worth surfacing as a download (and verifiable via sha256).
    pub fn is_downloadable_weight(&self) -> bool {
        let lower = self.rfilename.to_ascii_lowercase();
        self.sha256.is_some()
            && (lower.ends_with(".gguf")
                || lower.ends_with(".safetensors")
                || lower.ends_with(".bin"))
    }
    /// A self-contained GGUF quant (one file = one downloadable variant).
    pub fn is_gguf(&self) -> bool {
        self.rfilename.to_ascii_lowercase().ends_with(".gguf")
    }
    /// A safetensors weight shard (verifiable via sha256). Several of these,
    /// plus sidecars, make up one model — they aren't independent variants.
    pub fn is_safetensors_shard(&self) -> bool {
        self.sha256.is_some()
            && self
                .rfilename
                .to_ascii_lowercase()
                .ends_with(".safetensors")
    }
    /// A small companion file a multi-file model needs to load (config,
    /// tokenizer, the shard index, merges/vocab). Verifiable via its git blob
    /// OID. Excludes the weights themselves and repo cruft (README, .gitattributes,
    /// images), which we don't redistribute.
    pub fn is_model_sidecar(&self) -> bool {
        if self.blob_id.is_none() || self.is_downloadable_weight() {
            return false;
        }
        let lower = self.rfilename.to_ascii_lowercase();
        let base = lower.rsplit('/').next().unwrap_or(&lower);
        if base.starts_with('.') || base == "readme.md" || base.ends_with(".md") {
            return false;
        }
        let ext = base.rsplit('.').next().unwrap_or("");
        matches!(
            ext,
            "json" | "txt" | "model" | "vocab" | "merges" | "tiktoken" | "spm" | "jinja"
        )
    }
    /// A short quant/variant label derived from the filename, e.g. `Q4_K_M`.
    pub fn variant_label(&self) -> String {
        let stem = self
            .rfilename
            .rsplit('/')
            .next()
            .unwrap_or(&self.rfilename)
            .trim_end_matches(".gguf")
            .trim_end_matches(".safetensors");
        // Heuristic: the last `-`-separated token is usually the quant.
        stem.rsplit(['-', '.'])
            .next()
            .unwrap_or(stem)
            .to_uppercase()
    }
}

/// Full model detail: revision-pinned files + gating + license.
#[derive(Debug, Clone, Default)]
pub struct HfModelDetail {
    pub id: String,
    pub revision: String,
    pub gated: bool,
    pub license: Option<String>,
    /// Repo tags (e.g. `mlx`, `gguf`, `safetensors`) — used to label the bundle.
    pub tags: Vec<String>,
    pub files: Vec<HfFile>,
    pub downloads: u64,
    pub likes: u64,
    pub last_modified: Option<String>,
    /// Total parameter count, when the Hub reports it (safetensors or GGUF meta).
    pub params: Option<u64>,
    /// Context window in tokens, when the Hub reports it (GGUF metadata).
    pub context_length: Option<u64>,
    /// Model architecture (e.g. `llama`), from GGUF metadata or the repo config.
    pub architecture: Option<String>,
}

/// One GGUF quant, possibly split across shard files that download and install as one model.
#[derive(Debug, Clone)]
pub struct GgufQuant {
    pub label: String,
    /// The shard files in order. A standalone quant has exactly one.
    pub files: Vec<HfFile>,
}

impl GgufQuant {
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

/// Parse a split-GGUF filename `<base>-NNNNN-of-MMMMM.gguf`, returning the shared
/// base (the quant group key) and this shard's index. `None` for a normal file.
fn gguf_shard(rfilename: &str) -> Option<(String, u32)> {
    let stem = rfilename
        .strip_suffix(".gguf")
        .or_else(|| rfilename.strip_suffix(".GGUF"))?;
    let (left, total) = stem.rsplit_once("-of-")?;
    if total.is_empty() || !total.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (base, idx) = left.rsplit_once('-')?;
    if idx.is_empty() || !idx.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((base.to_string(), idx.parse().ok()?))
}

/// Derive a quant label (e.g. `Q4_K_M`) from a shard group's base stem.
fn quant_label_from_stem(stem: &str) -> String {
    stem.rsplit(['-', '.'])
        .next()
        .unwrap_or(stem)
        .to_uppercase()
}

impl HfModelDetail {
    pub fn name(&self) -> &str {
        self.id.rsplit('/').next().unwrap_or(&self.id)
    }
    /// Parameter count as a compact label ("7.6B", "356M"), when known.
    pub fn params_label(&self) -> Option<String> {
        let n = self.params?;
        Some(if n >= 1_000_000_000 {
            let b = n as f64 / 1e9;
            if b >= 100.0 {
                format!("{b:.0}B")
            } else {
                format!("{b:.1}B")
            }
        } else if n >= 1_000_000 {
            format!("{:.0}M", n as f64 / 1e6)
        } else {
            n.to_string()
        })
    }
    /// Context window as a compact label ("128K", "32K"), when known.
    pub fn context_label(&self) -> Option<String> {
        let n = self.context_length?;
        Some(if n >= 1024 {
            format!("{}K", n / 1024)
        } else {
            n.to_string()
        })
    }
    /// Downloadable weight files, largest-first is rarely useful; keep API order.
    pub fn weight_files(&self) -> Vec<&HfFile> {
        self.files
            .iter()
            .filter(|f| f.is_downloadable_weight())
            .collect()
    }
    /// GGUF weight files — each is a self-contained quant the user picks among.
    pub fn gguf_files(&self) -> Vec<&HfFile> {
        self.files.iter().filter(|f| f.is_gguf()).collect()
    }
    /// The safetensors weight shards (one or many) that form a single model.
    pub fn safetensors_shards(&self) -> Vec<&HfFile> {
        self.files
            .iter()
            .filter(|f| f.is_safetensors_shard())
            .collect()
    }
    /// The companion files (config/tokenizer/index) a safetensors/MLX model needs.
    pub fn model_sidecars(&self) -> Vec<&HfFile> {
        self.files.iter().filter(|f| f.is_model_sidecar()).collect()
    }
    /// Whether this repo holds a safetensors/MLX model that should be offered as
    /// one bundled download rather than per-file (there are no quants to choose).
    pub fn has_safetensors_bundle(&self) -> bool {
        !self.safetensors_shards().is_empty()
    }
    /// Whether this is an MLX repo (Apple-silicon weights), by tag or org.
    pub fn is_mlx(&self) -> bool {
        self.tags.iter().any(|t| t.eq_ignore_ascii_case("mlx"))
            || self.id.to_ascii_lowercase().contains("mlx")
    }
    /// The format string for the bundle's manifest (`mlx` vs plain `safetensors`).
    pub fn bundle_format(&self) -> &'static str {
        if self.is_mlx() {
            "mlx"
        } else {
            "safetensors"
        }
    }
    /// A human label for the one-click bundle, e.g. `MLX · 4-bit` or `Safetensors`.
    pub fn bundle_variant_label(&self) -> String {
        let base = if self.is_mlx() { "MLX" } else { "Safetensors" };
        // Surface a quant hint when the repo name encodes one (MLX repos do:
        // `…-4bit`, `…-8bit`, `…-bf16`). Pure-safetensors models have none.
        let lower = self.id.to_ascii_lowercase();
        let bits = ["4bit", "8bit", "6bit", "3bit", "2bit", "bf16", "fp16"]
            .into_iter()
            .find(|b| lower.contains(b));
        match bits {
            Some(b) => format!("{base} · {}", b.replace("bit", "-bit")),
            None => base.to_string(),
        }
    }
    /// Total bytes of the bundle (all shards + sidecars) — what the user downloads.
    pub fn bundle_total_size(&self) -> u64 {
        self.safetensors_shards()
            .iter()
            .chain(self.model_sidecars().iter())
            .map(|f| f.size)
            .sum()
    }

    /// Group this repo's GGUF files into one entry per quant, folding a split
    /// quant's shards (`…-00001-of-00009.gguf`) into a single [`GgufQuant`].
    /// Standalone quants are groups of one. Order follows first appearance.
    pub fn gguf_quants(&self) -> Vec<GgufQuant> {
        let mut order: Vec<String> = Vec::new();
        let mut groups: std::collections::HashMap<String, Vec<HfFile>> =
            std::collections::HashMap::new();
        for f in self.files.iter().filter(|f| f.is_gguf()) {
            let key = match gguf_shard(&f.rfilename) {
                Some((base, _)) => base,
                None => f.rfilename.clone(),
            };
            if !groups.contains_key(&key) {
                order.push(key.clone());
            }
            groups.entry(key).or_default().push(f.clone());
        }
        order
            .into_iter()
            .filter_map(|key| {
                let mut files = groups.remove(&key)?;
                files.sort_by_key(|f| gguf_shard(&f.rfilename).map(|(_, i)| i).unwrap_or(0));
                let label = if files.len() > 1 {
                    quant_label_from_stem(&key)
                } else {
                    files[0].variant_label()
                };
                Some(GgufQuant { label, files })
            })
            .collect()
    }
}

#[derive(Deserialize)]
struct RawModel {
    id: String,
    #[serde(default)]
    downloads: u64,
    #[serde(default)]
    likes: u64,
    #[serde(default)]
    pipeline_tag: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(rename = "lastModified", default)]
    last_modified: Option<String>,
    #[serde(default)]
    gated: serde_json::Value,
}

#[derive(Deserialize)]
struct RawDetail {
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    gated: serde_json::Value,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    siblings: Vec<RawSibling>,
    #[serde(default)]
    downloads: u64,
    #[serde(default)]
    likes: u64,
    #[serde(rename = "lastModified", default)]
    last_modified: Option<String>,
    #[serde(default)]
    config: Option<RawConfig>,
    #[serde(default)]
    safetensors: Option<RawParamCount>,
    #[serde(default)]
    gguf: Option<RawGgufMeta>,
}

#[derive(Deserialize)]
struct RawConfig {
    #[serde(default)]
    architectures: Vec<String>,
    #[serde(default)]
    model_type: Option<String>,
}

#[derive(Deserialize)]
struct RawParamCount {
    #[serde(default)]
    total: Option<u64>,
}

#[derive(Deserialize)]
struct RawGgufMeta {
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    architecture: Option<String>,
    #[serde(default)]
    context_length: Option<u64>,
}

#[derive(Deserialize)]
struct RawSibling {
    rfilename: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(rename = "blobId", default)]
    blob_id: Option<String>,
    #[serde(default)]
    lfs: Option<RawLfs>,
}

#[derive(Deserialize)]
struct RawLfs {
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

/// `gated` is `false` (bool) or `"auto"`/`"manual"` (string) in the API.
fn parse_gated(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::String(s) => s != "false",
        _ => false,
    }
}

fn license_from_tags(tags: &[String]) -> Option<String> {
    tags.iter()
        .find_map(|t| t.strip_prefix("license:").map(|s| s.to_string()))
}

/// Classify an HF license tag into a redistribution policy, via the shared
/// [`RedistributionClass::for_license`].
fn redistribution_for(license: &Option<String>) -> RedistributionClass {
    RedistributionClass::for_license(license.as_deref())
}

async fn get_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    query: &[(&str, &str)],
    token: Option<&str>,
) -> Result<T> {
    let mut req = client.get(url);
    if !query.is_empty() {
        req = req.query(query);
    }
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| Error::other(format!("hugging face request: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::other(format!(
            "hugging face returned {}",
            resp.status()
        )));
    }
    // Metadata JSON only (file content streams elsewhere). Bound it generously
    // (32 MiB) so a hostile mirror can't stream an unbounded body.
    let bytes = crate::util::read_body_capped(resp, 32 * 1024 * 1024).await?;
    serde_json::from_slice(&bytes).map_err(Error::from)
}

/// How to order a Hub model listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelSort {
    /// The Hub's "hot right now" ranking.
    #[default]
    Trending,
    Downloads,
    Likes,
    Updated,
}

impl ModelSort {
    fn api_key(self) -> &'static str {
        match self {
            ModelSort::Trending => "trendingScore",
            ModelSort::Downloads => "downloads",
            ModelSort::Likes => "likes",
            ModelSort::Updated => "lastModified",
        }
    }
}

/// Parameters for one page of a Hub model listing (search / browse / drill-down).
#[derive(Debug, Clone, Default)]
pub struct ModelListQuery {
    pub search: Option<String>,
    /// Restrict to one publisher/org (the Hub `author` param).
    pub author: Option<String>,
    /// Tag filters, ANDed by the Hub — e.g. `gguf`, `license:apache-2.0`,
    /// `base_model:quantized:org/name`.
    pub filters: Vec<String>,
    pub sort: ModelSort,
    pub limit: usize,
}

/// One page of a model listing plus the cursor URL of the next page.
#[derive(Debug, Clone)]
pub struct ModelPage {
    pub models: Vec<HfModel>,
    /// Absolute URL of the next page (from the Hub's `Link: rel="next"` header),
    /// consumed by [`list_models_page`]. `None` = last page.
    pub next: Option<String>,
}

fn raw_to_model(m: RawModel) -> HfModel {
    HfModel {
        id: m.id,
        downloads: m.downloads,
        likes: m.likes,
        pipeline_tag: m.pipeline_tag,
        gated: parse_gated(&m.gated),
        last_modified: m.last_modified,
        tags: m.tags,
    }
}

/// The `rel="next"` target of an RFC-8288 `Link` header, if any.
fn next_link(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let link = headers.get(reqwest::header::LINK)?.to_str().ok()?;
    for part in link.split(',') {
        let part = part.trim();
        if part.contains("rel=\"next\"") {
            let url = part.split('>').next()?.trim().strip_prefix('<')?;
            return Some(url.to_string());
        }
    }
    None
}

async fn get_models_page(
    client: &reqwest::Client,
    url: &str,
    query: &[(&str, &str)],
    token: Option<&str>,
    endpoint: &str,
) -> Result<ModelPage> {
    let mut req = client.get(url);
    if !query.is_empty() {
        req = req.query(query);
    }
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| Error::other(format!("hugging face request: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::other(format!(
            "hugging face returned {}",
            resp.status()
        )));
    }
    // Only follow pagination back to the origin we queried — a hostile mirror
    // must not be able to point the next page anywhere else.
    let next = next_link(resp.headers()).filter(|n| n.starts_with(normalize_endpoint(endpoint)));
    let bytes = crate::util::read_body_capped(resp, 32 * 1024 * 1024).await?;
    let raw: Vec<RawModel> = serde_json::from_slice(&bytes).map_err(Error::from)?;
    Ok(ModelPage {
        models: raw.into_iter().map(raw_to_model).collect(),
        next,
    })
}

/// List Hub models: search, author drill-down, tag facets, sort, pagination.
///
/// `endpoint` is the Hub origin to query (real Hub or an HF mirror).
pub async fn list_models(
    client: &reqwest::Client,
    endpoint: &str,
    query: &ModelListQuery,
    token: Option<&str>,
) -> Result<ModelPage> {
    let url = format!("{}/api/models", normalize_endpoint(endpoint));
    let limit_s = query.limit.max(1).to_string();
    let mut params: Vec<(&str, &str)> = vec![
        ("limit", &limit_s),
        ("full", "true"),
        ("config", "false"),
        ("sort", query.sort.api_key()),
        ("direction", "-1"),
    ];
    if let Some(s) = query.search.as_deref().filter(|s| !s.trim().is_empty()) {
        params.push(("search", s));
    }
    if let Some(a) = query.author.as_deref().filter(|a| !a.trim().is_empty()) {
        params.push(("author", a));
    }
    for f in &query.filters {
        params.push(("filter", f));
    }
    get_models_page(client, &url, &params, token, endpoint).await
}

/// Fetch the next page of a listing, using the cursor URL from a prior
/// [`ModelPage::next`].
pub async fn list_models_page(
    client: &reqwest::Client,
    next_url: &str,
    token: Option<&str>,
) -> Result<ModelPage> {
    get_models_page(client, next_url, &[], token, next_url).await
}

/// GGUF/MLX/other conversions of a base repo, via the Hub's model-tree tags
/// (`base_model:quantized:<repo>`), most-downloaded first.
pub async fn model_conversions(
    client: &reqwest::Client,
    endpoint: &str,
    base_id: &str,
    limit: usize,
    token: Option<&str>,
) -> Result<Vec<HfModel>> {
    let q = ModelListQuery {
        filters: vec![format!("base_model:quantized:{base_id}")],
        sort: ModelSort::Downloads,
        limit,
        ..Default::default()
    };
    Ok(list_models(client, endpoint, &q, token).await?.models)
}

/// Search the Hub for models matching `query` (Hub relevance order).
pub async fn search_models(
    client: &reqwest::Client,
    endpoint: &str,
    query: &str,
    limit: usize,
    token: Option<&str>,
) -> Result<Vec<HfModel>> {
    // Hub relevance ranking only applies without an explicit sort key, so this
    // bypasses list_models' always-sorted form.
    let url = format!("{}/api/models", normalize_endpoint(endpoint));
    let limit_s = limit.to_string();
    let raw: Vec<RawModel> = get_json(
        client,
        &url,
        &[
            ("search", query),
            ("limit", &limit_s),
            ("full", "true"),
            ("config", "false"),
        ],
        token,
    )
    .await?;
    Ok(raw.into_iter().map(raw_to_model).collect())
}

/// The most-downloaded models on the Hub (no search filter).
pub async fn popular_models(
    client: &reqwest::Client,
    endpoint: &str,
    limit: usize,
    token: Option<&str>,
) -> Result<Vec<HfModel>> {
    let q = ModelListQuery {
        sort: ModelSort::Downloads,
        limit,
        ..Default::default()
    };
    Ok(list_models(client, endpoint, &q, token).await?.models)
}

/// Coarse quality tier for a GGUF quant label (static community wisdom).
/// `None` for unrecognized labels.
pub fn quant_quality_tier(label: &str) -> Option<(&'static str, &'static str)> {
    let l = label.trim().to_ascii_uppercase();
    let starts = |p: &str| l.starts_with(p);
    Some(
        if starts("F32") || starts("F16") || starts("BF16") || starts("FP16") || starts("FP32") {
            (
                "full precision",
                "Original unquantized weights — biggest files, reference quality",
            )
        } else if starts("Q8") {
            (
                "near-lossless",
                "Practically indistinguishable from the original",
            )
        } else if starts("Q6") {
            ("near-lossless", "Very close to the original")
        } else if starts("Q5") {
            (
                "balanced+",
                "A notch above the Q4 sweet spot, for a little more size",
            )
        } else if starts("Q4") || starts("IQ4") {
            ("balanced", "The community sweet spot — good quality per GB")
        } else if starts("Q3") || starts("IQ3") {
            ("compact", "Noticeable quality loss — for tight memory")
        } else if starts("Q2") || starts("IQ2") || starts("IQ1") || starts("Q1") || starts("TQ") {
            ("smallest", "Visibly degraded — only when nothing else fits")
        } else {
            return None;
        },
    )
}

/// Fetch a model's revision-pinned file list + gating + license.
///
/// `endpoint` is the Hub origin to query (real Hub or an HF mirror).
pub async fn model_detail(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
    token: Option<&str>,
) -> Result<HfModelDetail> {
    let url = format!("{}/api/models/{id}", normalize_endpoint(endpoint));
    let raw: RawDetail = get_json(client, &url, &[("blobs", "true")], token).await?;
    let files = raw
        .siblings
        .into_iter()
        .map(|s| {
            let sha256 = s.lfs.as_ref().and_then(|l| l.sha256.clone());
            let size = s
                .size
                .or_else(|| s.lfs.as_ref().and_then(|l| l.size))
                .unwrap_or(0);
            // For LFS files `blobId` is the pointer's sha1 (not the content's),
            // so only keep it for plain (non-LFS) files, where it's the git OID
            // of the actual bytes and thus verifiable.
            let blob_id = if s.lfs.is_none() { s.blob_id } else { None };
            HfFile {
                rfilename: s.rfilename,
                size,
                sha256,
                blob_id,
            }
        })
        .collect();
    let params = raw
        .safetensors
        .as_ref()
        .and_then(|s| s.total)
        .or_else(|| raw.gguf.as_ref().and_then(|g| g.total));
    let architecture = raw
        .gguf
        .as_ref()
        .and_then(|g| g.architecture.clone())
        .or_else(|| {
            raw.config.as_ref().and_then(|c| {
                c.architectures
                    .first()
                    .cloned()
                    .or_else(|| c.model_type.clone())
            })
        });
    Ok(HfModelDetail {
        id: id.to_string(),
        revision: raw.sha.unwrap_or_else(|| "main".to_string()),
        gated: parse_gated(&raw.gated),
        license: license_from_tags(&raw.tags),
        tags: raw.tags,
        files,
        downloads: raw.downloads,
        likes: raw.likes,
        last_modified: raw.last_modified,
        params,
        context_length: raw.gguf.as_ref().and_then(|g| g.context_length),
        architecture,
    })
}

/// Fetch a model's raw README.md (the model card body) pinned to `revision`.
/// Returns `Ok(None)` when the repo has no README.
pub async fn model_readme(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
    revision: &str,
    token: Option<&str>,
) -> Result<Option<String>> {
    let url = format!(
        "{}/{id}/resolve/{revision}/README.md",
        normalize_endpoint(endpoint)
    );
    let mut req = client.get(&url);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| Error::other(format!("hugging face request: {e}")))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(Error::other(format!(
            "hugging face returned {}",
            resp.status()
        )));
    }
    // Model cards are text; bound the body so a hostile mirror can't stream an
    // unbounded one.
    let bytes = crate::util::read_body_capped(resp, 4 * 1024 * 1024).await?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Synthesize a (sha256-backed, unsigned) manifest for one file of an HF model.
/// The blake3 is left unknown — the engine computes and records it on download.
pub fn manifest_for(detail: &HfModelDetail, file: &HfFile) -> Result<Manifest> {
    let sha256 = file
        .sha256
        .clone()
        .ok_or_else(|| Error::other("file has no sha256 (not an LFS object); cannot verify"))?;

    let mut hasher = blake3::Hasher::new();
    hasher.update(detail.id.as_bytes());
    hasher.update(b"/");
    hasher.update(file.rfilename.as_bytes());
    let manifest_id = format!(
        "mdl_hf_{}",
        hex::encode(&hasher.finalize().as_bytes()[..12])
    );

    let license = detail.license.clone();
    let redistribution = redistribution_for(&license);
    let auth = if detail.gated {
        AuthPolicy::Token
    } else {
        AuthPolicy::None
    };

    let variant = file.variant_label();
    let model_name = format!("{} · {}", detail.name(), variant);

    let manifest = Manifest {
        schema_version: SCHEMA_VERSION.to_string(),
        manifest_id,
        publisher: Publisher {
            id: format!("hf:{}", detail.id),
            display_name: Some(detail.id.split('/').next().unwrap_or("").to_string()),
            public_keys: vec![],
        },
        model: Model {
            name: model_name,
            family: None,
            architecture: None,
            revision: Some(format!("hf:commit:{}", detail.revision)),
            format: file.format(),
            quantization: Some(variant),
        },
        license: License {
            spdx: license.unwrap_or_else(|| "unknown".to_string()),
            license_url: Some(format!("{HF_CANONICAL_ENDPOINT}/{}", detail.id)),
            redistribution,
        },
        access: Access {
            // Gating is enforced via the source's auth requirement (an HF token),
            // not via a third-party signature — so this stays non-gated here.
            gated: false,
            require_signed_manifest: false,
            // Allow every transport so a stalled route can fail over to any other
            // that has the bytes (all content-verified). HF is allowed here but
            // still gated at fetch time by the per-platform opt-in.
            allowed_source_classes: vec![
                SourceClass::Huggingface,
                SourceClass::HttpsMirror,
                SourceClass::Iroh,
            ],
        },
        artifacts: vec![Artifact {
            path: sanitize_install_name(&file.rfilename),
            role: "weights".to_string(),
            size_bytes: file.size,
            hashes: Hashes::sha256_only(sha256),
            chunking: None,
            format: file.format(),
            sources: vec![Source::Huggingface {
                repo_id: detail.id.clone(),
                revision: detail.revision.clone(),
                path: file.rfilename.clone(),
                auth,
            }],
        }],
        provenance: Some(Provenance {
            origin: Some("huggingface".to_string()),
            model_card_ref: Some(format!("hf:{}", detail.id)),
            note: None,
            malware_badges_observed: None,
            generated_at: Some(crate::util::now_rfc3339()),
        }),
        signatures: vec![],
    };
    Ok(manifest)
}

/// Synthesize one manifest for a GGUF quant, possibly split across shard files
/// (`…-00001-of-00009.gguf`) each verified by its LFS sha256, that download and
/// install as one model. Single-file quants route through here too.
pub fn manifest_for_gguf_quant(detail: &HfModelDetail, files: &[HfFile]) -> Result<Manifest> {
    if files.is_empty() {
        return Err(Error::other("no GGUF files in this quant"));
    }

    let mut hasher = blake3::Hasher::new();
    hasher.update(detail.id.as_bytes());
    hasher.update(b"@");
    hasher.update(detail.revision.as_bytes());
    hasher.update(b"#gguf");
    for f in files {
        hasher.update(b"/");
        hasher.update(f.rfilename.as_bytes());
    }
    let manifest_id = format!(
        "mdl_hf_{}",
        hex::encode(&hasher.finalize().as_bytes()[..12])
    );

    let license = detail.license.clone();
    let redistribution = redistribution_for(&license);
    let auth = if detail.gated {
        AuthPolicy::Token
    } else {
        AuthPolicy::None
    };

    let label = match gguf_shard(&files[0].rfilename) {
        Some((base, _)) if files.len() > 1 => quant_label_from_stem(&base),
        _ => files[0].variant_label(),
    };
    let model_name = format!("{} · {}", detail.name(), label);

    let mut artifacts = Vec::with_capacity(files.len());
    for f in files {
        let sha256 = f
            .sha256
            .clone()
            .ok_or_else(|| Error::other(format!("GGUF shard `{}` has no sha256", f.rfilename)))?;
        artifacts.push(Artifact {
            path: sanitize_install_name(&f.rfilename),
            role: "weights".to_string(),
            size_bytes: f.size,
            hashes: Hashes::sha256_only(sha256),
            chunking: None,
            format: Some("gguf".to_string()),
            sources: vec![Source::Huggingface {
                repo_id: detail.id.clone(),
                revision: detail.revision.clone(),
                path: f.rfilename.clone(),
                auth,
            }],
        });
    }

    Ok(Manifest {
        schema_version: SCHEMA_VERSION.to_string(),
        manifest_id,
        publisher: Publisher {
            id: format!("hf:{}", detail.id),
            display_name: Some(detail.id.split('/').next().unwrap_or("").to_string()),
            public_keys: vec![],
        },
        model: Model {
            name: model_name,
            family: None,
            architecture: None,
            revision: Some(format!("hf:commit:{}", detail.revision)),
            format: Some("gguf".to_string()),
            quantization: Some(label),
        },
        license: License {
            spdx: license.unwrap_or_else(|| "unknown".to_string()),
            license_url: Some(format!("{HF_CANONICAL_ENDPOINT}/{}", detail.id)),
            redistribution,
        },
        access: Access {
            gated: false,
            require_signed_manifest: false,
            allowed_source_classes: vec![
                SourceClass::Huggingface,
                SourceClass::HttpsMirror,
                SourceClass::Iroh,
            ],
        },
        artifacts,
        provenance: Some(Provenance {
            origin: Some("huggingface".to_string()),
            model_card_ref: Some(format!("hf:{}", detail.id)),
            note: None,
            malware_badges_observed: None,
            generated_at: Some(crate::util::now_rfc3339()),
        }),
        signatures: vec![],
    })
}

/// Synthesize one manifest covering an entire sharded safetensors/MLX model:
/// every weight shard (verified by its LFS sha256) plus the companion files
/// (config, tokenizer, shard index) needed to load it, verified by the git blob
/// OID the Hub publishes for those non-LFS files.
pub fn manifest_for_bundle(detail: &HfModelDetail) -> Result<Manifest> {
    let shards = detail.safetensors_shards();
    if shards.is_empty() {
        return Err(Error::other(
            "no safetensors weight shards to bundle in this repo",
        ));
    }
    let sidecars = detail.model_sidecars();

    // Stable id over the repo, revision, and the set of files in the bundle.
    let mut hasher = blake3::Hasher::new();
    hasher.update(detail.id.as_bytes());
    hasher.update(b"@");
    hasher.update(detail.revision.as_bytes());
    hasher.update(b"#bundle");
    for f in shards.iter().chain(sidecars.iter()) {
        hasher.update(b"/");
        hasher.update(f.rfilename.as_bytes());
    }
    let manifest_id = format!(
        "mdl_hf_{}",
        hex::encode(&hasher.finalize().as_bytes()[..12])
    );

    let license = detail.license.clone();
    let redistribution = redistribution_for(&license);
    let auth = if detail.gated {
        AuthPolicy::Token
    } else {
        AuthPolicy::None
    };
    let bundle_format = detail.bundle_format();
    let model_name = format!("{} · {}", detail.name(), detail.bundle_variant_label());

    let make_source = |path: &str| Source::Huggingface {
        repo_id: detail.id.clone(),
        revision: detail.revision.clone(),
        path: path.to_string(),
        auth,
    };

    let mut artifacts = Vec::with_capacity(shards.len() + sidecars.len());
    for f in &shards {
        let sha256 = f
            .sha256
            .clone()
            .ok_or_else(|| Error::other(format!("weight shard `{}` has no sha256", f.rfilename)))?;
        artifacts.push(Artifact {
            path: sanitize_install_name(&f.rfilename),
            role: "weights".to_string(),
            size_bytes: f.size,
            hashes: Hashes::sha256_only(sha256),
            chunking: None,
            // The shards are safetensors files even in an MLX repo, so the
            // safetensors header check still applies.
            format: Some("safetensors".to_string()),
            sources: vec![make_source(&f.rfilename)],
        });
    }
    for f in &sidecars {
        // Skip anything we can't verify or size (keeps the "every byte verified"
        // guarantee — a sidecar without a published OID is simply left out).
        let (Some(blob_id), true) = (f.blob_id.clone(), f.size > 0) else {
            continue;
        };
        artifacts.push(Artifact {
            path: sanitize_install_name(&f.rfilename),
            role: sidecar_role(&f.rfilename),
            size_bytes: f.size,
            hashes: Hashes::git_blob_sha1_only(blob_id),
            chunking: None,
            format: f.format(),
            sources: vec![make_source(&f.rfilename)],
        });
    }

    let manifest = Manifest {
        schema_version: SCHEMA_VERSION.to_string(),
        manifest_id,
        publisher: Publisher {
            id: format!("hf:{}", detail.id),
            display_name: Some(detail.id.split('/').next().unwrap_or("").to_string()),
            public_keys: vec![],
        },
        model: Model {
            name: model_name,
            family: None,
            architecture: None,
            revision: Some(format!("hf:commit:{}", detail.revision)),
            format: Some(bundle_format.to_string()),
            // Safetensors models have no GGUF-style quant to choose among; MLX
            // bakes its scheme into the weights, so leave this unset here.
            quantization: None,
        },
        license: License {
            spdx: license.unwrap_or_else(|| "unknown".to_string()),
            license_url: Some(format!("{HF_CANONICAL_ENDPOINT}/{}", detail.id)),
            redistribution,
        },
        access: Access {
            gated: false,
            require_signed_manifest: false,
            // Allow every transport so a stalled route can fail over to any other
            // that has the bytes (all content-verified). HF is allowed here but
            // still gated at fetch time by the per-platform opt-in.
            allowed_source_classes: vec![
                SourceClass::Huggingface,
                SourceClass::HttpsMirror,
                SourceClass::Iroh,
            ],
        },
        artifacts,
        provenance: Some(Provenance {
            origin: Some("huggingface".to_string()),
            model_card_ref: Some(format!("hf:{}", detail.id)),
            note: None,
            malware_badges_observed: None,
            generated_at: Some(crate::util::now_rfc3339()),
        }),
        signatures: vec![],
    };
    Ok(manifest)
}

/// Semantic role for a sidecar file, for display + install bookkeeping.
fn sidecar_role(rfilename: &str) -> String {
    let lower = rfilename.to_ascii_lowercase();
    if lower.contains("tokenizer") || lower.ends_with("merges.txt") || lower.ends_with(".model") {
        "tokenizer".into()
    } else if lower.ends_with("config.json") || lower.contains("config") {
        "config".into()
    } else if lower.ends_with(".index.json") {
        "index".into()
    } else {
        "data".into()
    }
}

/// Flatten a repo-relative path to a safe single install filename.
fn sanitize_install_name(rfilename: &str) -> String {
    rfilename
        .rsplit('/')
        .next()
        .unwrap_or(rfilename)
        .to_string()
}

/// Derive a Hub search query from a local model filename, dropping the quant
/// suffix so the *repo* surfaces (we then confirm the match by sha256).
/// e.g. `qwen2.5-0.5b-instruct-q4_k_m.gguf` -> `qwen2.5 0.5b instruct gguf`.
pub fn query_from_filename(filename: &str) -> String {
    let base = filename.rsplit('/').next().unwrap_or(filename);
    let is_st = base.to_ascii_lowercase().ends_with(".safetensors");
    let stem = base
        .trim_end_matches(".gguf")
        .trim_end_matches(".safetensors")
        .trim_end_matches(".bin");
    let mut tokens: Vec<&str> = stem
        .split(['-', '_', '.'])
        .filter(|t| !t.is_empty())
        .collect();
    while let Some(last) = tokens.last() {
        if is_quant_token(last) {
            tokens.pop();
        } else {
            break;
        }
    }
    let mut q = tokens.join(" ");
    if !q.is_empty() {
        q.push(' ');
    }
    q.push_str(if is_st { "safetensors" } else { "gguf" });
    q.trim().to_string()
}

fn is_quant_token(t: &str) -> bool {
    let t = t.to_ascii_lowercase();
    matches!(
        t.as_str(),
        "k" | "m" | "s" | "l" | "xl" | "xs" | "0" | "1" | "f16" | "bf16" | "fp16" | "f32" | "imat"
    ) || (t.starts_with('q') && t.len() <= 3 && t[1..].chars().all(|c| c.is_ascii_digit()))
        || (t.starts_with("iq") && t.len() <= 4)
}

/// A recommended GGUF quant for a given memory budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuantPick {
    /// The chosen file's `rfilename`.
    pub rfilename: String,
    /// Whether it's expected to fit the budget with runtime headroom. `false`
    /// means it's a best-effort pick (nothing fit, or the budget was unknown).
    pub fits: bool,
    /// Short human rationale, e.g. `fits your ~24 GB` or `largest that may run`.
    pub reason: String,
}

/// Runtime memory ≈ weights + KV cache + activations, so budget a quant's file
/// size against this headroom factor before calling it a fit.
const RUNTIME_HEADROOM: f64 = 1.2;

/// Recommend the best GGUF quant among `files` for `budget_bytes` of available
/// memory (system RAM or GPU VRAM — see [`crate::platform::detect_memory_budget_bytes`]).
/// Picks the **largest** quant (highest quality) whose estimated runtime
/// footprint fits the budget; if none fits, returns the smallest (most likely to
/// run) flagged `fits = false`. `budget_bytes == 0` (unknown) falls back to a
/// balanced default — Q4_K_M if present, else the median by size — with no fit
/// claim. Returns `None` only when there are no candidate files at all.
pub fn recommend_quant_for_budget(files: &[&HfFile], budget_bytes: u64) -> Option<QuantPick> {
    let mut ggufs: Vec<&HfFile> = files
        .iter()
        .copied()
        .filter(|f| f.is_gguf() && f.size > 0)
        .collect();
    if ggufs.is_empty() {
        // No sized GGUF quants to choose among — keep the prior behavior of
        // surfacing whatever the first file is, with no fit claim.
        return files.first().map(|f| QuantPick {
            rfilename: f.rfilename.clone(),
            fits: false,
            reason: String::new(),
        });
    }
    ggufs.sort_by_key(|f| f.size);

    if budget_bytes == 0 {
        let pick = ggufs
            .iter()
            .find(|f| f.rfilename.to_lowercase().contains("q4_k_m"))
            .copied()
            .unwrap_or(ggufs[ggufs.len() / 2]);
        return Some(QuantPick {
            rfilename: pick.rfilename.clone(),
            fits: false,
            reason: "balanced default".into(),
        });
    }

    let budget = budget_bytes as f64;
    if let Some(f) = ggufs
        .iter()
        .rev()
        .find(|f| (f.size as f64) * RUNTIME_HEADROOM <= budget)
        .copied()
    {
        Some(QuantPick {
            rfilename: f.rfilename.clone(),
            fits: true,
            reason: format!("fits your ~{}", human_gib(budget_bytes)),
        })
    } else {
        Some(QuantPick {
            rfilename: ggufs[0].rfilename.clone(),
            fits: false,
            reason: format!("largest that may run on ~{}", human_gib(budget_bytes)),
        })
    }
}

fn human_gib(bytes: u64) -> String {
    format!("{:.0} GB", bytes as f64 / 1_000_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_link_parses_hub_pagination() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            reqwest::header::LINK,
            "<https://huggingface.co/api/models?cursor=abc&limit=30>; rel=\"next\""
                .parse()
                .unwrap(),
        );
        assert_eq!(
            next_link(&h).as_deref(),
            Some("https://huggingface.co/api/models?cursor=abc&limit=30")
        );
        let empty = reqwest::header::HeaderMap::new();
        assert!(next_link(&empty).is_none());
    }

    #[test]
    fn quality_tiers_cover_common_quants() {
        assert_eq!(quant_quality_tier("Q4_K_M").unwrap().0, "balanced");
        assert_eq!(quant_quality_tier("IQ2_XS").unwrap().0, "smallest");
        assert_eq!(quant_quality_tier("Q8_0").unwrap().0, "near-lossless");
        assert_eq!(quant_quality_tier("F16").unwrap().0, "full precision");
        assert_eq!(quant_quality_tier("q6_k").unwrap().0, "near-lossless");
        assert!(quant_quality_tier("WEIRD").is_none());
    }

    #[test]
    fn params_and_context_labels() {
        let d = HfModelDetail {
            params: Some(7_615_000_000),
            context_length: Some(131_072),
            ..Default::default()
        };
        assert_eq!(d.params_label().as_deref(), Some("7.6B"));
        assert_eq!(d.context_label().as_deref(), Some("128K"));
    }

    #[test]
    fn parses_gated_variants() {
        assert!(parse_gated(&serde_json::json!("auto")));
        assert!(parse_gated(&serde_json::json!("manual")));
        assert!(!parse_gated(&serde_json::json!(false)));
        assert!(!parse_gated(&serde_json::json!("false")));
    }

    fn hf_model(tags: &[&str]) -> HfModel {
        HfModel {
            id: "org/Model".into(),
            downloads: 0,
            likes: 0,
            pipeline_tag: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            last_modified: None,
            gated: false,
        }
    }

    #[test]
    fn model_formats_from_tags() {
        // Multiple format tags surface as canonical ids in display order,
        // independent of tag order in the API payload.
        assert_eq!(
            hf_model(&["safetensors", "transformers", "gguf", "region:us"]).model_formats(),
            vec!["gguf", "safetensors"],
        );
        // The real Hub library tags map to their canonical ids, case-insensitively.
        assert_eq!(
            hf_model(&["MLX", "CoreML", "flax"]).model_formats(),
            vec!["mlx", "coreml", "flax"],
        );
        // Alias spellings fold to the same id (jax -> flax, paddlepaddle -> paddle)
        // and never produce a duplicate chip for that format.
        assert_eq!(hf_model(&["jax", "flax"]).model_formats(), vec!["flax"],);
        assert_eq!(hf_model(&["paddlepaddle"]).model_formats(), vec!["paddle"],);
        // No recognized weight format -> no chips.
        assert!(hf_model(&["text-generation", "en", "license:mit"])
            .model_formats()
            .is_empty());
        // The mixed-case labels (Core ML / Safetensors / PaddlePaddle) come from
        // the shared pretty_format, so card chips read the same as variant badges.
        assert_eq!(crate::inspect::pretty_format("coreml"), "Core ML");
        assert_eq!(crate::inspect::pretty_format("safetensors"), "Safetensors");
    }

    #[test]
    fn license_redistribution_mapping() {
        use RedistributionClass::{PublicDownloadOnly, PublicP2pAllowed};
        // Permissive + open-weight model licenses are reseedable (normalized,
        // by family), including the ones the user's Qwen download hit.
        for l in [
            "apache-2.0",
            "Apache-2.0", // case-insensitive
            " mit ",      // trimmed
            "bsd-3-clause",
            "cc-by-4.0",
            "llama3.1",
            "gemma",
            "qwen",
            "qwen-research",
            "tongyi-qianwen",
            "mistral",
        ] {
            assert_eq!(
                redistribution_for(&Some(l.into())),
                PublicP2pAllowed,
                "{l} should be reseedable"
            );
        }
        // Unknown / non-redistributable stay download-only (opt-in still works).
        for l in ["other", "proprietary", "cc-nd-4.0-fake", "unknown"] {
            assert_eq!(
                redistribution_for(&Some(l.into())),
                PublicDownloadOnly,
                "{l} should stay download-only"
            );
        }
        assert_eq!(redistribution_for(&None), PublicDownloadOnly);
    }

    fn hf_file(
        rfilename: &str,
        size: u64,
        sha256: Option<String>,
        blob_id: Option<String>,
    ) -> HfFile {
        HfFile {
            rfilename: rfilename.into(),
            size,
            sha256,
            blob_id,
        }
    }

    #[test]
    fn quant_recommendation_is_budget_aware() {
        let gb = 1_000_000_000u64;
        let q2 = hf_file("m-q2_k.gguf", 3 * gb, Some("a".repeat(64)), None);
        let q4 = hf_file("m-q4_k_m.gguf", 5 * gb, Some("b".repeat(64)), None);
        let q6 = hf_file("m-q6_k.gguf", 8 * gb, Some("c".repeat(64)), None);
        let q8 = hf_file("m-q8_0.gguf", 12 * gb, Some("d".repeat(64)), None);
        let files: Vec<&HfFile> = vec![&q2, &q4, &q6, &q8];

        // ~16 GB budget (×1.2 headroom): the largest that fits is q8
        let pick = recommend_quant_for_budget(&files, 16 * gb).unwrap();
        assert_eq!(pick.rfilename, "m-q8_0.gguf");
        assert!(pick.fits);

        // ~8 GB budget: q8/q6 are too big (×1.2), q4 (5×1.2=6 ≤ 8) is the best fit.
        let pick = recommend_quant_for_budget(&files, 8 * gb).unwrap();
        assert_eq!(pick.rfilename, "m-q4_k_m.gguf");
        assert!(pick.fits);

        // Tiny budget: nothing fits ⇒ smallest, flagged as not fitting.
        let pick = recommend_quant_for_budget(&files, gb).unwrap();
        assert_eq!(pick.rfilename, "m-q2_k.gguf");
        assert!(!pick.fits);

        // Unknown budget ⇒ balanced default prefers Q4_K_M.
        let pick = recommend_quant_for_budget(&files, 0).unwrap();
        assert_eq!(pick.rfilename, "m-q4_k_m.gguf");
        assert!(!pick.fits);
    }

    #[test]
    fn synthesized_manifest_is_valid() {
        let detail = HfModelDetail {
            id: "Qwen/Qwen2.5-0.5B-Instruct-GGUF".into(),
            revision: "abc123".into(),
            gated: false,
            license: Some("apache-2.0".into()),
            tags: vec![],
            files: vec![],
            ..Default::default()
        };
        let file = hf_file(
            "qwen2.5-0.5b-instruct-q4_k_m.gguf",
            400_000_000,
            Some("a".repeat(64)),
            None,
        );
        let m = manifest_for(&detail, &file).unwrap();
        m.validate().unwrap();
        assert!(!m.artifacts[0].hashes.has_blake3());
        assert!(m.artifacts[0].hashes.has_sha256());
        assert_eq!(m.model.format.as_deref(), Some("gguf"));
        assert!(matches!(
            m.artifacts[0].sources[0],
            Source::Huggingface { .. }
        ));
    }

    #[test]
    fn bundles_sharded_safetensors_with_sidecars() {
        // A two-shard MLX repo: shards verified by sha256, sidecars by git OID.
        let detail = HfModelDetail {
            id: "mlx-community/Some-Model-4bit".into(),
            revision: "deadbeef".into(),
            gated: false,
            license: Some("apache-2.0".into()),
            tags: vec!["mlx".into(), "safetensors".into()],
            files: vec![
                hf_file(
                    "model-00001-of-00002.safetensors",
                    100,
                    Some("a".repeat(64)),
                    None,
                ),
                hf_file(
                    "model-00002-of-00002.safetensors",
                    200,
                    Some("b".repeat(64)),
                    None,
                ),
                hf_file(
                    "model.safetensors.index.json",
                    40,
                    None,
                    Some("c".repeat(40)),
                ),
                hf_file("config.json", 30, None, Some("d".repeat(40))),
                hf_file("tokenizer.json", 50, None, Some("e".repeat(40))),
                // No blobId ⇒ unverifiable ⇒ excluded from the bundle.
                hf_file("vocab.json", 10, None, None),
                // Repo cruft is never bundled.
                hf_file("README.md", 5, None, Some("f".repeat(40))),
                hf_file(".gitattributes", 5, None, Some("0".repeat(40))),
            ],
            ..Default::default()
        };
        assert!(detail.has_safetensors_bundle());
        assert!(detail.is_mlx());
        assert_eq!(detail.bundle_variant_label(), "MLX · 4-bit");
        assert_eq!(detail.bundle_total_size(), 100 + 200 + 40 + 30 + 50);

        let m = manifest_for_bundle(&detail).unwrap();
        m.validate().unwrap();
        assert_eq!(m.artifacts.len(), 5);
        assert_eq!(m.model.format.as_deref(), Some("mlx"));
        assert_eq!(m.model.quantization, None);
        let weights: Vec<_> = m.artifacts.iter().filter(|a| a.role == "weights").collect();
        assert_eq!(weights.len(), 2);
        assert!(weights.iter().all(|a| a.hashes.has_sha256()));
        let sidecars: Vec<_> = m.artifacts.iter().filter(|a| a.role != "weights").collect();
        assert_eq!(sidecars.len(), 3);
        assert!(sidecars.iter().all(|a| a.hashes.has_git_blob_sha1()));
        assert!(m.artifacts.iter().all(|a| !a.path.contains('/')));
    }

    #[test]
    fn query_strips_quant_suffix() {
        assert_eq!(
            query_from_filename("qwen2.5-0.5b-instruct-q4_k_m.gguf"),
            "qwen2 5 0 5b instruct gguf"
        );
        assert_eq!(
            query_from_filename("Meta-Llama-3-8B-Instruct.IQ3_M.gguf"),
            "Meta Llama 3 8B Instruct gguf"
        );
        assert_eq!(
            query_from_filename("model.safetensors"),
            "model safetensors"
        );
    }

    #[test]
    fn variant_label_from_filename() {
        let f = hf_file("qwen2.5-0.5b-instruct-q4_k_m.gguf", 1, None, None);
        assert_eq!(f.variant_label(), "Q4_K_M");
    }

    #[test]
    fn sidecar_classification() {
        // Verifiable companion files are sidecars…
        assert!(hf_file("config.json", 1, None, Some("a".repeat(40))).is_model_sidecar());
        assert!(hf_file("tokenizer.model", 1, None, Some("a".repeat(40))).is_model_sidecar());
        assert!(hf_file("merges.txt", 1, None, Some("a".repeat(40))).is_model_sidecar());
        // …but weights, cruft, and unverifiable files are not.
        assert!(!hf_file("model.safetensors", 1, Some("a".repeat(64)), None).is_model_sidecar());
        assert!(!hf_file("README.md", 1, None, Some("a".repeat(40))).is_model_sidecar());
        assert!(!hf_file("config.json", 1, None, None).is_model_sidecar());
    }

    #[test]
    fn split_gguf_collapses_into_one_quant_and_manifest() {
        let files = vec![
            hf_file(
                "GLM-5-Q4_K_M-00001-of-00003.gguf",
                10,
                Some("a".repeat(64)),
                None,
            ),
            hf_file(
                "GLM-5-Q4_K_M-00002-of-00003.gguf",
                10,
                Some("b".repeat(64)),
                None,
            ),
            hf_file(
                "GLM-5-Q4_K_M-00003-of-00003.gguf",
                10,
                Some("c".repeat(64)),
                None,
            ),
            hf_file("GLM-5-Q8_0.gguf", 50, Some("d".repeat(64)), None),
        ];
        let detail = HfModelDetail {
            id: "org/GLM-5-GGUF".to_string(),
            revision: "main".to_string(),
            gated: false,
            license: None,
            tags: vec![],
            files,
            ..Default::default()
        };

        let quants = detail.gguf_quants();
        assert_eq!(
            quants.len(),
            2,
            "three shards collapse into one quant, plus the single Q8_0"
        );

        let q4 = quants
            .iter()
            .find(|q| q.label == "Q4_K_M")
            .expect("Q4_K_M quant");
        assert_eq!(q4.files.len(), 3);
        assert_eq!(q4.total_size(), 30);
        assert!(
            q4.files[0].rfilename.contains("00001-of-00003"),
            "shards sorted by index"
        );
        assert!(q4.files[2].rfilename.contains("00003-of-00003"));

        let q8 = quants
            .iter()
            .find(|q| q.label == "Q8_0")
            .expect("Q8_0 quant");
        assert_eq!(q8.files.len(), 1);
        let m = manifest_for_gguf_quant(&detail, &q4.files).unwrap();
        assert_eq!(m.artifacts.len(), 3);
        assert_eq!(m.model.quantization.as_deref(), Some("Q4_K_M"));
    }
}
