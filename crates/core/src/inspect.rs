use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

/// Metadata sniffed from a model file's header. All fields are best-effort.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileMeta {
    /// `"gguf"` / `"safetensors"`, derived from the magic bytes or extension.
    pub format: Option<String>,
    /// `general.name` — the author's human name for the model.
    pub name: Option<String>,
    /// `general.architecture` — e.g. `llama`, `qwen2`, `phi3`.
    pub architecture: Option<String>,
    /// Quantization label (e.g. `Q4_K_M`), decoded from `general.file_type`.
    pub quantization: Option<String>,
    /// `general.size_label` — e.g. `8B`, `8x7B`.
    pub size_label: Option<String>,
    /// `general.license` — an SPDX or HF-style tag, when the author set one.
    pub license: Option<String>,
    /// `general.basename` — the family stem, e.g. `Mistral`.
    pub basename: Option<String>,
    /// `general.finetune` — e.g. `Instruct`, `Chat`.
    pub finetune: Option<String>,
    /// `general.source.url` / `general.repo_url` — where the model came from.
    pub source_url: Option<String>,
}

/// A structured, human-meaningful identity for a model, merged from the file's
/// header and its filename. This is what the share composer pre-fills.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedModel {
    /// A human title, e.g. `Mistral-7B-Instruct-v0.3` (no quant, no extension).
    pub title: String,
    pub family: Option<String>,
    pub size_label: Option<String>,
    pub variant: Option<String>,
    /// Canonical quant label, e.g. `Q4_K_M`, `Q8_0`, `F16`.
    pub quant: Option<String>,
    pub architecture: Option<String>,
    /// `"gguf"` / `"safetensors"`, when known.
    pub format: Option<String>,
    /// A license tag the author embedded, when present.
    pub license: Option<String>,
    /// Original source URL embedded in the header, when present.
    pub source_url: Option<String>,
}

impl ParsedModel {
    /// The receiver-facing display label, e.g. `Mistral-7B-Instruct-v0.3 · Q4_K_M · GGUF`.
    pub fn display_label(&self) -> String {
        let mut s = self.title.clone();
        if let Some(q) = &self.quant {
            s.push_str(" · ");
            s.push_str(q);
        }
        if let Some(f) = &self.format {
            s.push_str(" · ");
            s.push_str(&pretty_format(f));
        }
        s
    }
}

/// Read a model file's header and return whatever metadata it carries. Never
/// fails — returns an empty `FileMeta` for anything it can't parse.
pub fn read_file_meta(path: &Path) -> FileMeta {
    match File::open(path) {
        Ok(f) => {
            let mut r = BufReader::new(f);
            let mut magic = [0u8; 8];
            if r.read_exact(&mut magic).is_err() {
                return meta_from_extension(path);
            }
            if &magic[0..4] == b"GGUF" {
                let version = u32::from_le_bytes(magic[4..8].try_into().unwrap());
                return read_gguf_meta(&mut r, version).unwrap_or_else(|| FileMeta {
                    format: Some("gguf".into()),
                    ..Default::default()
                });
            }
            // safetensors: 8-byte little-endian header length, then JSON. The
            // first 8 bytes we just read ARE that length prefix.
            if path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("safetensors"))
                .unwrap_or(false)
            {
                let header_len = u64::from_le_bytes(magic);
                return read_safetensors_meta(&mut r, header_len);
            }
            meta_from_extension(path)
        }
        Err(_) => meta_from_extension(path),
    }
}

fn meta_from_extension(path: &Path) -> FileMeta {
    FileMeta {
        format: format_from_name(path.file_name().and_then(|s| s.to_str()).unwrap_or("")),
        ..Default::default()
    }
}

