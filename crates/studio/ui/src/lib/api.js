import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

export const api = {
  appInfo: () => invoke("app_info"),
  search: (query) => invoke("search_models", { query }),
  popular: () => invoke("popular_models"),
  modelDetail: (id) => invoke("model_detail", { id }),
  download: (id, file) => invoke("download_model", { id, file: file ?? null }),
  resumeDownload: (manifestId) => invoke("resume_download", { manifestId }),
  mesh: (query) => invoke("mesh_search", { query }),
  addByLink: (link) => invoke("add_by_link", { link }),
  addFromMesh: (m) =>
    invoke("add_from_mesh", {
      blake3: m.blake3,
      sha256: m.sha256,
      name: m.name,
      size: m.size,
      license: m.license,
    }),
  worldwidePeers: (hash) => invoke("worldwide_peers", { hash }),
  startWorldwide: () => invoke("start_worldwide"),
  stopWorldwide: () => invoke("stop_worldwide"),
  worldwideStatus: () => invoke("worldwide_status"),
  seederMetrics: () => invoke("seeder_metrics"),
  uploadsList: () => invoke("uploads_list"),
  applyIdentity: (deviceName, groupCode) =>
    invoke("apply_identity", { deviceName, groupCode }),
  createGroup: () => invoke("create_group"),
  library: () => invoke("list_library"),
  install: (manifestId, target) => invoke("install_model", { manifestId, target }),
  setShare: (blake3, sha256, on) => invoke("set_share", { blake3, sha256, on }),
  importLocal: (args) => invoke("import_local", args),
  editModel: (args) => invoke("edit_model", args),
  deleteModel: (blake3, sha256) => invoke("delete_model", { blake3, sha256 }),
  copyShareLink: (manifestId) => invoke("copy_share_link", { manifestId }),
  reveal: (path) => invoke("reveal", { path }),
  pauseDownload: () => invoke("pause_download"),
  stopDownload: () => invoke("stop_download"),
  setToken: (token) => invoke("set_token", { token }),
  clearToken: () => invoke("clear_token"),
  tokenStatus: () => invoke("token_status"),
  cache: () => invoke("list_cache"),
  health: () => invoke("source_health"),
  clearCache: () => invoke("clear_cache"),
  exportDiagnostics: () => invoke("export_diagnostics"),
  getSettings: () => invoke("get_settings"),
  saveSettings: (settings) => invoke("save_settings", { settings }),
};
export const pickModelFile = () =>
  open({
    multiple: false,
    directory: false,
    filters: [{ name: "Model weights", extensions: ["gguf", "safetensors", "bin"] }],
  });

export const onProgress = (cb) =>
  listen("download://progress", (e) => cb(e.payload));
export const onDone = (cb) => listen("download://done", (e) => cb(e.payload));
export const onImportProgress = (cb) =>
  listen("import://progress", (e) => cb(e.payload));

export async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch (e) {
    return false;
  }
}
