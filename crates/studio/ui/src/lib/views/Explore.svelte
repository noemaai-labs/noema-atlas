<script>
  import { onMount, onDestroy } from "svelte";
  import { api } from "../api.js";
  import { fmtSize, rowFormat } from "../format.js";
  export let startLink = () => {};
  export let startMesh = () => {};
  export let goTransfers = () => {};

  let link = "";
  let query = "";
  let results = [];
  let loading = false;
  let error = "";
  let timer = null;
  let started = new Set();
  let linkStarted = false;

  $: mine = results.filter((m) => m.mine);
  $: others = results.filter((m) => !m.mine);

  async function search() {
    loading = true;
    error = "";
    try {
      results = await api.mesh(query);
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  function add() {
    const l = link.trim();
    if (!l) return;
    // Clear the "Fetching…" ack once the link transfer settles (any terminal state),
    // so a failed / stopped paste doesn't leave a stuck banner.
    startLink(l, () => {
      linkStarted = false;
    });
    linkStarted = true;
  }

  function get(m) {
    const key = m.blake3;
    // Clear the inline "Downloading…" ack once this transfer settles (success OR
    // failure / stop / waiting), so the Get button never stays stuck.
    startMesh(m, () => {
      started.delete(key);
      started = started;
    });
    started.add(key);
    started = started;
  }

  onMount(() => {
    search();
    timer = setInterval(() => {
      if (!loading) search();
    }, 20000);
  });
  onDestroy(() => timer && clearInterval(timer));
</script>

<div class="view">
  <div style="display:flex; align-items:center; justify-content:space-between; gap:8px;">
    <h2 style="margin:0;">Explore the worldwide mesh</h2>
    <button class="btn sm" on:click={search} disabled={loading}>
      {loading ? "Refreshing…" : "Refresh"}
    </button>
  </div>

  <div class="search">
    <input
      bind:value={link}
      placeholder="Paste a share link — atlas1:… or atlasb1:…"
      on:keydown={(e) => e.key === "Enter" && add()}
    />
    <button class="btn primary" on:click={add}>Add</button>
  </div>
  {#if linkStarted}
    <p class="dl-ack" style="margin:-8px 0 14px;">
      <span class="upwave" aria-hidden="true"><i></i><i></i><i></i><i></i><i></i></span>
      <span class="dl-live">Fetching from the mesh…</span>
      <button class="btn sm" on:click={goTransfers}>View transfer</button>
    </p>
  {/if}

  <div class="search">
    <input
      bind:value={query}
      placeholder="Search models peers are sharing worldwide…"
      on:keydown={(e) => e.key === "Enter" && search()}
    />
    <button class="btn" on:click={search}>Search</button>
  </div>

  {#if loading && results.length === 0}<p class="muted">Querying the tracker…</p>{/if}
  {#if error}<p class="err">{error}</p>{/if}

  {#if mine.length}
    <p class="section">From your devices</p>
    {#each mine as m (m.blake3 + m.sha256)}
      <div class="card">
        <div class="card-head" style="cursor:default">
          <div class="grow">
            <div class="title">
              {m.name}
              {#if rowFormat(null, m.name)}<span class="pill">{rowFormat(null, m.name)}</span>{/if}
            </div>
            <div class="muted">{fmtSize(m.size)}{m.quant ? " · " + m.quant : ""}{m.devices && m.devices.length ? " · " + m.devices.join(", ") : ""}</div>
          </div>
          {#if m.in_library}
            <span class="pill">in library</span>
          {:else if started.has(m.blake3)}
            <div class="dl-ack">
              <span class="upwave" aria-hidden="true"><i></i><i></i><i></i><i></i><i></i></span>
              <span class="dl-live">Downloading…</span>
              <button class="btn sm" on:click={goTransfers}>View transfer</button>
            </div>
          {:else}
            <button class="btn sm primary" on:click={() => get(m)}>Get</button>
          {/if}
        </div>
      </div>
    {/each}
  {/if}

  {#if others.length}
    <p class="section">Shared on the network</p>
    {#each others as m (m.blake3 + m.sha256)}
      <div class="card">
        <div class="card-head" style="cursor:default">
          <div class="grow">
            <div class="title">
              {m.name}
              {#if rowFormat(null, m.name)}<span class="pill">{rowFormat(null, m.name)}</span>{/if}
            </div>
            <div class="muted">{fmtSize(m.size)}{m.quant ? " · " + m.quant : ""}{m.license ? " · " + m.license : ""} · {m.peers} {m.peers === 1 ? "peer" : "peers"}</div>
          </div>
          {#if m.in_library}
            <span class="pill">in library</span>
          {:else if started.has(m.blake3)}
            <div class="dl-ack">
              <span class="upwave" aria-hidden="true"><i></i><i></i><i></i><i></i><i></i></span>
              <span class="dl-live">Downloading…</span>
              <button class="btn sm" on:click={goTransfers}>View transfer</button>
            </div>
          {:else}
            <button class="btn sm primary" on:click={() => get(m)}>Get</button>
          {/if}
        </div>
      </div>
    {/each}
  {/if}

  {#if !loading && results.length === 0 && !error}
    <p class="muted">
      No models on the mesh yet. Paste a share link above to fetch one, or turn on
      worldwide sharing in Settings so your models appear here.
    </p>
  {/if}
</div>