/// Derive a coarse format tag from a filename. Extension-driven, with a name
/// heuristic for MLX (which ships as safetensors/npz tuned for Apple Silicon).
/// The tag is for display + cataloguing only — integrity is always the manifest
/// hash, and `validate_format_header` only hard-checks the formats it has magic
/// bytes for (gguf/safetensors), so a richer tag here never gates a download.
pub fn format_from_name(filename: &str) -> Option<String> {
    let lower = filename.to_ascii_lowercase();
    // MLX has no unique extension (it is safetensors/npz under the hood); read it
    // off the name so the badge says "MLX" rather than the container format.
    if lower.contains(".mlx") || lower.contains("-mlx") || lower.contains("_mlx") {
        return Some("mlx".into());
    }
    let fmt = match lower.rsplit('.').next()? {
        "gguf" => "gguf",
        "safetensors" | "sft" => "safetensors",
        "onnx" | "onnx_data" => "onnx",
        "pt" | "pth" => "pytorch",
        // Legacy GGML weights also used `.bin`; disambiguate by name, else the
        // overwhelmingly common case in a model repo is a PyTorch state dict.
        "bin" if lower.contains("ggml") => "ggml",
        "bin" => "pytorch",
        "ggml" => "ggml",
        "mlmodel" | "mlpackage" => "coreml",
        "tflite" => "tflite",
        "h5" | "hdf5" | "keras" => "keras",
        "npz" | "npy" => "numpy",
        "msgpack" => "flax",
        "pdparams" | "pdmodel" => "paddle",
        "trt" | "engine" | "plan" => "tensorrt",
        _ => return None,
    };
    Some(fmt.into())
}

/// Human-facing label for a format tag from [`format_from_name`] — nicer casing
/// than a bare uppercase (e.g. `coreml` -> `Core ML`). Unknown tags uppercase.
pub fn pretty_format(format: &str) -> String {
    match format.to_ascii_lowercase().as_str() {
        "gguf" => "GGUF",
        "safetensors" => "Safetensors",
        "onnx" => "ONNX",
        "pytorch" => "PyTorch",
        "ggml" => "GGML",
        "mlx" => "MLX",
        "coreml" => "Core ML",
        "tflite" => "TensorFlow Lite",
        "keras" => "Keras",
        "numpy" => "NumPy",
        "flax" => "Flax",
        "paddle" => "PaddlePaddle",
        "tensorrt" => "TensorRT",
        other => return other.to_ascii_uppercase(),
    }
    .to_string()
}
// We only need the `general.*` strings + the file_type enum. Cap how far we'll
// scan so a hostile/huge metadata block (e.g. a giant tokenizer token array)
// can't make us read forever — genuine `general.*` keys sit at the very front.
const GGUF_MAX_KV: u64 = 4096;
const GGUF_MAX_STR: u64 = 64 * 1024;
const GGUF_MAX_ARRAY_BYTES: u64 = 256 * 1024 * 1024;

fn read_gguf_meta<R: Read>(r: &mut R, version: u32) -> Option<FileMeta> {
    let mut meta = FileMeta {
        format: Some("gguf".into()),
        ..Default::default()
    };
    // After the 8-byte magic+version: tensor_count then metadata_kv_count.
    // v1 used u32 counts; v2/v3 use u64.
    let (_tensor_count, kv_count) = if version >= 2 {
        (read_u64(r)?, read_u64(r)?)
    } else {
        (read_u32(r)? as u64, read_u32(r)? as u64)
    };
    let mut file_type: Option<u32> = None;
    let mut n = 0u64;
    while n < kv_count.min(GGUF_MAX_KV) {
        n += 1;
        let key = read_gguf_string(r)?;
        let vtype = read_u32(r)?;
        match key.as_str() {
            "general.name" => meta.name = read_gguf_value_string(r, vtype),
            "general.architecture" => meta.architecture = read_gguf_value_string(r, vtype),
            "general.size_label" => meta.size_label = read_gguf_value_string(r, vtype),
            "general.license" => meta.license = read_gguf_value_string(r, vtype),
            "general.basename" => meta.basename = read_gguf_value_string(r, vtype),
            "general.finetune" => meta.finetune = read_gguf_value_string(r, vtype),
            "general.source.url" | "general.repo_url" | "general.source.huggingface.repository" => {
                if meta.source_url.is_none() {
                    meta.source_url = read_gguf_value_string(r, vtype);
                } else {
                    skip_gguf_value(r, vtype)?;
                }
            }
            "general.file_type" => {
                file_type = read_gguf_value_u32(r, vtype);
            }
            _ => skip_gguf_value(r, vtype)?,
        }
    }
    if let Some(ft) = file_type {
        meta.quantization = quant_from_file_type(ft);
    }
    Some(meta)
}

