<script>
  import { onMount } from "svelte";
  import { api, copyText } from "../api.js";
  import { fmtSize, prettyFormat, formatId, TRANSPORT_HINTS } from "../format.js";
  import RouteDetail from "../RouteDetail.svelte";
  import Readme from "../Readme.svelte";
  export let startDownload;
  export let goTransfers = () => {};
  export let focus = 0;
  // A search another view asked for (Library's "Update available" hop); each seq bump re-runs it.
  export let presetQuery = "";
  export let presetSeq = 0;

  let searchEl;
  $: if (focus && searchEl) searchEl.focus();
  let query = "";
  let started = new Set();

  let sort = "best";
  let ggufOnly = false;
  let nextCursor = null;
  let loadingMore = false;

  let lastPresetSeq = 0;
  $: if (presetSeq && presetSeq !== lastPresetSeq) {
    lastPresetSeq = presetSeq;
    query = presetQuery;
    runQuery();
  }

  const HEADINGS = {
    best: "Trending on Hugging Face",
    trending: "Trending on Hugging Face",
    downloads: "Most downloaded on Hugging Face",
    likes: "Most liked on Hugging Face",
    updated: "Recently updated on Hugging Face",
  };

  function vkey(v) {
    return v.file || v.label;
  }
  function start(v) {
    const key = vkey(v);
    // Clear the inline "Downloading…" ack once this transfer settles (success OR
    // failure / stop / waiting), so the Download button never stays stuck.
    startDownload(
      detail.id,
      v.file,
      () => {
        started.delete(key);
        started = started;
      },
      v.is_bundle
    );
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

  // Live Atlas availability, indexed by a variant's sha256 content id: peers seeding each quant over Iroh vs BitTorrent right now.
  let meshBySha = {};
  async function loadMesh() {
    try {
      const rows = await api.mesh("");
      const m = {};
      for (const r of rows) {
        if (r.sha256) m[r.sha256] = { iroh: r.peers, bt: r.bt_seeders, magnet: r.magnet };
      }
      meshBySha = m;
    } catch (e) {
      // Non-fatal: Discover still works without the availability overlay.
    }
  }
  // Availability for a variant, or null when it isn't seeded (or has no single-file content id — sharded/bundle variants).
  function avail(v) {
    return v && v.content_id ? meshBySha[v.content_id] || null : null;
  }

  // Collapsible model card (README), fetched lazily on first expand and cached per repo@revision.
  let readmeOpen = false;
  let readmeLoading = false;
  let readmeError = "";
  let readmeBlocks = null;
  let readmeMissing = false;
  const readmeCache = new Map();
  function resetReadme() {
    readmeOpen = false;
    readmeLoading = false;
    readmeError = "";
    readmeBlocks = null;
    readmeMissing = false;
  }
  async function toggleReadme() {
    readmeOpen = !readmeOpen;
    if (!readmeOpen || readmeBlocks || readmeMissing || readmeLoading || !detail) return;
    const key = `${detail.id}@${detail.revision}`;
    if (readmeCache.has(key)) {
      const c = readmeCache.get(key);
      if (c === "missing") readmeMissing = true;
      else readmeBlocks = c;
      return;
    }
    readmeLoading = true;
    readmeError = "";
    try {
      const blocks = await api.readme(detail.id, detail.revision);
      readmeCache.set(key, blocks ?? "missing");
      // The user may have opened another card while this was in flight.
      if (!detail || `${detail.id}@${detail.revision}` !== key) return;
      if (blocks == null) readmeMissing = true;
      else readmeBlocks = blocks;
    } catch (e) {
      readmeError = String(e);
    } finally {
      readmeLoading = false;
    }
  }

  // The variant whose routes & peers popup is open.
  let detailVariant = null;
  function variantProps(v) {
    const a = avail(v) || {};
    return {
      title: (detail ? (detail.id || "").split("/").pop() + " — " : "") + v.label,
      subtitle: fmtSize(v.size) + (v.shards > 1 ? ` · ${v.shards} shards` : ""),
      sha256: v.content_id || "",
      magnet: a.magnet || "",
      iroh: a.iroh || 0,
      bt: a.bt || 0,
      hfSource: true,
    };
  }

  // No query = browse feed; "Best match" + query + no facet = Hub relevance search; else the sorted/filtered listing.
  async function runQuery() {
    const q = query.trim();
    loading = true;
    error = "";
    results = [];
    nextCursor = null;
    openId = null;
    detail = null;
    isHome = !q;
    try {
      if (q && sort === "best" && !ggufOnly) {
        results = await api.search(q);
      } else {
        const page = await api.modelList({
          search: q || null,
          sort: sort === "best" ? "trending" : sort,
          ggufOnly,
          limit: 30,
        });
        results = page.models;
        nextCursor = page.next;
      }
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }
  onMount(() => {
    // A preset hop may already have started a search before mount.
    if (!query.trim() && !loading && results.length === 0) runQuery();
    loadMesh();
  });

  async function loadMore() {
    if (!nextCursor || loadingMore) return;
    loadingMore = true;
    try {
      const page = await api.modelListPage(nextCursor);
      const seen = new Set(results.map((r) => r.id));
      results = [...results, ...page.models.filter((m) => !seen.has(m.id))];
      nextCursor = page.next;
    } catch (e) {
      error = String(e);
    } finally {
      loadingMore = false;
    }
  }

  // Community GGUF conversions of the open repo, fetched when it has no quants.
  let conversions = [];
  let conversionsLoading = false;
  async function loadConversions(id) {
    conversionsLoading = true;
    conversions = [];
    try {
      const rows = await api.modelConversions(id);
      if (openId === id) conversions = rows;
    } catch (e) {
      // Non-fatal: the section just shows "none found".
    } finally {
      conversionsLoading = false;
    }
  }
  // Open a conversion's own card: surface it under the base repo if missing, then expand it.
  function viewConversion(c) {
    if (!results.some((r) => r.id === c.id)) {
      const i = results.findIndex((r) => r.id === openId);
      results = [...results.slice(0, i + 1), c, ...results.slice(i + 1)];
    }
    toggle(c.id);
  }

  // Compact metadata line for the expanded card; omits whatever is missing.
  function metaLine(d) {
    if (!d) return "";
    const parts = [];
    if (d.params_label) parts.push(d.params_label + " params");
    if (d.context_label) parts.push(d.context_label + " context");
    if (d.architecture) parts.push(d.architecture);
    if (d.downloads) parts.push(d.downloads.toLocaleString() + " downloads");
    if (d.last_modified) parts.push("updated " + d.last_modified.slice(0, 10));
    return parts.join(" · ");
  }

  async function toggle(id) {
    resetReadme();
    conversions = [];
    conversionsLoading = false;
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
      if (openId === id && detail && !detail.has_gguf) loadConversions(id);
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
      on:keydown={(e) => e.key === "Enter" && runQuery()}
    />
    <button class="btn primary" on:click={runQuery}>Search</button>
  </div>

  <div class="disc-controls">
    <select bind:value={sort} on:change={runQuery} title="Order the results">
      <option value="best">Best match</option>
      <option value="trending">Trending</option>
      <option value="downloads">Most downloaded</option>
      <option value="likes">Most liked</option>
      <option value="updated">Recently updated</option>
    </select>
    <button
      class="pill toggle"
      class:on={ggufOnly}
      title="Only repos that publish GGUF quants — ready for local runtimes"
      on:click={() => {
        ggufOnly = !ggufOnly;
        runQuery();
      }}
    >
      GGUF only
    </button>
  </div>

  {#if loading}
    <p class="muted">{isHome ? "Loading models…" : "Searching the Hub…"}</p>
  {/if}
  {#if error}<p class="err">{error}</p>{/if}

  {#if isHome && !loading && results.length}
    <p class="muted" style="margin: 2px 0 12px; font-weight: 500;">
      {HEADINGS[sort] || HEADINGS.trending}
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
            {m.author} · {m.downloads.toLocaleString()} downloads{m.license
              ? " · " + m.license
              : ""}{m.last_modified ? " · updated " + m.last_modified.slice(0, 10) : ""}
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
            {#if metaLine(detail)}
              <p class="muted meta-line">{metaLine(detail)}</p>
            {/if}
            <div
              class="readme-head"
              role="button"
              tabindex="0"
              title="Read this model's card without leaving Atlas"
              on:click={toggleReadme}
              on:keydown={(e) =>
                (e.key === "Enter" || e.key === " ") && (e.preventDefault(), toggleReadme())}
            >
              <span class="readme-caret">{readmeOpen ? "▾" : "▸"}</span>
              <span>Model card</span>
            </div>
            {#if readmeOpen}
              <div class="readme-body">
                {#if readmeLoading}<p class="muted">Loading model card…</p>{/if}
                {#if readmeError}<p class="err">{readmeError}</p>{/if}
                {#if readmeMissing}<p class="muted">This model has no README.</p>{/if}
                {#if readmeBlocks}<Readme blocks={readmeBlocks} />{/if}
              </div>
            {/if}
            {#if !detail.has_gguf}
              <div class="conv">
                <div class="conv-title">Get this as GGUF</div>
                <p class="muted">
                  This repo has no GGUF quants; these community conversions do — ready
                  for local runtimes.
                </p>
                {#if conversionsLoading}<p class="muted">Looking for conversions…</p>{/if}
                {#each conversions as c (c.id)}
                  <div class="conv-row">
                    <span class="grow mono">{c.id}</span>
                    <span class="muted">{c.downloads.toLocaleString()} downloads</span>
                    <button class="btn xs" on:click={() => viewConversion(c)}>View</button>
                  </div>
                {/each}
                {#if !conversionsLoading && conversions.length === 0}
                  <p class="muted">No GGUF conversions found for this repo.</p>
                {/if}
              </div>
            {/if}
            {#each detail.variants as v}
              <div class="variant">
                <div
                  class="clickable"
                  role="button"
                  tabindex="0"
                  title="Show routes & peers for this quant"
                  on:click={() => (detailVariant = v)}
                  on:keydown={(e) =>
                    (e.key === "Enter" || e.key === " ") &&
                    (e.preventDefault(), (detailVariant = v))}
                >
                  <span class="vlabel">{v.label}</span>
                  {#if v.recommended}<span class="pill ok">Recommended</span>{/if}
                  {#if v.format}<span class="pill fmt f-{formatId(v.format)}">{prettyFormat(v.format)}</span>{/if}
                  {#if v.tier}<span class="pill" title={v.tier_hint || ""}>{v.tier}</span>{/if}
                  {#if v.fits === true}
                    <span class="pill ok" title="Estimated to fit this machine's memory with runtime headroom">Fits this device</span>
                  {:else if v.fits === false}
                    <span class="pill warn" title="Larger than this machine's detected memory budget">Too big for this device</span>
                  {/if}
                  <span class="muted">
                    {fmtSize(v.size)}{v.shards > 1 ? ` · ${v.shards} shards` : ""}
                  </span>
                  {#if avail(v)}
                    <span
                      class="pill t-iroh"
                      title={"Peers seeding this quant right now. " + TRANSPORT_HINTS.iroh}
                    >
                      ● Iroh {avail(v).iroh}
                    </span>
                    <span
                      class="pill t-bt"
                      title={"Seeders for this quant right now. " + TRANSPORT_HINTS.bt}
                    >
                      BitTorrent {avail(v).bt}
                    </span>
                  {/if}
                  {#if v.recommended && v.fit_reason}
                    <div class="muted">Recommended for this machine · {v.fit_reason}</div>
                  {/if}
                  {#if v.content_id}
                    <button
                      class="copyid"
                      title="Copy content id"
                      on:click|stopPropagation={() => copyText(v.content_id)}
                    >
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

  {#if nextCursor && !loading}
    <div class="load-more">
      <button class="btn sm" disabled={loadingMore} on:click={loadMore}>
        {loadingMore ? "Loading…" : "Load more"}
      </button>
    </div>
  {/if}

  {#if !loading && results.length === 0 && !error}
    <p class="muted">No models found. Try a different search.</p>
  {/if}
</div>

{#if detailVariant}
  <RouteDetail
    item={variantProps(detailVariant)}
    onClose={() => (detailVariant = null)}
    onGet={started.has(vkey(detailVariant))
      ? null
      : () => {
          const v = detailVariant;
          detailVariant = null;
          start(v);
        }}
    getLabel="Download"
  />
{/if}
