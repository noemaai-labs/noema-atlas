import { writable } from "svelte/store";
import { fmtSize } from "./format.js";

// Session-only completion receipts keyed by manifest_id; not persisted.
export const receipts = writable(new Map());

const LABELS = {
  iroh: "an Iroh peer",
  bt: "the BitTorrent swarm",
  hf: "Hugging Face",
  https: "an HTTPS mirror",
  file: "a local file",
};

// routeBytes → "fetched from …" summary, biggest contributor first; "" when empty.
export function buildReceipt(routeBytes) {
  const parts = Object.entries(routeBytes || {})
    .filter(([, b]) => b > 0)
    .sort((a, b) => b[1] - a[1])
    .map(([c, b]) => `${LABELS[c] || c} ${fmtSize(b)}`);
  return parts.length ? `fetched from ${parts.join(" + ")} — verified` : "";
}

export function setReceipt(manifestId, text) {
  if (!manifestId || !text) return;
  receipts.update((m) => {
    m.set(manifestId, text);
    return m;
  });
}