/// A GGUF string: u64 length, then that many UTF-8 bytes.
fn read_gguf_string<R: Read>(r: &mut R) -> Option<String> {
    let len = read_u64(r)?.min(GGUF_MAX_STR);
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Read a typed GGUF value, returning it as a string only if it IS a string.
fn read_gguf_value_string<R: Read>(r: &mut R, vtype: u32) -> Option<String> {
    if vtype == 8 {
        read_gguf_string(r).filter(|s| !s.trim().is_empty())
    } else {
        skip_gguf_value(r, vtype);
        None
    }
}

/// Read a typed GGUF value as a u32 (handles the common integer widths).
fn read_gguf_value_u32<R: Read>(r: &mut R, vtype: u32) -> Option<u32> {
    match vtype {
        4 => read_u32(r),                         // UINT32
        5 => read_u32(r),                         // INT32 (reinterpret)
        10 | 11 => read_u64(r).map(|v| v as u32), // UINT64 / INT64
        _ => {
            skip_gguf_value(r, vtype);
            None
        }
    }
}

/// Advance past a GGUF value of the given type without interpreting it.
fn skip_gguf_value<R: Read>(r: &mut R, vtype: u32) -> Option<()> {
    match vtype {
        0 | 1 | 7 => skip(r, 1), // (U)INT8, BOOL
        2..=3 => skip(r, 2),     // (U)INT16
        4..=6 => skip(r, 4),     // (U)INT32, FLOAT32
        10..=12 => skip(r, 8),   // (U)INT64, FLOAT64
        8 => {
            let len = read_u64(r)?.min(GGUF_MAX_STR);
            skip(r, len)
        }
        9 => {
            // ARRAY: element type (u32), count (u64), then elements.
            let elem_type = read_u32(r)?;
            let count = read_u64(r)?;
            if elem_type == 8 {
                // String array: each element is length-prefixed.
                let mut read = 0u64;
                for _ in 0..count {
                    let len = read_u64(r)?.min(GGUF_MAX_STR);
                    skip(r, len)?;
                    read = read.saturating_add(len).saturating_add(8);
                    if read > GGUF_MAX_ARRAY_BYTES {
                        return None;
                    }
                }
                Some(())
            } else {
                let each = scalar_width(elem_type)?;
                let total = (count as u128).saturating_mul(each as u128);
                if total > GGUF_MAX_ARRAY_BYTES as u128 {
                    return None;
                }
                skip(r, total as u64)
            }
        }
        _ => None,
    }
}

fn scalar_width(vtype: u32) -> Option<u64> {
    Some(match vtype {
        0 | 1 | 7 => 1,
        2..=3 => 2,
        4..=6 => 4,
        10..=12 => 8,
        _ => return None,
    })
}

/// Map the GGML `general.file_type` enum to a human quant label. Best-effort:
/// covers the common values; unknown values yield `None` (filename wins).
fn quant_from_file_type(ft: u32) -> Option<String> {
    Some(
        match ft {
            0 => "F32",
            1 => "F16",
            2 => "Q4_0",
            3 => "Q4_1",
            7 => "Q8_0",
            8 => "Q5_0",
            9 => "Q5_1",
            10 => "Q2_K",
            11 => "Q3_K_S",
            12 => "Q3_K_M",
            13 => "Q3_K_L",
            14 => "Q4_K_S",
            15 => "Q4_K_M",
            16 => "Q5_K_S",
            17 => "Q5_K_M",
            18 => "Q6_K",
            19 => "IQ2_XXS",
            20 => "IQ2_XS",
            21 => "Q2_K_S",
            23 => "IQ3_XXS",
            24 => "IQ1_S",
            25 => "IQ4_NL",
            26 => "IQ3_S",
            27 => "IQ3_M",
            28 => "IQ2_S",
            29 => "IQ2_M",
            30 => "IQ4_XS",
            31 => "IQ1_M",
            32 => "BF16",
            _ => return None,
        }
        .to_string(),
    )
}

fn read_u32<R: Read>(r: &mut R) -> Option<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).ok()?;
    Some(u32::from_le_bytes(b))
}

