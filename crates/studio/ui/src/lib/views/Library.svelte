<script>
  import { onMount } from "svelte";
  import { api, copyText } from "../api.js";
  import { fmtSize, rowFormat, formatId, TRANSPORT_HINTS } from "../format.js";
  import { receipts } from "../receipts.js";
  import ShareComposer from "../ShareComposer.svelte";

  // Jump to Discover with a search prefilled (the "Update available" hop).
  export let openDiscover = () => {};

  let items = [];
  let loading = true;
  let error = "";
  let modelsDir = "";
  let composer = null;
  let confirmDelete = null;
  let confirmShare = null;
  let confirmStopShare = null;
  let toast = "";

  let scanning = false;
  let checkingUpdates = false;
  // manifest_id -> live HF repo, for rows whose pinned revision is behind.
  let updates = {};
  let runtimes = { lmstudio: false, ollama: false };
  let handing = false;

  async function scanImport() {
    if (scanning) return;
    scanning = true;
    try {
      const r = await api.scanImport();
      flash(
        r.imported
          ? `Imported ${r.imported} model${r.imported === 1 ? "" : "s"}` +
              (r.failed ? ` · ${r.failed} failed` : "")
          : r.failed
            ? `Nothing imported · ${r.failed} failed`
            : "No GGUF models found in other apps' folders"
      );
      if (r.imported) await load();
    } catch (e) {
      flash("Scan failed: " + e);
    } finally {
      scanning = false;
    }
  }

  async function checkUpdates() {
    if (checkingUpdates) return;
    checkingUpdates = true;
    try {
      const hits = await api.checkModelUpdates();
      const map = {};
      for (const h of hits) map[h.manifest_id] = h.repo;
      updates = map;
      flash(
        hits.length
          ? `${hits.length} model${hits.length === 1 ? " has" : "s have"} an update`
          : "All Hugging Face models are up to date"
      );
    } catch (e) {
      flash("Update check failed: " + e);
    } finally {
      checkingUpdates = false;
    }
  }

  const isInstalledGguf = (m) => !!m.install_path && /\.gguf$/i.test(m.install_path);
  async function handoff(m, target) {
    if (handing) return;
    handing = true;
    try {
      const msg =
        target === "ollama"
          ? await api.handoffOllama(m.install_path, m.name)
          : await api.handoffLmstudio(m.install_path, m.name);
      flash(msg);
    } catch (e) {
      flash(String(e));
    } finally {
      handing = false;
    }
  }

  async function load() {
    loading = true;
    error = "";
    try {
      items = await api.library();
      // Enrich each row with live Iroh-seeding state (best-effort per-row; a failure just leaves the pill off).
      await Promise.all(
        items.map(async (m) => {
          try {
            m.iroh_seeding = await api.isIrohSeeding(m.blake3);
          } catch (e) {
            m.iroh_seeding = false;
          }
          // Per-model seed-ratio override (null = follow the global cap).
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
    // Which local runtimes are installed — gates the per-row handoff buttons.
    api
      .runtimesPresent()
      .then((r) => (runtimes = r))
      .catch(() => {});
    await load();
  });

  function flash(msg) {
    toast = msg;
    setTimeout(() => (toast = ""), 2500);
  }

  async function toggleShare(m, e) {
    // The DOM checkbox already flipped on click; Svelte won't rewrite it while
    // `m.shareable` is unchanged, so every path that doesn't change the backend
    // state must snap it back explicitly (privacy: never show sharing ON when
    // the backend never turned it on). Captured before the awaits detach it.
    const box = e.currentTarget;
    const snapBack = () => {
      if (box) box.checked = m.shareable;
    };
    // Turning sharing ON for a gated / restrictively-licensed model needs an
    // explicit confirmation first (engine.needs_share_confirmation).
    if (!m.shareable) {
      try {
        if (await api.shareNeedsConfirmation(m.manifest_id)) {
          snapBack();
          confirmShare = m;
          return;
        }
      } catch (e2) {}
    }
    // Turning sharing OFF while peers are mid-download hard-disconnects them —
    // confirm first instead of silently cutting the cord.
    if (m.shareable) {
      try {
        const peers = await api.shareActivity(m.blake3);
        if (peers > 0) {
          snapBack();
          confirmStopShare = { model: m, peers };
          return;
        }
      } catch (e2) {}
    }
    await applyShare(m, !m.shareable);
    // No-op on success; restores the visual state if applyShare failed.
    snapBack();
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
    // The number input binds a Number — never assume a string here.
    const raw = String(m.ratio_input ?? "").trim();
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
  <div style="display:flex; gap:8px; margin: -8px 0 16px; flex-wrap: wrap;">
    <button class="btn sm" on:click={openFolder}>Open folder in Finder</button>
    <button
      class="btn sm"
      disabled={scanning}
      title="Scan LM Studio / GPT4All / llama.cpp folders for GGUF models and import them"
      on:click={scanImport}
    >
      {scanning ? "Scanning…" : "Import from other apps"}
    </button>
    <button
      class="btn sm"
      disabled={checkingUpdates}
      title="Compare each Hugging Face model against its live repo"
      on:click={checkUpdates}
    >
      {checkingUpdates ? "Checking…" : "Check for updates"}
    </button>
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
            {#if updates[m.manifest_id]}
              <button
                class="linklike"
                title="The repo has moved past your pinned revision — find the fresh version on Discover"
                on:click={() => openDiscover(updates[m.manifest_id])}
              >
                Update available ›
              </button>
            {/if}
          </div>
          {#if $receipts.get(m.manifest_id)}
            <div class="muted">{$receipts.get(m.manifest_id)}</div>
          {/if}
          <div class="muted">
            {fmtSize(m.size_bytes)}{m.quant ? " · " + m.quant : ""} ·
            {m.install_path ? "installed" : "cached"}{m.license ? " · " + m.license : ""}
          </div>
          {#if m.shareable && (m.bt_seeding || m.iroh_seeding)}
            <div class="muted pills">
              {#if m.iroh_seeding}
                <span class="pill t-iroh" title={"Seeding over Iroh. " + TRANSPORT_HINTS.iroh}>seeding · Iroh</span>
              {/if}
              {#if m.bt_seeding}
                <span class="pill t-bt" title={"Seeding over BitTorrent. " + TRANSPORT_HINTS.bt}>seeding · BitTorrent</span>
              {/if}
            </div>
          {/if}
        </div>
        <label class="switch" title="Share worldwide">
          <input type="checkbox" checked={m.shareable} on:change={(e) => toggleShare(m, e)} />
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
        {#if isInstalledGguf(m)}
          {#if runtimes.ollama}
            <button
              class="btn sm"
              disabled={handing}
              title="Register this model with Ollama (ollama create)"
              on:click={() => handoff(m, "ollama")}
            >
              Ollama
            </button>
          {/if}
          {#if runtimes.lmstudio}
            <button
              class="btn sm"
              disabled={handing}
              title="Add this model to LM Studio's models folder"
              on:click={() => handoff(m, "lmstudio")}
            >
              LM Studio
            </button>
          {/if}
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
