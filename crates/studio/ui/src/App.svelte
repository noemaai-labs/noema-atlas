<script>
  import { onMount } from "svelte";
  import { api, onProgress, onDone, onRegistered } from "./lib/api.js";
  import { routeClassOf } from "./lib/format.js";
  import { buildReceipt, setReceipt } from "./lib/receipts.js";
  import { check as checkUpdate } from "@tauri-apps/plugin-updater";
  import { relaunch } from "@tauri-apps/plugin-process";
  import Sidebar from "./lib/Sidebar.svelte";
  import Discover from "./lib/views/Discover.svelte";
  import Explore from "./lib/views/Explore.svelte";
  import Library from "./lib/views/Library.svelte";
  import Transfers from "./lib/views/Transfers.svelte";
  import Settings from "./lib/views/Settings.svelte";
  import ShareComposer from "./lib/ShareComposer.svelte";

  let tab = "discover";
  let info = { name: "Noema Studio", version: "", root: "" };
  // Concurrent transfers, keyed by transfer_id (equal to the manifest id).
  let transfers = {};
  let settings = null;
  let showIntro = false;

  // Auto-update (Tauri updater). `updateInfo` is the plugin's Update handle when a
  // newer signed build is available; the banner drives download + relaunch.
  let updateInfo = null;
  let updateBusy = false;
  let updateProgress = 0;
  let updateDismissed = false;
  let updateError = "";
  let discoverFocus = 0;
  let dropComposer = null;

  // A search another view asked Discover to run (e.g. Library's "Update
  // available" hop to the model's repo). The seq bump makes each hop distinct.
  let discoverQuery = "";
  let discoverSeq = 0;
  function openDiscoverSearch(q) {
    discoverQuery = q || "";
    discoverSeq++;
    tab = "discover";
  }

  // Provisional rows created before the engine assigns the real transfer_id; each
  // start passes its temp key as `client_ref` and `download://registered` echoes it
  // back with the real id, so we re-key that exact row.
  let tmpSeq = 0;
  // Per-transfer settle callbacks: fire when a transfer reaches any terminal state
  // (done/stopped/error/waiting), not just success. Keyed like `transfers`, so they
  // follow the row through the tmp→real re-key.
  let settleCbs = {};

  function applyTheme(theme) {
    const resolved =
      theme === "system"
        ? window.matchMedia("(prefers-color-scheme: dark)").matches
          ? "dark"
          : "light"
        : theme;
    document.documentElement.setAttribute("data-theme", resolved);
  }

  const settledPhase = (ph) =>
    !ph ||
    ["done", "paused", "stopped", "waiting"].includes(ph) ||
    String(ph).startsWith("error");

  // Phases a single-row Resume is allowed from: a settled, non-running card. error is
  // included so a failed card can be retried individually.
  const isResumablePhase = (ph) =>
    ph === "paused" || ph === "waiting" || String(ph).startsWith("error");

  // Phases Resume-all acts on: paused or waiting only. Failed/errored rows are
  // excluded — a bulk resume doesn't retry failures.
  const isResumeAllPhase = (ph) => ph === "paused" || ph === "waiting";

  // A live, pauseable transfer: mid-flight, not already settling or settled.
  const isActivePhase = (ph) =>
    !settledPhase(ph) && ph !== "pausing" && ph !== "stopping";

  // Map a backend lifecycle state (TransferState's Debug string) to the UI phase a
  // reconciled card shows. Terminal states return null so a finished/failed transfer
  // never resurrects as a live card after a reload.
  function phaseFromState(state) {
    switch (state) {
      case "Paused":
        return "paused";
      case "WaitingForPeers":
        return "waiting";
      case "Queued":
        return "queued";
      case "Connecting":
        return "connecting";
      case "Verifying":
        return "verifying";
      case "Seeding":
        return "seeding";
      case "Downloading":
        return "downloading";
      // Failed / Stopped / Complete (and any unknown) are terminal — skip them so
      // they don't reappear as live rows.
      default:
        return null;
    }
  }

  // Re-key the provisional row `clientRef` to its real transfer_id, driven by the
  // engine's `download://registered` event; matched by the exact temp key it was
  // created with, so concurrent starts never get misattributed.
  function adopt(clientRef, realId) {
    if (clientRef == null || !transfers[clientRef]) return false;
    if (clientRef === realId) return true;
    const row = transfers[clientRef];
    delete transfers[clientRef];
    transfers[realId] = { ...row, manifest_id: realId };
    transfers = { ...transfers };
    // Carry the launching view's settle-callback across the re-key so it still
    // fires when this (now correctly-keyed) transfer settles.
    if (settleCbs[clientRef]) {
      settleCbs[realId] = settleCbs[clientRef];
      delete settleCbs[clientRef];
    }
    return true;
  }

  // Fire and drop the settle-callback for a transfer id, clearing the launching
  // view's inline "Downloading…" acknowledgement. Idempotent.
  function settle(id) {
    const cb = settleCbs[id];
    if (cb) {
      delete settleCbs[id];
      try {
        cb();
      } catch (e) {}
    }
  }

  // Ask the VPS (via the Tauri updater) whether a newer signed build exists. Returns
  // "available" | "current" | "error:<msg>" so the Settings "Check now" button can
  // give feedback. Best-effort: a failure never blocks the app.
  async function checkForUpdates() {
    try {
      const upd = await checkUpdate();
      if (upd) {
        updateInfo = upd;
        updateDismissed = false;
        return "available";
      }
      return "current";
    } catch (e) {
      return "error:" + e;
    }
  }

  async function installUpdate() {
    if (!updateInfo || updateBusy) return;
    updateBusy = true;
    updateError = "";
    updateProgress = 0;
    let total = 0;
    let done = 0;
    try {
      await updateInfo.downloadAndInstall((ev) => {
        if (ev.event === "Started") total = ev.data?.contentLength || 0;
        else if (ev.event === "Progress") {
          done += ev.data?.chunkLength || 0;
          updateProgress = total ? done / total : 0;
        } else if (ev.event === "Finished") updateProgress = 1;
      });
      // The new version is installed; relaunch into it.
      await relaunch();
    } catch (e) {
      // Keep updateInfo so the banner's Install button can retry the download.
      updateError = "Update failed: " + e;
    } finally {
      updateBusy = false;
    }
  }

  onMount(async () => {
    try {
      info = await api.appInfo();
    } catch (e) {}
    try {
      settings = await api.getSettings();
      applyTheme(settings.theme);
      showIntro = !settings.seen_intro;
      if (settings.auto_update) checkForUpdates();
    } catch (e) {
      applyTheme("system");
    }

    // Re-key the right provisional row the moment the engine registers the id —
    // before any progress arrives, so the card never flickers under a temp key.
    onRegistered((r) => {
      if (r && r.client_ref && r.transfer_id) adopt(r.client_ref, r.transfer_id);
    });

    onProgress((p) => {
      let id = p.transfer_id || p.manifest_id;
      // The provisional row was already re-keyed by `onRegistered`; a still-unknown
      // id here means a transfer we never created a card for (e.g. resumed in
      // another view), so start a fresh row.
      if (!transfers[id]) transfers[id] = { manifest_id: id };
      const row = transfers[id];
      // Once a pause/stop is initiated (or settled), ignore late in-flight ticks
      // so the card doesn't flicker back to "downloading" before the done event.
      if (["pausing", "stopping", "paused", "waiting"].includes(row.phase)) {
        transfers = { ...transfers };
        return;
      }
      const now = performance.now();
      let mbps = row.mbps ?? 0;
      if (row.lastTick && p.bytes_done >= row.lastTick.bytes) {
        const dt = (now - row.lastTick.t) / 1000;
        if (dt > 0.25) {
          mbps = (p.bytes_done - row.lastTick.bytes) / dt / 1e6;
          row.lastTick = { t: now, bytes: p.bytes_done };
        }
      } else {
        row.lastTick = { t: now, bytes: p.bytes_done };
      }
      // Attribute this tick's byte delta to the active route: feeds the per-path
      // rate next to the Paths pills and the "fetched from …" completion receipt.
      // `bytes_done` is cumulative across sources; only forward motion counts.
      const cls = routeClassOf(p.source);
      const wall = Date.now();
      if (cls && row.prevDone != null && p.bytes_done > row.prevDone) {
        const delta = p.bytes_done - row.prevDone;
        row.routeBytes = { ...(row.routeBytes || {}) };
        row.routeBytes[cls] = (row.routeBytes[cls] || 0) + delta;
        const r = { ...((row.routeRates || {})[cls] || { bps: 0, win: 0, winStart: wall }) };
        r.win += delta;
        if (wall - r.winStart >= 500) {
          const inst = (r.win * 1000) / (wall - r.winStart);
          r.bps = r.bps > 0 ? r.bps * 0.7 + inst * 0.3 : inst;
          r.win = 0;
          r.winStart = wall;
        }
        r.at = wall;
        row.routeRates = { ...(row.routeRates || {}), [cls]: r };
      }
      row.prevDone = p.bytes_done;
      const remaining = p.bytes_total - p.bytes_done;
      const etaSec = mbps > 0 && remaining > 0 ? Math.round(remaining / (mbps * 1e6)) : null;
      transfers[id] = { ...row, ...p, manifest_id: id, mbps, etaSec, lastTick: row.lastTick };
      transfers = { ...transfers };
    });

    onDone((payload) => {
      const status = payload && typeof payload === "object" ? payload.status : "done";
      const id = (payload && (payload.transfer_id || payload.manifest_id)) || null;
      // Clear the launching view's inline ack on ANY terminal status, not just
      // success. Done before the row guard below so it fires even if the row was
      // already pruned.
      if (id != null) settle(id);
      // The row is keyed by the engine id (re-keyed at `onRegistered`), so the done
      // event's id matches directly — no FIFO fallback.
      if (id == null || !transfers[id]) return;
      const row = transfers[id];
      if (status === "stopped") {
        delete transfers[id];
      } else if (status === "paused") {
        transfers[id] = { ...row, manifest_id: id, phase: "paused", mbps: 0, etaSec: null };
      } else if (status === "waiting") {
        transfers[id] = { ...row, manifest_id: id, phase: "waiting", mbps: 0, etaSec: null };
      } else if (status === "error") {
        const msg = (payload && payload.message) || "failed";
        transfers[id] = { ...row, phase: "error: " + msg, mbps: 0, etaSec: null };
      } else {
        // Record where the verified bytes came from, for this session's receipt
        // lines (Transfers done-card + Library row).
        setReceipt(id, buildReceipt(row.routeBytes));
        transfers[id] = { ...row, phase: "done", mbps: 0, etaSec: null };
        // Clear a finished row after a moment so it doesn't linger forever — unless
        // the user is still looking at it / it changed in the meantime.
        setTimeout(() => {
          if (transfers[id] && transfers[id].phase === "done") {
            delete transfers[id];
            transfers = { ...transfers };
          }
        }, 4000);
      }
      transfers = { ...transfers };
    });

    // Reconcile with the engine's live transfer registry on launch (e.g. a window
    // reload that lost the in-memory map but left downloads running).
    try {
      const live = await api.listTransfers();
      for (const t of live) {
        const phase = phaseFromState(t.state);
        // Skip terminal rows (Failed / Complete / Stopped) so they don't come
        // back as phantom downloads; only seed cards for live/resumable states.
        if (phase && !transfers[t.transfer_id]) {
          transfers[t.transfer_id] = {
            manifest_id: t.transfer_id,
            artifact: t.transfer_id,
            phase,
            bytes_done: 0,
            bytes_total: 0,
            mbps: 0,
          };
        }
      }
      transfers = { ...transfers };
    } catch (e) {}

    // After a full restart the live registry is empty, but interrupted downloads
    // survive on disk (kept `.part` + manifest). Re-offer them as Paused cards so
    // the user can resume.
    try {
      const resumable = await api.resumableDownloads();
      for (const r of resumable) {
        if (!transfers[r.transfer_id]) {
          transfers[r.transfer_id] = {
            manifest_id: r.transfer_id,
            artifact: r.artifact || r.transfer_id,
            phase: "paused",
            bytes_done: r.bytes_done || 0,
            bytes_total: r.bytes_total || 0,
            mbps: 0,
          };
        }
      }
      transfers = { ...transfers };
    } catch (e) {}

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
    } catch (e) {}
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

  // Per-row pause/stop/resume/remove, keyed by transfer_id.
  async function pauseTransfer(id) {
    if (transfers[id])
      transfers[id] = { ...transfers[id], phase: "pausing", mbps: 0, etaSec: null };
    transfers = { ...transfers };
    try {
      await api.pauseDownload(id);
    } catch (e) {}
  }
  async function stopTransfer(id) {
    if (transfers[id])
      transfers[id] = { ...transfers[id], phase: "stopping", mbps: 0, etaSec: null };
    transfers = { ...transfers };
    try {
      await api.stopDownload(id);
    } catch (e) {}
  }
  async function resumeTransfer(id) {
    const row = transfers[id];
    if (!row) return;
    // Guard against a double-press: once the row is starting / running, a second
    // resume would hit the engine's already-running guard and flip the live card
    // to a scary "transfer is already running" error. Only resume a settled row.
    const ph = row.phase || "";
    if (!isResumablePhase(ph)) return;
    // Always give immediate feedback so a press never feels like a no-op; the real
    // phase (downloading / waiting for peers / error) arrives via events.
    transfers[id] = { ...row, phase: "starting", mbps: 0, etaSec: null, lastTick: null };
    transfers = { ...transfers };
    if (!id) {
      transfers[id] = { ...transfers[id], phase: "error: nothing to resume" };
      transfers = { ...transfers };
      return;
    }
    try {
      await api.resumeDownload(id);
    } catch (e) {
      transfers[id] = { ...transfers[id], phase: "error: " + e };
      transfers = { ...transfers };
    }
  }
  async function removeTransfer(id) {
    // A paused/waiting/errored row still has a kept `.part` + resumable DB row on
    // disk; removing it must discard those (else they leak). A done/stopped row was
    // already cleaned up, so a registry-only forget is enough.
    const phase = (transfers[id] && transfers[id].phase) || "";
    const hasPartial =
      phase === "paused" ||
      phase === "waiting" ||
      phase === "failed" ||
      phase.startsWith("error");
    delete transfers[id];
    transfers = { ...transfers };
    try {
      if (hasPartial) await api.discardTransfer(id);
      else await api.removeTransfer(id);
    } catch (e) {}
  }

  // Pause / resume every transfer at once (Transfers header actions). Pause-all
  // optimistically flips each live row to "pausing"; the per-transfer done events
  // settle them.
  async function pauseAll() {
    for (const id of Object.keys(transfers)) {
      const ph = (transfers[id] && transfers[id].phase) || "";
      if (isActivePhase(ph))
        transfers[id] = { ...transfers[id], phase: "pausing", mbps: 0, etaSec: null };
    }
    transfers = { ...transfers };
    try {
      await api.pauseAll();
    } catch (e) {}
  }
  async function resumeAll() {
    // Snapshot only the rows the UI shows as resumable (paused / waiting) and drive
    // each through the same per-id resume path as the row's own Resume button, so a
    // bulk resume never re-drives a failed download or an orphaned backend row the
    // user can't see.
    const ids = Object.keys(transfers).filter((id) =>
      isResumeAllPhase((transfers[id] && transfers[id].phase) || "")
    );
    for (const id of ids) {
      resumeTransfer(id);
    }
  }

  function goTransfers() {
    tab = "transfers";
  }

  // Create an optimistic provisional row under a unique temp key (passed to the
  // backend as `client_ref`) and return it. Optional `onSettle` fires once the
  // transfer reaches a terminal state.
  function beginProvisional(seed, onSettle) {
    const tmp = "tmp_" + tmpSeq++;
    transfers[tmp] = { manifest_id: tmp, mbps: 0, lastTick: null, ...seed };
    if (typeof onSettle === "function") settleCbs[tmp] = onSettle;
    transfers = { ...transfers };
    return tmp;
  }

  // Tag a provisional row as errored — but only if it hasn't already been re-keyed
  // to its real id (in which case the `done` event carries the failure instead).
  // Either way the launching view's ack must clear, so settle the ref too.
  function failProvisional(ref, e) {
    if (ref && transfers[ref]) {
      transfers[ref] = { ...transfers[ref], phase: "error: " + e };
      transfers = { ...transfers };
    }
    if (ref) settle(ref);
  }

  async function startMesh(m, onSettle) {
    const ref = beginProvisional(
      {
        artifact: m.name,
        bytes_done: 0,
        bytes_total: m.size || 0,
        phase: "starting",
      },
      onSettle
    );
    try {
      await api.addFromMesh(m, ref);
    } catch (e) {
      failProvisional(ref, e);
    }
  }

  async function startDownload(id, file, onSettle, bundle = false) {
    const ref = beginProvisional(
      {
        artifact: file || id,
        bytes_done: 0,
        bytes_total: 0,
        phase: "starting",
      },
      onSettle
    );
    try {
      await api.download(id, file, ref, bundle);
    } catch (e) {
      failProvisional(ref, e);
    }
  }

  async function startLink(link, onSettle) {
    const ref = beginProvisional(
      {
        artifact: "share link",
        bytes_done: 0,
        bytes_total: 0,
        phase: "starting",
      },
      onSettle
    );
    try {
      await api.addByLink(link, ref);
    } catch (e) {
      failProvisional(ref, e);
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
</script>

<svelte:window on:keydown={onKey} />

<div class="app">
  <Sidebar {tab} {transfers} {info} {settings} on:nav={(e) => (tab = e.detail)} />
  <main class="main">
    {#if (updateInfo && !updateDismissed) || updateError}
      <div class="update-banner">
        {#if updateError}
          <span>{updateError}</span>
          <button class="btn sm" on:click={() => (updateError = "")}>Dismiss</button>
        {:else}
          <span><strong>Studio {updateInfo.version}</strong> is available</span>
          {#if updateBusy}
            <span class="muted">Installing… {Math.round(updateProgress * 100)}%</span>
          {:else}
            <button class="btn sm primary" on:click={installUpdate}>Install &amp; restart</button>
            <button class="btn sm" on:click={() => (updateDismissed = true)}>Dismiss</button>
          {/if}
        {/if}
      </div>
    {/if}
    {#if tab === "discover"}
      <Discover
        {startDownload}
        {goTransfers}
        focus={discoverFocus}
        presetQuery={discoverQuery}
        presetSeq={discoverSeq}
      />
    {:else if tab === "explore"}
      <Explore {startLink} {startMesh} {goTransfers} />
    {:else if tab === "library"}
      <Library openDiscover={openDiscoverSearch} />
    {:else if tab === "transfers"}
      <Transfers
        {transfers}
        pause={pauseTransfer}
        stop={stopTransfer}
        resume={resumeTransfer}
        remove={removeTransfer}
        {pauseAll}
        {resumeAll}
      />
    {:else if tab === "settings"}
      <Settings {applyTheme} checkUpdates={checkForUpdates} />
    {/if}
  </main>
</div>

{#if showIntro}
  <div class="modal-backdrop">
    <div class="modal" style="max-width:460px">
      <div class="modal-head"><h3>Welcome to Noema Studio</h3></div>
      <p class="muted">
        Verified, multi-source model downloads that dedup into one cache and can be
        shared worldwide over Iroh.
      </p>
      <p class="muted">
        Sharing is on by default — your verified, openly-licensed downloads are
        re-seeded to peers over BitTorrent and Iroh, Noema's worldwide peer
        network. BitTorrent binds
        local ports for inbound peers; reachability isn't guaranteed behind every
        NAT, so we can't promise a relay. Gated and privately-imported models stay
        private unless you opt them in.
      </p>
      <p class="muted">
        Privacy note: BitTorrent announces your IP address and the model's info-hash
        to the DHT and public trackers, so peers can see your IP is downloading or
        sharing that file. A SOCKS5 proxy routes this through the proxy; any other
        proxy still exposes your real IP. Tune trackers and proxy under Settings.
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