fn read_u64<R: Read>(r: &mut R) -> Option<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b).ok()?;
    Some(u64::from_le_bytes(b))
}

fn skip<R: Read>(r: &mut R, n: u64) -> Option<()> {
    let mut remaining = n;
    let mut buf = [0u8; 8192];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        r.read_exact(&mut buf[..want]).ok()?;
        remaining -= want as u64;
    }
    Some(())
}
fn read_safetensors_meta<R: Read>(r: &mut R, header_len: u64) -> FileMeta {
    let mut meta = FileMeta {
        format: Some("safetensors".into()),
        ..Default::default()
    };
    if header_len == 0 || header_len > 64 * 1024 * 1024 {
        return meta;
    }
    let mut buf = vec![0u8; header_len as usize];
    if r.read_exact(&mut buf).is_err() {
        return meta;
    }
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&buf) else {
        return meta;
    };
    if let Some(m) = json.get("__metadata__").and_then(|v| v.as_object()) {
        let get = |k: &str| m.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
        meta.name = get("model_name").or_else(|| get("name"));
        meta.architecture = get("model_type").or_else(|| get("architecture"));
        meta.license = get("license");
        meta.source_url = get("source").or_else(|| get("repo"));
    }
    meta
}
/// Merge a file's header metadata and its filename into one structured identity.
/// Header values win (the author wrote them); the filename fills the gaps —
/// most importantly the quant token, which GGUF only encodes as an enum.
pub fn parse_model(filename: &str, meta: &FileMeta) -> ParsedModel {
    let base = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let format = meta.format.clone().or_else(|| format_from_name(base));
    let stem = strip_known_ext(base);
    let tokens: Vec<&str> = stem
        .split(['-', '_', '.', ' '])
        .filter(|t| !t.is_empty())
        .collect();

    let quant = meta
        .quantization
        .clone()
        .or_else(|| quant_from_tokens(&tokens));
    let size_label = meta
        .size_label
        .clone()
        .or_else(|| size_from_tokens(&tokens));
    let variant = meta
        .finetune
        .clone()
        .or_else(|| variant_from_tokens(&tokens));

    // Title: prefer the author's `general.name`; else reconstruct from the
    // filename stem with quant/format tokens dropped.
    let title = meta
        .name
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| title_from_tokens(&tokens));

    let family = meta
        .basename
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| tokens.first().map(|t| t.to_string()));

    ParsedModel {
        title,
        family,
        size_label,
        variant,
        quant,
        architecture: meta.architecture.clone(),
        format,
        license: meta.license.clone(),
        source_url: meta.source_url.clone(),
    }
}

fn strip_known_ext(name: &str) -> &str {
    for ext in [".gguf", ".safetensors", ".bin", ".pt", ".pth", ".ckpt"] {
        if name.to_ascii_lowercase().ends_with(ext) {
            return &name[..name.len() - ext.len()];
        }
    }
    name
}

/// Reconstruct a human title from filename tokens: drop trailing quant tokens,
/// drop a `gguf`/`safetensors`/shard suffix, rejoin with hyphens, original case.
fn title_from_tokens(tokens: &[&str]) -> String {
    let mut kept: Vec<&str> = tokens
        .iter()
        .copied()
        .filter(|t| {
            let l = t.to_ascii_lowercase();
            l != "gguf" && l != "safetensors" && l != "bin"
        })
        .collect();
    // Drop a trailing shard marker like `00001` `of` `00003`.
    while matches!(kept.last(), Some(t) if t.eq_ignore_ascii_case("of") || is_shard_number(t)) {
        kept.pop();
    }
    // Drop trailing quant tokens (often several: `q4`, `k`, `m`).
    while matches!(kept.last(), Some(t) if is_quant_token(t)) {
        kept.pop();
    }
    let joined = kept.join("-");
    if joined.trim().is_empty() {
        "Untitled model".to_string()
    } else {
        joined
    }
}

fn is_shard_number(t: &str) -> bool {
    t.len() >= 3 && t.chars().all(|c| c.is_ascii_digit())
}

