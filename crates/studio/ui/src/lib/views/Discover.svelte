<script>
  import { onMount } from "svelte";
  import { api, copyText } from "../api.js";
  import { fmtSize, prettyFormat, formatId } from "../format.js";
  export let startDownload;
  export let goTransfers = () => {};
  export let focus = 0;

  let searchEl;
  $: if (focus && searchEl) searchEl.focus();
  let query = "";
  let started = new Set();

  function vkey(v) {
    return v.file || v.label;
  }
  function start(v) {
    const key = vkey(v);
    // Clear the inline "Downloading…" ack once this transfer settles (success OR
    // failure / stop / waiting), so the Download button never stays stuck.
    startDownload(detail.id, v.file, () => {
      started.delete(key);
      started = started;
    });
    started.add(key);
    started = started;
  }
  let results = [];
  let loading = false;
  let error = "";
  let openId = null;
  let detail = null;
  let detailLoading = false;
  let isHome = true;

  // Live Atlas availability, indexed by a variant's sha256 content id: how many
  // peers are seeding each quant over Iroh vs BitTorrent right now. This is what
  // makes Discover more than a Hugging Face mirror — it overlays the real mesh.
  let meshBySha = {};
  async function loadMesh() {
    try {
      const rows = await api.mesh("");
      const m = {};
      for (const r of rows) {
        if (r.sha256) m[r.sha256] = { iroh: r.peers, bt: r.bt_seeders };
      }
      meshBySha = m;
    } catch (e) {
      // Non-fatal: Discover still works without the availability overlay.
    }
  }
  // Availability for a variant, or null when it isn't seeded on the mesh (or has no
  // single-file content id to match — sharded/bundle variants).
  function avail(v) {
    return v && v.content_id ? meshBySha[v.content_id] || null : null;
  }

  async function loadHome() {
    loading = true;
    error = "";
    openId = null;
    detail = null;
    try {
      results = await api.popular();
      isHome = true;
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }
  onMount(() => {
    loadHome();
    loadMesh();
  });

  async function search() {
    if (!query.trim()) {
      loadHome();
      return;
    }
    loading = true;
    error = "";
    results = [];
    openId = null;
    detail = null;
    isHome = false;
    try {
      results = await api.search(query);
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  async function toggle(id) {
    if (openId === id) {
      openId = null;
      detail = null;
      return;
    }
    openId = id;
    detail = null;
    detailLoading = true;
    try {
      detail = await api.modelDetail(id);
    } catch (e) {
      error = String(e);
    } finally {
      detailLoading = false;
    }
  }
</script>

<div class="view">
  <div class="search">
    <input
      bind:this={searchEl}
      bind:value={query}
      placeholder="Search Hugging Face — mistral, llama, qwen…"
      on:keydown={(e) => e.key === "Enter" && search()}
    />
    <button class="btn primary" on:click={search}>Search</button>
  </div>

  {#if loading}
    <p class="muted">{isHome ? "Loading popular models…" : "Searching the Hub…"}</p>
  {/if}
  {#if error}<p class="err">{error}</p>{/if}

  {#if isHome && !loading && results.length}
    <p class="muted" style="margin: 2px 0 12px; font-weight: 500;">
      Most downloaded on Hugging Face
    </p>
  {/if}

  {#each results as m (m.id)}
    <div class="card">
      <div
        class="card-head"
        role="button"
        tabindex="0"
        on:click={() => toggle(m.id)}
        on:keydown={(e) =>
          (e.key === "Enter" || e.key === " ") && (e.preventDefault(), toggle(m.id))}
      >
        <div class="avatar">{m.author.slice(0, 2).toUpperCase()}</div>
        <div class="grow">
          <div class="title">{m.name}</div>
          <div class="muted">
            {m.author} · {m.downloads.toLocaleString()} downloads{m.license ? " · " + m.license : ""}
          </div>
        </div>
        {#if m.gated}<span class="pill warn">gated</span>{/if}
        {#each m.formats || [] as f}
          <span
            class="pill fmt f-{f}"
            title="This repo publishes weights in {prettyFormat(f)} format"
          >
            {prettyFormat(f)}
          </span>
        {/each}
      </div>

      {#if openId === m.id}
        <div class="detail">
          {#if detailLoading}<p class="muted">Loading variants…</p>{/if}
          {#if detail}
            {#each detail.variants as v}
              <div class="variant">
                <div>
                  <span class="vlabel">{v.label}</span>
                  {#if v.recommended}<span class="pill ok">Recommended</span>{/if}
                  {#if v.format}<span class="pill fmt f-{formatId(v.format)}">{prettyFormat(v.format)}</span>{/if}
                  <span class="muted">
                    {fmtSize(v.size)}{v.shards > 1 ? ` · ${v.shards} shards` : ""}
                  </span>
                  {#if avail(v)}
                    <span class="pill ok" title="Peers seeding this quant on the Atlas network right now">
                      ● Iroh {avail(v).iroh} · BitTorrent {avail(v).bt}
                    </span>
                  {/if}
                  {#if v.recommended && v.fit_reason}
                    <div class="muted">Recommended for this machine · {v.fit_reason}</div>
                  {/if}
                  {#if v.content_id}
                    <button class="copyid" title="Copy content id" on:click={() => copyText(v.content_id)}>
                      {v.content_id.slice(0, 16)}…
                    </button>
                  {/if}
                </div>
                {#if started.has(vkey(v))}
                  <div class="dl-ack">
                    <span class="upwave" aria-hidden="true"><i></i><i></i><i></i><i></i><i></i></span>
                    <span class="dl-live">Downloading…</span>
                    <button class="btn sm" on:click={goTransfers}>View transfer</button>
                  </div>
                {:else}
                  <button class="btn primary" on:click={() => start(v)}>Download</button>
                {/if}
              </div>
            {/each}
            {#if detail.variants.length === 0}
              <p class="muted">No downloadable weights in this repo.</p>
            {/if}
          {/if}
        </div>
      {/if}
    </div>
  {/each}

  {#if !loading && results.length === 0 && !error}
    <p class="muted">No models found. Try a different search.</p>
  {/if}
</div>
