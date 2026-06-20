<script>
  import { onMount, onDestroy } from "svelte";
  import { api } from "../api.js";
  import { fmtSize } from "../format.js";
  export let transfer = null;
  export let pause = () => {};
  export let stop = () => {};
  export let resume = () => {};

  function phaseLabel(p) {
    switch (p) {
      case "discovering peers":
        return "Waiting for peers…";
      case "waiting":
        return "Waiting for peers";
      case "pausing":
        return "Pausing…";
      case "stopping":
        return "Stopping…";
      case "paused":
        return "Paused";
      case "starting":
        return "Starting…";
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

  let health = [];
  let sharing = { sharing: false, total: 0, models: [] };
  let modelsDir = "";
  let timer = null;

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
  onDestroy(() => timer && clearInterval(timer));

  async function openFolder() {
    try {
      await api.reveal(modelsDir);
    } catch (e) {}
  }

  $: pct =
    transfer && transfer.bytes_total > 0
      ? Math.round((transfer.bytes_done / transfer.bytes_total) * 100)
      : 0;
  $: phase = transfer ? transfer.phase : "";
  $: busy = phase === "pausing" || phase === "stopping";
  $: resumable = phase === "paused" || phase === "waiting";
  $: active =
    transfer &&
    !busy &&
    !resumable &&
    phase !== "done" &&
    phase !== "stopped" &&
    !String(phase).startsWith("error");
</script>

<div class="view">
  <h2>Transfers</h2>
  <div style="display:flex; gap:8px; margin: -8px 0 16px;">
    <button class="btn sm" on:click={openFolder}>Open folder in Finder</button>
  </div>

  {#if transfer}
    <div class="card">
      <div class="card-head" style="cursor:default">
        <div class="grow">
          <div class="title">{transfer.artifact}</div>
          <div class="muted">{phaseLabel(transfer.phase)}{transfer.source ? " · " + transfer.source : ""}</div>
        </div>
        {#if active}
          <button class="btn sm" on:click={pause}>Pause</button>
          <button class="btn sm danger" on:click={stop}>Stop</button>
        {:else if busy}
          <span class="muted">{phase === "pausing" ? "Pausing…" : "Stopping…"}</span>
        {:else if resumable}
          <button class="btn sm primary" on:click={resume}>Resume</button>
        {/if}
      </div>
      <div class="bar"><div class="bar-fill" style="width:{pct}%"></div></div>
      <div class="bar-meta">
        <span>{fmtSize(transfer.bytes_done)} / {fmtSize(transfer.bytes_total)} · {pct}%</span>
        <span>
          {transfer.mbps ? transfer.mbps.toFixed(1) : "0.0"} MB/s{transfer.etaSec != null
            ? " · " + fmtEta(transfer.etaSec) + " left"
            : ""}
        </span>
      </div>
      {#if transfer.failover_reason}
        <p class="muted">Switched source · {transfer.failover_reason}</p>
      {/if}
      {#if transfer.phase === "paused"}
        <p class="muted">Paused. Resume picks up from where it left off.</p>
      {:else if transfer.phase === "waiting"}
        <p class="muted">Waiting for peers. Resume to try the available sources again.</p>
      {/if}
    </div>
  {:else}
    <p class="muted">No active transfer. Start one from Discover or Explore.</p>
  {/if}

  <div class="card" style="margin-top:12px">
    <div class="card-head" style="cursor:default">
      <div class="grow">
        <div class="title">Worldwide sharing</div>
        <div class="muted">
          {#if sharing.sharing}
            Seeding {sharing.models.length} model{sharing.models.length === 1 ? "" : "s"} · {sharing.total}
            peer{sharing.total === 1 ? "" : "s"} pulling now
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
              {#if m.uploads > 0}
                <span class="upwave" aria-hidden="true"><i></i><i></i><i></i><i></i><i></i></span>
                <span class="upcount">{m.uploads} pulling</span>
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