/// Detect and canonicalize a quant token from the filename tokens, e.g. the run
/// `q4 k m` -> `Q4_K_M`, `q8 0` -> `Q8_0`, `f16` -> `F16`.
fn quant_from_tokens(tokens: &[&str]) -> Option<String> {
    let lower: Vec<String> = tokens.iter().map(|t| t.to_ascii_lowercase()).collect();
    // Find the index of a quant *anchor* (`q<digits>`, `iq<n>`, `f16`, `bf16`).
    let start = lower.iter().position(|t| {
        t == "f16"
            || t == "bf16"
            || t == "f32"
            || t == "fp16"
            || (t.starts_with('q')
                && t.len() >= 2
                && t[1..].chars().next().is_some_and(|c| c.is_ascii_digit()))
            || (t.starts_with("iq") && t.len() >= 3)
    })?;
    let mut parts: Vec<String> = vec![lower[start].clone()];
    for t in lower.iter().skip(start + 1) {
        if is_quant_suffix(t) {
            parts.push(t.clone());
        } else {
            break;
        }
    }
    Some(parts.join("_").to_ascii_uppercase())
}

fn is_quant_suffix(t: &str) -> bool {
    matches!(
        t,
        "k" | "m" | "s" | "l" | "xl" | "xs" | "xxs" | "0" | "1" | "nl"
    )
}

fn is_quant_token(t: &str) -> bool {
    let t = t.to_ascii_lowercase();
    is_quant_suffix(&t)
        || matches!(t.as_str(), "f16" | "bf16" | "fp16" | "f32" | "imat")
        || (t.starts_with('q') && t.len() <= 3 && t[1..].chars().all(|c| c.is_ascii_digit()))
        || (t.starts_with("iq") && t.len() <= 4)
}

/// A size token like `7b`, `8x7b`, `70b`, `1.5b`, `350m`.
fn size_from_tokens(tokens: &[&str]) -> Option<String> {
    tokens.iter().find_map(|t| {
        let l = t.to_ascii_lowercase();
        let ok = (l.ends_with('b') || l.ends_with('m'))
            && l.len() >= 2
            && l[..l.len() - 1]
                .chars()
                .all(|c| c.is_ascii_digit() || c == 'x' || c == '.')
            && l.chars().any(|c| c.is_ascii_digit());
        ok.then(|| t.to_string())
    })
}

const VARIANT_WORDS: &[&str] = &[
    "instruct",
    "chat",
    "base",
    "it",
    "dpo",
    "sft",
    "rlhf",
    "code",
    "vision",
    "reasoning",
];

