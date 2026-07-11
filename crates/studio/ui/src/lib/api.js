import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

export const api = {
  appInfo: () => invoke("app_info"),
  search: (query) => invoke("search_models", { query }),
  popular: () => invoke("popular_models"),
  modelList: ({ search, sort, ggufOnly, limit } = {}) =>
    invoke("model_list", {
      search: search ?? null,
      sort: sort || "trending",
      ggufOnly: !!ggufOnly,
      limit: limit ?? null,
    }),
  modelListPage: (next) => invoke("model_list_page", { next }),
  modelConversions: (id) => invoke("model_conversions", { id }),
  checkModelUpdates: () => invoke("check_model_updates"),
  scanImport: () => invoke("scan_import"),
  runtimesPresent: () => invoke("runtimes_present"),
  handoffLmstudio: (path, name) => invoke("handoff_lmstudio", { path, name }),
  handoffOllama: (path, name) => invoke("handoff_ollama", { path, name }),
  modelDetail: (id) => invoke("model_detail", { id }),
  readme: (id, revision) => invoke("model_readme", { id, revision }),
  openExternal: (url) => invoke("open_external", { url }),
  download: (id, file, clientRef, bundle) =>
    invoke("download_model", {
      id,
      file: file ?? null,
      bundle: bundle ?? null,
      clientRef: clientRef ?? null,
    }),
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
  shareActivity: (blake3) => invoke("share_activity", { blake3 }),
  transferRoutes: (manifestId) => invoke("transfer_routes", { manifestId }),
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
// Fires after the engine registers the manifest, so the UI can re-key its tmp_… row by the real transfer id.
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
