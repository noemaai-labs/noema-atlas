<script>
  import { onMount, onDestroy } from "svelte";
  import { api } from "../api.js";
  import { fmtSize, fmtRatio, rowFormat } from "../format.js";
  export let transfers = {};
  export let pause = () => {};
  export let stop = () => {};
  export let resume = () => {};
  export let remove = () => {};
  export let pauseAll = () => {};
  export let resumeAll = () => {};

  // Newest first, keyed by transfer_id for stable rendering.
  $: rows = Object.entries(transfers).map(([id, t]) => ({ id, ...t }));

  // Whether the header bulk actions are useful: at least one live (pauseable) row,
  // or at least one resumable (paused / waiting) row.
  $: anyActive = rows.some((t) => isActive(t.phase));
  $: anyResumable = rows.some((t) => isResumable(t.phase));

  // Per-transfer expandable peer list (BitTorrent). Keyed by transfer_id (== the
  // blob's blake3 for content transfers): which rows are expanded, their last
  // fetched peers, and a poll timer so an open table stays live.
  let expanded = {};
  let peers = {};
  let peerTimers = {};

  async function loadPeers(id) {
    try {
      peers[id] = await api.btPeers(id);
      peers = { ...peers };
    } catch (e) {
      peers[id] = [];
    }
  }
  function togglePeers(id) {
    if (expanded[id]) {
      delete expanded[id];
      if (peerTimers[id]) {
        clearInterval(peerTimers[id]);
        delete peerTimers[id];
      }
      expanded = { ...expanded };
    } else {
      expanded[id] = true;
      expanded = { ...expanded };
      loadPeers(id);
      peerTimers[id] = setInterval(() => loadPeers(id), 3000);
    }
  }

  // Map the engine's raw phase tokens (and the UI's own transient states) to
  // friendly labels. The engine emits lowercase strings like "downloading" /
  // "connecting" / "verifying" / "discovering peers" / "queued" via the progress
  // event; without this they'd leak verbatim into the card caption.
  function phaseLabel(p) {
    if (p == null) return "";
    if (String(p).startsWith("error")) return p; // keep the error detail
    switch (p) {
      case "queued":
        return "Queued…";
      case "connecting":
        return "Connecting…";
      case "discovering peers":
      case "waiting-for-peers":
        return "Waiting for peers…";
      case "waiting":
        return "Waiting for peers";
      case "downloading":
        return "Downloading…";
      case "verifying":
        return "Verifying…";
      case "seeding":
        return "Seeding…";
      case "pausing":
        return "Pausing…";
      case "stopping":
        return "Stopping…";
      case "paused":
        return "Paused";
      case "starting":
        return "Starting…";
      case "done":
        return "Done";
      case "stopped":
        return "Stopped";
      default:
        return p;
    }
  }

  function fmtEta(s) {
    if (s == null) return "";
    if (s < 60) return Math.round(s) + "s";
    const m = Math.floor(s / 60);
    const r = Math.round(s % 60);
    return m + "m " + (r < 10 ? "0" + r : r) + "s";
  }

  const pct = (t) =>
    t && t.bytes_total > 0 ? Math.round((t.bytes_done / t.bytes_total) * 100) : 0;
  const isBusy = (p) => p === "pausing" || p === "stopping";
  const isResumable = (p) => p === "paused" || p === "waiting";
  const isDone = (p) => p === "done" || p === "stopped" || String(p).startsWith("error");
  const isActive = (p) => !isBusy(p) && !isResumable(p) && !isDone(p);

  let health = [];
  let sharing = { sharing: false, total: 0, models: [] };
  let modelsDir = "";
  let timer = null;
  let toast = "";

  function flash(m) {
    toast = m;
    setTimeout(() => (toast = ""), 2500);
  }

  async function loadHealth() {
    try {
      health = await api.health();
    } catch (e) {}
  }
  async function loadSharing() {
    try {
      sharing = await api.uploadsList();
    } catch (e) {}
  }
  onMount(async () => {
    try {
      modelsDir = (await api.getSettings()).models_dir;
    } catch (e) {}
    loadHealth();
    loadSharing();
    timer = setInterval(loadSharing, 3000);
  });
  onDestroy(() => {
    if (timer) clearInterval(timer);
    Object.values(peerTimers).forEach((t) => clearInterval(t));
  });

  async function openFolder() {
    const path = (modelsDir || "").trim();
    if (!path) {
      flash("No models folder set — choose one in Settings");
      return;
    }
    try {
      await api.reveal(path);
    } catch (e) {
      flash("Could not open the folder — it may not exist yet");
    }
  }
</script>

