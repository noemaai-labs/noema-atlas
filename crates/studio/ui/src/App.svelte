<script>
  import { onMount } from "svelte";
  import { api, onProgress, onDone } from "./lib/api.js";
  import Sidebar from "./lib/Sidebar.svelte";
  import Discover from "./lib/views/Discover.svelte";
  import Explore from "./lib/views/Explore.svelte";
  import Library from "./lib/views/Library.svelte";
  import Transfers from "./lib/views/Transfers.svelte";
  import Settings from "./lib/views/Settings.svelte";
  import ShareComposer from "./lib/ShareComposer.svelte";

  let tab = "discover";
  let info = { name: "Noema Studio", version: "", root: "" };
  let transfer = null;
  let lastTick = null;
  let settings = null;
  let showIntro = false;
  let discoverFocus = 0;
  let dropComposer = null;

  function applyTheme(theme) {
    const resolved =
      theme === "system"
        ? window.matchMedia("(prefers-color-scheme: dark)").matches
          ? "dark"
          : "light"
        : theme;
    document.documentElement.setAttribute("data-theme", resolved);
  }

  onMount(async () => {
    try {
      info = await api.appInfo();
    } catch (e) {
    }
    try {
      settings = await api.getSettings();
      applyTheme(settings.theme);
      showIntro = !settings.seen_intro;
    } catch (e) {
      applyTheme("system");
    }

    onProgress((p) => {
      // Once a pause/stop is initiated (or settled), ignore late in-flight ticks
      // so the card doesn't flicker back to "downloading" before the done event.
      if (transfer && ["pausing", "stopping", "paused", "waiting"].includes(transfer.phase))
        return;
      const now = performance.now();
      let mbps = transfer?.mbps ?? 0;
      if (lastTick && p.bytes_done >= lastTick.bytes) {
        const dt = (now - lastTick.t) / 1000;
        if (dt > 0.25) {
          mbps = (p.bytes_done - lastTick.bytes) / dt / 1e6;
          lastTick = { t: now, bytes: p.bytes_done };
        }
      } else {
        lastTick = { t: now, bytes: p.bytes_done };
      }
      const remaining = p.bytes_total - p.bytes_done;
      const etaSec = mbps > 0 && remaining > 0 ? Math.round(remaining / (mbps * 1e6)) : null;
      transfer = { ...p, mbps, etaSec };
    });
    onDone((payload) => {
      if (!transfer) return;
      const status = payload && typeof payload === "object" ? payload.status : "done";
      const mid = (payload && payload.manifest_id) || transfer.manifest_id;
      if (status === "stopped") {
        transfer = null;
      } else if (status === "paused") {
        transfer = { ...transfer, manifest_id: mid, phase: "paused", mbps: 0, etaSec: null };
      } else if (status === "waiting") {
        transfer = { ...transfer, manifest_id: mid, phase: "waiting", mbps: 0, etaSec: null };
      } else if (status === "error") {
        const msg = (payload && payload.message) || "failed";
        transfer = { ...transfer, phase: "error: " + msg, mbps: 0, etaSec: null };
      } else {
        transfer = { ...transfer, phase: "done", mbps: 0, etaSec: null };
        // Clear a finished transfer after a moment so the card doesn't linger
        // forever — unless it's been replaced by a new one in the meantime.
        setTimeout(() => {
          if (transfer && transfer.phase === "done") transfer = null;
        }, 4000);
      }
    });
    try {
      const { getCurrentWebview } = await import("@tauri-apps/api/webview");
      await getCurrentWebview().onDragDropEvent((event) => {
        if (event.payload && event.payload.type === "drop") {
          const f = (event.payload.paths || []).find((p) =>
            /\.(gguf|safetensors|bin)$/i.test(p)
          );
          if (f) dropComposer = { path: f };
        }
      });
    } catch (e) {
    }
  });

  function onKey(e) {
    if ((e.metaKey || e.ctrlKey) && (e.key === "f" || e.key === "F")) {
      e.preventDefault();
      tab = "discover";
      discoverFocus++;
    } else if (e.key === "Escape") {
      if (dropComposer) dropComposer = null;
      else if (showIntro) dismissIntro();
    }
  }

  async function pauseTransfer() {
    if (transfer) transfer = { ...transfer, phase: "pausing", mbps: 0, etaSec: null };
    try {
      await api.pauseDownload();
    } catch (e) {}
  }
  async function stopTransfer() {
    if (transfer) transfer = { ...transfer, phase: "stopping", mbps: 0, etaSec: null };
    try {
      await api.stopDownload();
    } catch (e) {}
  }
  async function resumeTransfer() {
    if (!transfer) return;
    const id = transfer.manifest_id;
    // Always give immediate feedback so a press never feels like a no-op; the
    // real phase (downloading / waiting for peers / error) arrives via events.
    transfer = { ...transfer, phase: "starting", mbps: 0, etaSec: null };
    lastTick = null;
    if (!id) {
      transfer = { ...transfer, phase: "error: nothing to resume" };
      return;
    }
    try {
      await api.resumeDownload(id);
    } catch (e) {
      transfer = { ...transfer, phase: "error: " + e };
    }
  }

  function goTransfers() {
    tab = "transfers";
  }

  const settledPhase = (ph) =>
    !ph ||
    ["done", "paused", "stopped", "waiting"].includes(ph) ||
    String(ph).startsWith("error");

  // Tear down any in-flight transfer and wait for the engine to unwind it before
  // starting a new one, so two downloads never overlap (which can leave the next
  // pull stuck reusing a half-open peer connection).
  async function stopActiveAndSettle() {
    if (!transfer || settledPhase(transfer.phase)) return;
    try {
      await api.stopDownload();
    } catch (e) {}
    for (let i = 0; i < 25; i++) {
      if (!transfer || settledPhase(transfer.phase)) return;
      await new Promise((r) => setTimeout(r, 100));
    }
  }

  async function startMesh(m) {
    await stopActiveAndSettle();
    transfer = {
      manifest_id: m.blake3,
      artifact: m.name,
      bytes_done: 0,
      bytes_total: m.size || 0,
      phase: "starting",
      mbps: 0,
    };
    lastTick = null;
    try {
      await api.addFromMesh(m);
    } catch (e) {
      transfer = { ...transfer, phase: "error: " + e };
    }
  }

  async function startDownload(id, file) {
    await stopActiveAndSettle();
    transfer = {
      manifest_id: id,
      artifact: file || id,
      bytes_done: 0,
      bytes_total: 0,
      phase: "starting",
      mbps: 0,
    };
    lastTick = null;
    try {
      await api.download(id, file);
    } catch (e) {
      transfer = { ...transfer, phase: "error: " + e };
    }
  }

  async function dismissIntro() {
    showIntro = false;
    if (settings) {
      settings.seen_intro = true;
      try {
        await api.saveSettings(settings);
      } catch (e) {}
    }
  }

  async function startLink(link) {
    await stopActiveAndSettle();
    transfer = {
      manifest_id: link,
      artifact: "share link",
      bytes_done: 0,
      bytes_total: 0,
      phase: "starting",
      mbps: 0,
    };
    lastTick = null;
    try {
      await api.addByLink(link);
    } catch (e) {
      transfer = { ...transfer, phase: "error: " + e };
    }
  }
