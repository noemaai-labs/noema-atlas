<script>
  import { onMount, onDestroy } from "svelte";
  import { api } from "../api.js";
  import { fmtSize, rowFormat, formatId, TRANSPORT_HINTS } from "../format.js";
  import RouteDetail from "../RouteDetail.svelte";
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
  // The mesh row whose routes & peers popup is open.
  let detailItem = null;
  function openDetail(m) {
    detailItem = m;
  }
  function detailProps(m) {
    return {
      title: m.name,
      subtitle:
        fmtSize(m.size) +
        (m.quant ? " · " + m.quant : "") +
        (m.license ? " · " + m.license : ""),
      sha256: m.sha256,
      blake3: m.blake3,
      magnet: m.magnet,
      iroh: m.peers,
      bt: m.bt_seeders,
      hfSource: false,
    };
  }

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

  // Multi-GB downloads confirm first (name + size) unless the user opted out via
  // "Don't ask again" (settings.skip_download_confirm). RouteDetail's Get bypasses
  // this: its modal already showed name + size behind an explicit button.
  let settings = null;
  let confirmDl = null; // { kind: "mesh", m } | { kind: "link", link }
  let dontAsk = false;
  function requestGet(m) {
    if (settings && settings.skip_download_confirm) return get(m);
    dontAsk = false;
    confirmDl = { kind: "mesh", m };
  }
  function requestAdd() {
    const l = link.trim();
    if (!l) return;
    if (settings && settings.skip_download_confirm) return add();
    dontAsk = false;
    confirmDl = { kind: "link", link: l };
  }
  async function acceptConfirm() {
    const c = confirmDl;
    confirmDl = null;
    if (!c) return;
    if (dontAsk) {
      try {
        const s = settings || (await api.getSettings());
        s.skip_download_confirm = true;
        await api.saveSettings(s);
        settings = s;
      } catch (e) {}
    }
    if (c.kind === "mesh") get(c.m);
    else add();
  }

  onMount(async () => {
    search();
    timer = setInterval(() => {
      if (!loading) search();
    }, 20000);
    try {
      settings = await api.getSettings();
    } catch (e) {}
  });
  onDestroy(() => timer && clearInterval(timer));
</script>

<div class="view">
  <div style="display:flex; align-items:center; justify-content:space-between; gap:8px;">
    <h2 style="margin:0;">Explore the Atlas network</h2>
    <button class="btn sm" on:click={search} disabled={loading}>
      {loading ? "Refreshing…" : "Refresh"}
    </button>
  </div>

  <div class="search">
    <input
      bind:value={link}
      placeholder="Paste a share link — atlas1:… or atlasb1:…"
      on:keydown={(e) => e.key === "Enter" && requestAdd()}
    />
    <button class="btn primary" on:click={requestAdd}>Add</button>
  </div>
  {#if linkStarted}
    <p class="dl-ack" style="margin:-8px 0 14px;">
      <span class="upwave" aria-hidden="true"><i></i><i></i><i></i><i></i><i></i></span>
      <span class="dl-live">Searching the Atlas network…</span>
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
          <div
            class="grow clickable"
            role="button"
            tabindex="0"
            title="Show routes & peers"
            on:click={() => openDetail(m)}
            on:keydown={(e) =>
              (e.key === "Enter" || e.key === " ") && (e.preventDefault(), openDetail(m))}
          >
            <div class="title">
              {m.name}
              {#if rowFormat(null, m.name)}<span class="pill fmt f-{formatId(null, m.name)}">{rowFormat(null, m.name)}</span>{/if}
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
            <button class="btn sm primary" on:click={() => requestGet(m)}>Get</button>
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
          <div
            class="grow clickable"
            role="button"
            tabindex="0"
            title="Show routes & peers"
            on:click={() => openDetail(m)}
            on:keydown={(e) =>
              (e.key === "Enter" || e.key === " ") && (e.preventDefault(), openDetail(m))}
          >
            <div class="title">
              {m.name}
              {#if rowFormat(null, m.name)}<span class="pill fmt f-{formatId(null, m.name)}">{rowFormat(null, m.name)}</span>{/if}
            </div>
            <div class="muted">
              {fmtSize(m.size)}{m.quant ? " · " + m.quant : ""}{m.license ? " · " + m.license : ""}
              <span class="pill t-iroh" title={TRANSPORT_HINTS.iroh}>Iroh {m.peers}</span>
              <span class="pill t-bt" title={TRANSPORT_HINTS.bt}>BitTorrent {m.bt_seeders}</span>
            </div>
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
            <button class="btn sm primary" on:click={() => requestGet(m)}>Get</button>
          {/if}
        </div>
      </div>
    {/each}
  {/if}

  {#if !loading && results.length === 0 && !error}
    <p class="muted">
      Nothing shared on the Atlas network yet. Paste a share link above to fetch one,
      or turn on worldwide sharing in Settings so your models appear here.
    </p>
  {/if}
</div>

{#if confirmDl}
  <div class="modal-backdrop">
    <div class="modal" style="max-width:440px">
      <div class="modal-head"><h3>Download this model?</h3></div>
      {#if confirmDl.kind === "mesh"}
        <p class="muted">
          <strong>{confirmDl.m.name}</strong> ·
          {confirmDl.m.size ? fmtSize(confirmDl.m.size) : "size unknown"}
        </p>
      {:else}
        <p class="muted">Fetch the model behind this share link — size unknown until the manifest arrives.</p>
        <p class="mono" style="word-break:break-all; user-select:text">
          {confirmDl.link.length > 90 ? confirmDl.link.slice(0, 90) + "…" : confirmDl.link}
        </p>
      {/if}
      <label class="check">
        <input type="checkbox" bind:checked={dontAsk} />
        Don't ask again
      </label>
      <div class="actions">
        <button class="btn" on:click={() => (confirmDl = null)}>Cancel</button>
        <button class="btn primary" on:click={acceptConfirm}>Download</button>
      </div>
    </div>
  </div>
{/if}

{#if detailItem}
  <RouteDetail
    item={detailProps(detailItem)}
    onClose={() => (detailItem = null)}
    onGet={detailItem.in_library || started.has(detailItem.blake3)
      ? null
      : () => {
          const m = detailItem;
          detailItem = null;
          get(m);
        }}
    getLabel="Get"
  />
{/if}
