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

// Seed ratio = bytes uploaded to peers / bytes of the file downloaded. Returns a
// short string ("0.00", "1.4×") or "" when there's nothing downloaded yet.
export function fmtRatio(uploaded, downloaded) {
  const up = Number(uploaded) || 0;
  const down = Number(downloaded) || 0;
  if (down <= 0) return "";
  return (up / down).toFixed(2) + "×";
}

// Human label for a model format tag — mirrors noema_core::inspect::pretty_format
// so a badge reads "Core ML" / "Safetensors" rather than a bare lowercase tag.
// Unknown tags uppercase.
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

// Recognize a model format from a filename — mirrors
// noema_core::inspect::format_from_name so rows that only carry a name (Library,
// Transfers, the mesh) can show the same badge as the HF browser. Returns "" when
// nothing recognizable (so the badge is simply omitted).
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

// The canonical format id (lowercase: "gguf", "mlx", …) for a row, from its
// explicit format or its filename. "" when unrecognized. Used as the `.f-<id>`
// chip class so the format badge is color-coded the same everywhere it appears.
export function formatId(format, name) {
  return (format ? String(format).toLowerCase() : formatFromName(name)) || "";
}
