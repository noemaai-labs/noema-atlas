import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

export const api = {
  appInfo: () => invoke("app_info"),
  search: (query) => invoke("search_models", { query }),
  popular: () => invoke("popular_models"),
  modelDetail: (id) => invoke("model_detail", { id }),
  download: (id, file, clientRef) =>
    invoke("download_model", { id, file: file ?? null, clientRef: clientRef ?? null }),
  resumeDownload: (manifestId) => invoke("resume_download", { manifestId }),
  mesh: (query) => invoke("mesh_search", { query }),
  addByLink: (link, clientRef) =>
    invoke("add_by_link", { link, clientRef: clientRef ?? null }),
  addFromMesh: (m, clientRef) =>
    invoke("add_from_mesh", {
      blake3: m.blake3,
      sha256: m.sha256,
      name: m.name,
      size: m.size,
      license: m.license,
      magnet: m.magnet ?? null,
      clientRef: clientRef ?? null,
    }),
  listTransfers: () => invoke("list_transfers"),
  resumableDownloads: () => invoke("resumable_downloads"),
  removeTransfer: (transferId) => invoke("remove_transfer", { transferId }),
  discardTransfer: (transferId) => invoke("discard_transfer", { transferId }),
  startWorldwide: () => invoke("start_worldwide"),
  stopWorldwide: () => invoke("stop_worldwide"),
  worldwideStatus: () => invoke("worldwide_status"),
  uploadsList: () => invoke("uploads_list"),
  applyIdentity: (deviceName) => invoke("apply_identity", { deviceName }),
  library: () => invoke("list_library"),
  install: (manifestId, target) => invoke("install_model", { manifestId, target }),
  setShare: (blake3, sha256, on) => invoke("set_share", { blake3, sha256, on }),
  shareNeedsConfirmation: (manifestId) =>
    invoke("share_needs_confirmation", { manifestId }),
  confirmGatedShare: (blake3, sha256) =>
    invoke("confirm_gated_share", { blake3, sha256 }),
  importLocal: (args) => invoke("import_local", args),
  editModel: (args) => invoke("edit_model", args),
  deleteModel: (blake3, sha256) => invoke("delete_model", { blake3, sha256 }),
  copyShareLink: (manifestId) => invoke("copy_share_link", { manifestId }),
  btMagnet: (blake3) => invoke("bt_magnet", { blake3 }),
  isIrohSeeding: (blake3) => invoke("is_iroh_seeding", { blake3 }),
  btPeers: (transferId) => invoke("bt_peers", { transferId }),
  btBlobRatio: (blake3) => invoke("bt_blob_ratio", { blake3 }),
  setBtBlobRatio: (blake3, cap) => invoke("set_bt_blob_ratio", { blake3, cap }),
  btForceRecheck: (blake3) => invoke("bt_force_recheck", { blake3 }),
  downloadQueueOrder: () => invoke("download_queue_order"),
  queueReorder: (id, dir) => invoke("queue_reorder", { id, dir }),
  setDownloadPreference: (preference) =>
    invoke("set_download_preference", { preference }),
  pauseAll: () => invoke("pause_all"),
  reveal: (path) => invoke("reveal", { path }),
  pauseDownload: (transferId) => invoke("pause_download", { transferId: transferId ?? null }),
  stopDownload: (transferId) => invoke("stop_download", { transferId: transferId ?? null }),
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
// Fires once per download, right after the engine registers the manifest, so the
// front-end can re-key its provisional (`tmp_…`) row by the real transfer id.
export const onRegistered = (cb) =>
  listen("download://registered", (e) => cb(e.payload));
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