</script>

<svelte:window on:keydown={onKey} />

<div class="app">
  <Sidebar {tab} {transfer} {info} on:nav={(e) => (tab = e.detail)} />
  <main class="main">
    {#if tab === "discover"}
      <Discover {startDownload} {goTransfers} focus={discoverFocus} />
    {:else if tab === "explore"}
      <Explore {startLink} {startMesh} {goTransfers} />
    {:else if tab === "library"}
      <Library />
    {:else if tab === "transfers"}
      <Transfers {transfer} pause={pauseTransfer} stop={stopTransfer} resume={resumeTransfer} />
    {:else if tab === "settings"}
      <Settings {applyTheme} />
    {/if}
  </main>
</div>

{#if showIntro}
  <div class="modal-backdrop">
    <div class="modal" style="max-width:460px">
      <div class="modal-head"><h3>Welcome to Noema Studio</h3></div>
      <p class="muted">
        Verified, multi-source model downloads that dedup into one cache and can
        be shared worldwide over the mesh.
      </p>
      <p class="muted">
        Worldwide sharing is on by default — your verified, openly-licensed
        downloads are re-seeded to peers. Gated and privately-imported models
        stay private unless you opt them in.
      </p>
      <div class="actions">
        <button class="btn" on:click={() => { dismissIntro(); tab = "settings"; }}>Review sharing</button>
        <button class="btn primary" on:click={dismissIntro}>Got it</button>
      </div>
    </div>
  </div>
{/if}

{#if dropComposer}
  <ShareComposer
    initialPath={dropComposer.path}
    onClose={() => (dropComposer = null)}
    onSaved={() => (dropComposer = null)}
  />
{/if}