fn variant_from_tokens(tokens: &[&str]) -> Option<String> {
    tokens.iter().find_map(|t| {
        let l = t.to_ascii_lowercase();
        VARIANT_WORDS.contains(&l.as_str()).then(|| t.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gguf_filename_no_header() {
        let m = FileMeta {
            format: Some("gguf".into()),
            ..Default::default()
        };
        let p = parse_model("mistral-7b-instruct-v0.3.Q4_K_M.gguf", &m);
        assert_eq!(p.quant.as_deref(), Some("Q4_K_M"));
        assert_eq!(p.size_label.as_deref(), Some("7b"));
        assert_eq!(p.variant.as_deref(), Some("instruct"));
        assert_eq!(p.format.as_deref(), Some("gguf"));
        assert!(!p.title.to_lowercase().contains("q4"));
        assert!(!p.title.to_lowercase().ends_with("gguf"));
    }

    #[test]
    fn header_name_wins_over_filename() {
        let m = FileMeta {
            format: Some("gguf".into()),
            name: Some("Mistral 7B Instruct v0.3".into()),
            architecture: Some("llama".into()),
            quantization: Some("Q4_K_M".into()),
            ..Default::default()
        };
        let p = parse_model("ggml-model-q4_0.gguf", &m);
        assert_eq!(p.title, "Mistral 7B Instruct v0.3");
        assert_eq!(p.architecture.as_deref(), Some("llama"));
        // Header file_type wins over the filename's q4_0.
        assert_eq!(p.quant.as_deref(), Some("Q4_K_M"));
    }

    #[test]
    fn opaque_filename_yields_untitled() {
        let m = FileMeta::default();
        let p = parse_model("ggml-model-q4_0.gguf", &m);
        assert_eq!(p.quant.as_deref(), Some("Q4_0"));
        // "ggml model" remains as the title once quant is stripped.
        assert!(!p.title.is_empty());
    }

    #[test]
    fn quant_variants() {
        let m = FileMeta::default();
        assert_eq!(
            parse_model("foo-q8_0.gguf", &m).quant.as_deref(),
            Some("Q8_0")
        );
        assert_eq!(
            parse_model("foo.f16.gguf", &m).quant.as_deref(),
            Some("F16")
        );
        assert_eq!(
            parse_model("foo-iq4_xs.gguf", &m).quant.as_deref(),
            Some("IQ4_XS")
        );
        assert_eq!(parse_model("foo.safetensors", &m).quant, None);
    }

    #[test]
    fn display_label_composes() {
        let p = ParsedModel {
            title: "Mistral-7B-Instruct-v0.3".into(),
            quant: Some("Q4_K_M".into()),
            format: Some("gguf".into()),
            ..Default::default()
        };
        assert_eq!(
            p.display_label(),
            "Mistral-7B-Instruct-v0.3 · Q4_K_M · GGUF"
        );
    }

    #[test]
    fn gguf_header_roundtrip() {
        // Build a tiny in-memory GGUF v3 header with two string KVs and a
        // file_type, then parse it back.
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"GGUF");
        buf.extend_from_slice(&3u32.to_le_bytes()); // version
        buf.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
        buf.extend_from_slice(&2u64.to_le_bytes()); // kv_count
        let put_str = |buf: &mut Vec<u8>, k: &str, v: &str| {
            buf.extend_from_slice(&(k.len() as u64).to_le_bytes());
            buf.extend_from_slice(k.as_bytes());
            buf.extend_from_slice(&8u32.to_le_bytes()); // STRING
            buf.extend_from_slice(&(v.len() as u64).to_le_bytes());
            buf.extend_from_slice(v.as_bytes());
        };
        put_str(&mut buf, "general.name", "Tiny Test Model");
        // general.file_type = 15 (Q4_K_M) as UINT32
        let k = "general.file_type";
        buf.extend_from_slice(&(k.len() as u64).to_le_bytes());
        buf.extend_from_slice(k.as_bytes());
        buf.extend_from_slice(&4u32.to_le_bytes()); // UINT32
        buf.extend_from_slice(&15u32.to_le_bytes());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.gguf");
        std::fs::write(&path, &buf).unwrap();
        let m = read_file_meta(&path);
        assert_eq!(m.format.as_deref(), Some("gguf"));
        assert_eq!(m.name.as_deref(), Some("Tiny Test Model"));
        assert_eq!(m.quantization.as_deref(), Some("Q4_K_M"));
    }

    #[test]
    fn format_from_name_recognizes_common_formats() {
        let cases = [
            ("model.gguf", Some("gguf")),
            ("model.safetensors", Some("safetensors")),
            ("model.onnx", Some("onnx")),
            ("model.onnx_data", Some("onnx")),
            ("consolidated.00.pth", Some("pytorch")),
            ("pytorch_model.bin", Some("pytorch")),
            ("ggml-model-q4_0.bin", Some("ggml")),
            ("Model.MLModel", Some("coreml")),
            ("model.mlpackage", Some("coreml")),
            ("model.tflite", Some("tflite")),
            ("weights.npz", Some("numpy")),
            ("model.trt", Some("tensorrt")),
            // MLX has no unique extension — recognized by name.
            ("Qwen2.5-7B-mlx.safetensors", Some("mlx")),
            ("notes.txt", None),
            ("README.md", None),
        ];
        for (name, want) in cases {
            assert_eq!(
                format_from_name(name).as_deref(),
                want,
                "format_from_name({name:?})"
            );
        }
    }

    #[test]
    fn pretty_format_labels() {
        assert_eq!(pretty_format("safetensors"), "Safetensors");
        assert_eq!(pretty_format("onnx"), "ONNX");
        assert_eq!(pretty_format("coreml"), "Core ML");
        assert_eq!(pretty_format("mlx"), "MLX");
        // Unknown tags fall back to uppercase.
        assert_eq!(pretty_format("xyz"), "XYZ");
    }
}
