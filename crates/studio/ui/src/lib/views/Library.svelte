<script>
  import { onMount } from "svelte";
  import { api, copyText } from "../api.js";
  import { fmtSize, rowFormat, formatId } from "../format.js";
  import ShareComposer from "../ShareComposer.svelte";

  let items = [];
  let loading = true;
  let error = "";
  let modelsDir = "";
  let composer = null;
  let confirmDelete = null;
  let confirmShare = null;
  let confirmStopShare = null;
  let toast = "";

  async function load() {
    loading = true;
    error = "";
    try {
      items = await api.library();
      // Enrich each row with its live Iroh-seeding state so the Library can show a
      // "seeding · Iroh" pill symmetric with the engine-provided `bt_seeding` one.
      // Best-effort and per-row: a failure just leaves the pill off.
      await Promise.all(
        items.map(async (m) => {
          try {
            m.iroh_seeding = await api.isIrohSeeding(m.blake3);
          } catch (e) {
            m.iroh_seeding = false;
          }
          // Per-model seed-ratio override (null = follow the global cap). Surfaced as
          // an editable number; the input mirrors the override or "" when unset.
          try {
            const r = await api.btBlobRatio(m.blake3);
            m.ratio_override = r;
            m.ratio_input = r == null ? "" : String(r);
          } catch (e) {
            m.ratio_override = null;
            m.ratio_input = "";
          }
        })
      );
      items = items;
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }
  onMount(async () => {
    try {
      modelsDir = (await api.getSettings()).models_dir;
    } catch (e) {}
    await load();
  });

  function flash(msg) {
    toast = msg;
    setTimeout(() => (toast = ""), 2500);
  }

  async function toggleShare(m) {
    // Turning sharing ON for a gated / restrictively-licensed model needs an
    // explicit confirmation first (engine.needs_share_confirmation).
    if (!m.shareable) {
      try {
        if (await api.shareNeedsConfirmation(m.manifest_id)) {
          confirmShare = m;
          // Re-render so the checkbox snaps back to off until the user confirms.
          items = items;
          return;
        }
      } catch (e) {}
    }
    // Turning sharing OFF while peers are mid-download hard-disconnects them —
    // confirm first instead of silently cutting the cord.
    if (m.shareable) {
      try {
        const peers = await api.shareActivity(m.blake3);
        if (peers > 0) {
          confirmStopShare = { model: m, peers };
          // Snap the checkbox back to on until the user confirms.
          items = items;
          return;
        }
      } catch (e) {}
    }
    await applyShare(m, !m.shareable);
  }
  async function applyShare(m, on) {
    try {
      await api.setShare(m.blake3, m.sha256, on);
      m.shareable = on;
      items = items;
    } catch (e) {
      error = String(e);
    }
  }
  async function acceptStopShare() {
    const c = confirmStopShare;
    confirmStopShare = null;
    if (!c) return;
    await applyShare(c.model, false);
    flash("Sharing stopped — peers disconnected");
  }
  async function acceptGatedShare() {
    const m = confirmShare;
    confirmShare = null;
    if (!m) return;
    try {
      await api.confirmGatedShare(m.blake3, m.sha256);
      m.shareable = true;
      items = items;
      flash("Sharing confirmed");
    } catch (e) {
      error = String(e);
    }
  }
  async function del(m) {
    try {
      const freed = await api.deleteModel(m.blake3, m.sha256);
      confirmDelete = null;
      flash(`Deleted · freed ${fmtSize(freed)}`);
      await load();
    } catch (e) {
      error = String(e);
      confirmDelete = null;
    }
  }
  async function copyLink(m) {
    try {
      const link = await api.copyShareLink(m.manifest_id);
      await copyText(link);
      flash("Share link copied");
    } catch (e) {
      error = String(e);
    }
  }
  async function copyMagnet(m) {
    try {
      const magnet = await api.btMagnet(m.blake3);
      if (!magnet) {
        flash("No magnet yet — share this model over BitTorrent first");
        return;
      }
      await copyText(magnet);
      flash("Magnet link copied");
    } catch (e) {
      error = String(e);
    }
  }
  async function applyRatio(m) {
    const raw = (m.ratio_input ?? "").trim();
    const cap = raw === "" ? null : Number(raw);
    if (cap != null && (Number.isNaN(cap) || cap < 0)) {
      flash("Ratio must be 0 or higher");
      return;
    }
    try {
      await api.setBtBlobRatio(m.blake3, cap);
      m.ratio_override = cap;
      items = items;
      flash(cap == null ? "Following the global ratio" : "Ratio override set");
    } catch (e) {
      error = String(e);
    }
  }
  async function clearRatio(m) {
    try {
      await api.setBtBlobRatio(m.blake3, null);
      m.ratio_override = null;
      m.ratio_input = "";
      items = items;
      flash("Following the global ratio");
    } catch (e) {
      error = String(e);
    }
  }
  async function recheck(m) {
    try {
      await api.btForceRecheck(m.blake3);
      flash("Rechecking pieces…");
    } catch (e) {
      flash("Recheck failed — the blob may not be a live torrent");
    }
  }
  async function install(m) {
    try {
      const dir = (modelsDir || "").replace(/\/$/, "") + "/" + m.name.replace(/[^A-Za-z0-9._-]/g, "-");
      await api.install(m.manifest_id, dir);
      flash("Installed to " + dir);
      await load();
    } catch (e) {
      error = String(e);
    }
  }
  async function reveal(m) {
    // Never hand the OS reveal an empty path (it would open the wrong place, or
    // nothing). Prefer the installed file, fall back to the models dir.
    const path = (m.install_path || modelsDir || "").trim();
    if (!path) {
      flash("No location to open — set a models folder in Settings");
      return;
    }
    try {
      await api.reveal(path);
    } catch (e) {
      flash("Could not open the folder — the file may have moved");
    }
  }
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
  <h2>Your library</h2>
  <div style="display:flex; gap:8px; margin: -8px 0 16px;">
    <button class="btn sm" on:click={openFolder}>Open folder in Finder</button>
    <button class="btn primary" on:click={() => (composer = { model: null })}>+ Share a model…</button>
  </div>

  {#if loading}<p class="muted">Loading…</p>{/if}
  {#if error}<p class="err">{error}</p>{/if}

  {#each items as m (m.manifest_id)}
    <div class="card">
      <div class="card-head" style="cursor:default">
        <div class="grow">
          <div class="title">
            {m.name}
            {#if rowFormat(m.format, m.name)}<span class="pill fmt f-{formatId(m.format, m.name)}">{rowFormat(m.format, m.name)}</span>{/if}
          </div>
          <div class="muted">
            {fmtSize(m.size_bytes)}{m.quant ? " · " + m.quant : ""} ·
            {m.install_path ? "installed" : "cached"}{m.license ? " · " + m.license : ""}
          </div>
          {#if m.shareable && (m.bt_seeding || m.iroh_seeding)}
            <div class="muted pills">
              {#if m.iroh_seeding}
                <span class="pill ok" title="Seeding over Iroh">seeding · Iroh</span>
              {/if}
              {#if m.bt_seeding}
                <span class="pill ok" title="Seeding over BitTorrent">seeding · BitTorrent</span>
              {/if}
            </div>
          {/if}
        </div>
        <label class="switch" title="Share worldwide">
          <input type="checkbox" checked={m.shareable} on:change={() => toggleShare(m)} />
          <span></span>
        </label>
      </div>

      <div class="card-actions">
        <span class="muted mono">{m.blake3.slice(0, 16)}…</span>
        <div class="spacer"></div>
        <button class="btn sm" on:click={() => copyLink(m)}>Copy link</button>
        {#if m.bt_seeding}
          <button class="btn sm" on:click={() => copyMagnet(m)} title="Copy the BitTorrent magnet for this model">Copy magnet</button>
          <button class="btn sm" on:click={() => recheck(m)} title="Force a full piece re-hash against the torrent">Recheck</button>
          <label class="ratio" title="Stop seeding this model at this ratio (0 = unlimited; clear to follow the global cap)">
            <span class="muted">Ratio</span>
            <input
              type="number"
              min="0"
              step="0.1"
              placeholder="default"
              bind:value={m.ratio_input}
              on:change={() => applyRatio(m)}
            />
            {#if m.ratio_override != null}
              <button class="btn xs" on:click={() => clearRatio(m)} title="Clear the override — follow the global ratio cap">×</button>
            {/if}
          </label>
        {/if}
        <button class="btn sm" on:click={() => (composer = { model: m })}>Edit</button>
        {#if m.install_path}
          <button class="btn sm" on:click={() => reveal(m)} title="Show the installed file in Finder">Open folder</button>
        {:else}
          <button class="btn sm" on:click={() => install(m)}>Install</button>
        {/if}
        {#if confirmDelete === m.manifest_id}
          <button class="btn sm danger" on:click={() => del(m)}>Confirm</button>
          <button class="btn sm" on:click={() => (confirmDelete = null)}>Cancel</button>
        {:else}
          <button class="btn sm danger" on:click={() => (confirmDelete = m.manifest_id)}>Delete</button>
        {/if}
      </div>
    </div>
  {/each}

  {#if !loading && items.length === 0 && !error}
    <p class="muted">
      No models yet — download one from Discover, or
      <button class="btn sm" on:click={() => (composer = { model: null })}>share a model you already have</button>.
    </p>
  {/if}
</div>

{#if composer}
  <ShareComposer
    model={composer.model}
    onClose={() => (composer = null)}
    onSaved={(msg) => {
      load();
      if (msg) flash(msg);
    }}
  />
{/if}

{#if confirmShare}
  <div class="modal-backdrop">
    <div class="modal" style="max-width:440px">
      <div class="modal-head"><h3>Share this model with peers?</h3></div>
      <p class="muted">
        <strong>{confirmShare.name}</strong> is {confirmShare.gated ? "gated" : "restrictively licensed"}.
        Sharing it re-seeds it to peers worldwide. Only do this if its license permits
        redistribution — gated and restrictive models stay private until you confirm.
      </p>
      <div class="actions">
        <button class="btn" on:click={() => (confirmShare = null)}>Cancel</button>
        <button class="btn primary" on:click={acceptGatedShare}>Share it</button>
      </div>
    </div>
  </div>
{/if}
{#if confirmStopShare}
  <div class="modal-backdrop">
    <div class="modal" style="max-width:440px">
      <div class="modal-head"><h3>Stop sharing this model?</h3></div>
      <p class="muted">
        <strong>{confirmStopShare.peers}</strong>
        {confirmStopShare.peers === 1 ? "peer is" : "peers are"} downloading
        <strong> {confirmStopShare.model.name}</strong> from you right now.
        Stopping cuts them off immediately — their downloads will fail over to
        other peers if any exist.
      </p>
      <div class="actions">
        <button class="btn" on:click={() => (confirmStopShare = null)}>Keep sharing</button>
        <button class="btn danger" on:click={acceptStopShare}>Stop &amp; disconnect</button>
      </div>
    </div>
  </div>
{/if}
{#if toast}<div class="toast">{toast}</div>{/if}
