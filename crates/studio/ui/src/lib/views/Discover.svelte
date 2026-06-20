<script>
  import { onMount } from "svelte";
  import { api, copyText } from "../api.js";
  import { fmtSize } from "../format.js";
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
    startDownload(detail.id, v.file);
    started.add(vkey(v));
    started = started;
  }
  let results = [];
  let loading = false;
  let error = "";
  let openId = null;
  let detail = null;
  let detailLoading = false;
  let isHome = true;

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
  onMount(loadHome);

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
        {#if m.has_gguf}<span class="pill">gguf</span>{/if}
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
                  <span class="muted">
                    {v.format} · {fmtSize(v.size)}{v.shards > 1 ? ` · ${v.shards} shards` : ""}
                  </span>
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
