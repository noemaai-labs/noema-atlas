<script>
  import { onMount } from "svelte";
  import { api, copyText } from "../api.js";
  import { fmtSize } from "../format.js";
  import ShareComposer from "../ShareComposer.svelte";

  let items = [];
  let loading = true;
  let error = "";
  let modelsDir = "";
  let composer = null;
  let confirmDelete = null;
  let toast = "";

  async function load() {
    loading = true;
    error = "";
    try {
      items = await api.library();
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
    try {
      await api.setShare(m.blake3, m.sha256, !m.shareable);
      m.shareable = !m.shareable;
      items = items;
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
    try {
      await api.reveal(m.install_path || modelsDir || "");
    } catch (e) {
      error = String(e);
    }
  }
  async function openFolder() {
    try {
      await api.reveal(modelsDir);
    } catch (e) {
      error = String(e);
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
          <div class="title">{m.name}</div>
          <div class="muted">
            {fmtSize(m.size_bytes)}{m.quant ? " · " + m.quant : ""} ·
            {m.install_path ? "installed" : "cached"}{m.license ? " · " + m.license : ""}
          </div>
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
        <button class="btn sm" on:click={() => (composer = { model: m })}>Edit</button>
        {#if m.install_path}
          <button class="btn sm" on:click={() => reveal(m)}>Reveal</button>
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
  <ShareComposer model={composer.model} onClose={() => (composer = null)} onSaved={load} />
{/if}
{#if toast}<div class="toast">{toast}</div>{/if}
