export function fmtSize(bytes) {
  const b = Number(bytes) || 0;
  if (b === 0) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let n = b;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return n.toFixed(i > 1 ? 2 : 0) + " " + units[i];
}

// Bytes/sec → short rate string ("1.2 MB/s"). "—" when zero/unknown.
export function fmtRate(bps) {
  const n = Number(bps) || 0;
  if (n === 0) return "—";
  return fmtSize(n) + "/s";
}

// Seed ratio = bytes uploaded to peers / bytes downloaded. "" when nothing downloaded yet.
export function fmtRatio(uploaded, downloaded) {
  const up = Number(uploaded) || 0;
  const down = Number(downloaded) || 0;
  if (down <= 0) return "";
  return (up / down).toFixed(2) + "×";
}

// Route class ("iroh" / "bt" / "https" / "hf" / "file") of a raw engine source id
// (iroh:<hash> / btv2:<magnet> / hf:… / https:… / file:…). "" when unrecognized.
export function routeClassOf(src) {
  if (!src) return "";
  const s = String(src).trim().toLowerCase();
  if (s.startsWith("iroh:")) return "iroh";
  if (s.startsWith("https:")) return "https";
  if (s.startsWith("hf:")) return "hf";
  if (s.startsWith("btv2:") || s.startsWith("bittorrent") || s.startsWith("magnet:")) return "bt";
  if (s.startsWith("file:")) return "file";
  return "";
}

// One hover blurb per transport, shared across all transport pills/names.
export const TRANSPORT_HINTS = {
  iroh: "Noema's worldwide peer network — verified pieces striped from many peers at once.",
  bt: "The public torrent network — extra seeders beyond Noema users.",
  hf: "The original host — fallback route, verified against the same hash.",
  https: "A direct mirror download, verified against the same hash.",
};

// Human label for a model format tag — mirrors noema_core::inspect::pretty_format. Unknown tags uppercase.
const FORMAT_LABELS = {
  gguf: "GGUF",
  safetensors: "Safetensors",
  onnx: "ONNX",
  pytorch: "PyTorch",
  ggml: "GGML",
  mlx: "MLX",
  coreml: "Core ML",
  tflite: "TensorFlow Lite",
  keras: "Keras",
  numpy: "NumPy",
  flax: "Flax",
  paddle: "PaddlePaddle",
  tensorrt: "TensorRT",
  json: "JSON",
};
export function prettyFormat(format) {
  if (!format) return "";
  const f = String(format).toLowerCase();
  return FORMAT_LABELS[f] || f.toUpperCase();
}

// Recognize a model format from a filename — mirrors noema_core::inspect::format_from_name. "" when nothing recognizable.
export function formatFromName(filename) {
  if (!filename) return "";
  const lower = String(filename).toLowerCase();
  if (lower.includes(".mlx") || lower.includes("-mlx") || lower.includes("_mlx"))
    return "mlx";
  const ext = lower.includes(".") ? lower.split(".").pop() : "";
  switch (ext) {
    case "gguf":
      return "gguf";
    case "safetensors":
    case "sft":
      return "safetensors";
    case "onnx":
    case "onnx_data":
      return "onnx";
    case "pt":
    case "pth":
      return "pytorch";
    case "bin":
      return lower.includes("ggml") ? "ggml" : "pytorch";
    case "ggml":
      return "ggml";
    case "mlmodel":
    case "mlpackage":
      return "coreml";
    case "tflite":
      return "tflite";
    case "h5":
    case "hdf5":
    case "keras":
      return "keras";
    case "npz":
    case "npy":
      return "numpy";
    case "msgpack":
      return "flax";
    case "pdparams":
    case "pdmodel":
      return "paddle";
    case "trt":
    case "engine":
    case "plan":
      return "tensorrt";
    default:
      return "";
  }
}

// Best-effort format label for a row that may carry an explicit format string or
// only a name. Returns "" when nothing recognizable.
export function rowFormat(format, name) {
  return prettyFormat(format || formatFromName(name));
}

// Canonical lowercase format id ("gguf", "mlx", …) for a row. "" when unrecognized. Used as the `.f-<id>` chip class.
export function formatId(format, name) {
  return (format ? String(format).toLowerCase() : formatFromName(name)) || "";
}