<div class="view">
  <h2>Transfers</h2>
  <div style="display:flex; gap:8px; margin: -8px 0 16px;">
    <button class="btn sm" on:click={openFolder}>Open folder in Finder</button>
    <div style="flex:1"></div>
    {#if anyActive}
      <button class="btn sm" on:click={() => pauseAll()}>Pause all</button>
    {/if}
    {#if anyResumable}
      <button class="btn sm primary" on:click={() => resumeAll()}>Resume all</button>
    {/if}
  </div>

  {#if rows.length === 0}
    <p class="muted">No active transfers. Start one from Discover or Explore.</p>
  {/if}

  {#each rows as t (t.id)}
    <div class="card">
      <div class="card-head" style="cursor:default">
        <div class="grow">
          <div class="title">
            {t.artifact}
            {#if rowFormat(null, t.artifact)}<span class="pill">{rowFormat(null, t.artifact)}</span>{/if}
          </div>
          <div class="muted">
            {phaseLabel(t.phase)}{t.source ? " · " + t.source : ""}{t.peers
              ? " · " + t.peers + (t.peers === 1 ? " peer" : " peers")
              : ""}
          </div>
        </div>
        {#if isActive(t.phase)}
          <button class="btn sm" on:click={() => pause(t.id)}>Pause</button>
          <button class="btn sm danger" on:click={() => stop(t.id)}>Stop</button>
        {:else if isBusy(t.phase)}
          <span class="muted">{t.phase === "pausing" ? "Pausing…" : "Stopping…"}</span>
        {:else if isResumable(t.phase)}
          <button class="btn sm primary" on:click={() => resume(t.id)}>Resume</button>
          <button class="btn sm" on:click={() => remove(t.id)}>Remove</button>
        {:else}
          <button class="btn sm" on:click={() => remove(t.id)}>Remove</button>
        {/if}
      </div>
      <div class="bar"><div class="bar-fill" style="width:{pct(t)}%"></div></div>
      <div class="bar-meta">
        <span>{fmtSize(t.bytes_done)} / {fmtSize(t.bytes_total)} · {pct(t)}%</span>
        <span>
          {t.mbps ? t.mbps.toFixed(1) : "0.0"} MB/s{t.etaSec != null
            ? " · " + fmtEta(t.etaSec) + " left"
            : ""}
        </span>
      </div>
      {#if t.uploaded_bytes}
        <div class="bar-meta">
          <span class="muted">
            ↑ {fmtSize(t.uploaded_bytes)} seeded{fmtRatio(t.uploaded_bytes, t.bytes_done)
              ? " · ratio " + fmtRatio(t.uploaded_bytes, t.bytes_done)
              : ""}
          </span>
        </div>
      {/if}
      {#if t.failover_reason}
        <p class="muted">Switched source · {t.failover_reason}</p>
      {/if}
      {#if t.phase === "paused"}
        <p class="muted">Paused. Resume picks up from where it left off.</p>
      {:else if t.phase === "waiting"}
        <p class="muted">Waiting for peers. Resume to try the available sources again.</p>
      {/if}
      {#if t.peers || t.uploaded_bytes || expanded[t.id]}
        <button class="btn xs peers-toggle" on:click={() => togglePeers(t.id)}>
          {expanded[t.id] ? "Hide peers" : "Show peers"}
        </button>
        {#if expanded[t.id]}
          {#if peers[t.id] && peers[t.id].length}
            <table class="peers">
              <thead>
                <tr><th>Peer</th><th>Conn</th><th>Down</th><th>Up</th></tr>
              </thead>
              <tbody>
                {#each peers[t.id] as p (p.addr)}
                  <tr>
                    <td class="mono">{p.addr}</td>
                    <td>{p.conn_kind}</td>
                    <td>{fmtSize(p.downloaded)}</td>
                    <td>{fmtSize(p.uploaded)}</td>
                  </tr>
                {/each}
              </tbody>
            </table>
          {:else}
            <p class="muted">No connected BitTorrent peers right now.</p>
          {/if}
        {/if}
      {/if}
    </div>
  {/each}

  <div class="card" style="margin-top:12px">
    <div class="card-head" style="cursor:default">
      <div class="grow">
        <div class="title">Worldwide sharing</div>
        <div class="muted">
          {#if sharing.sharing}
            Seeding {sharing.models.length} model{sharing.models.length === 1 ? "" : "s"}
            {#if sharing.iroh}· {sharing.total} peer{sharing.total === 1 ? "" : "s"} pulling now{/if}
            {#if sharing.bt_seeding}· seeding over BitTorrent{/if}
          {:else}
            Not sharing
          {/if}
        </div>
      </div>
      {#if sharing.sharing}<span class="pill ok">live</span>{/if}
    </div>

    {#if sharing.sharing && sharing.models.length}
      <div class="uploads">
        {#each sharing.models as m (m.blake3)}
          <div class="uprow {m.uploads > 0 ? 'active' : ''}">
            <span class="upname mono">{m.name}</span>
            <div class="upright">
              {#if m.iroh_seeding}<span class="pill" title="Seeding over Iroh">Iroh</span>{/if}
              {#if m.bt_seeding}<span class="pill" title="Seeding over BitTorrent">BT</span>{/if}
              {#if m.uploads > 0}
                <span class="upwave" aria-hidden="true"><i></i><i></i><i></i><i></i><i></i></span>
                <span class="upcount">{m.uploads} pulling</span>
              {:else if m.bt_seeding}
                <span class="upidle">seeding (BT)</span>
              {:else}
                <span class="upidle">idle</span>
              {/if}
            </div>
          </div>
        {/each}
      </div>
    {/if}
  </div>

  <h3>Source health</h3>
  <button class="btn sm" on:click={loadHealth}>Refresh</button>
  {#if health.length === 0}<p class="muted">No source activity recorded yet.</p>{/if}
  {#each health as h}
    <div class="row">
      <span class="mono">{h.source_id}</span>
      <span class="muted">
        {h.success} ok · {h.failure} fail{h.banned ? " · banned" : ""}{h.last_latency_ms != null
          ? " · " + h.last_latency_ms + "ms"
          : ""}
      </span>
    </div>
  {/each}
</div>

{#if toast}<div class="toast">{toast}</div>{/if}
