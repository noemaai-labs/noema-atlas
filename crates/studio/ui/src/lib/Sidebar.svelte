<script>
  import { createEventDispatcher, onMount, onDestroy } from "svelte";
  import logo from "./logo.png?inline";
  import { api } from "./api.js";
  import { TRANSPORT_HINTS } from "./format.js";
  export let tab;
  export let transfers = {};
  export let info;
  // settings snapshot paints the chips first; polling keeps them fresh after edits.
  export let settings = null;
  const dispatch = createEventDispatcher();

  let polled = null;
  let seeding = false;
  let chipTimer = null;
  $: chipS = polled || settings;
  async function refreshChips() {
    try {
      polled = await api.getSettings();
    } catch (e) {}
    try {
      seeding = !!(await api.worldwideStatus()).sharing;
    } catch (e) {}
  }
  onMount(() => {
    refreshChips();
    chipTimer = setInterval(refreshChips, 30000);
  });
  onDestroy(() => chipTimer && clearInterval(chipTimer));
  const items = [
    { id: "discover", label: "Discover" },
    { id: "explore", label: "Explore" },
    { id: "library", label: "Library" },
    { id: "transfers", label: "Transfers" },
    { id: "settings", label: "Settings" },
  ];
  // "Active" = genuinely running (fetching bytes), not paused/terminal, so those don't inflate the badge or speed.
  const RUNNING = new Set([
    "starting",
    "queued",
    "connecting",
    "discovering peers",
    "downloading",
    "verifying",
    "seeding",
  ]);
  $: rows = Object.values(transfers);
  $: activeRows = rows.filter((t) => RUNNING.has(t.phase));
  $: active = activeRows.length > 0;
  $: totalMbps = activeRows.reduce((a, t) => a + (t.mbps || 0), 0);
</script>

<aside class="side">
  <div class="brand">
    <img class="brand-logo" src={logo} alt="Noema Atlas Studio" />
    <div>
      <div class="brand-name">Noema Atlas</div>
      <div class="brand-sub">Studio</div>
    </div>
  </div>

  <nav>
    {#each items as it}
      <button class="nav {tab === it.id ? 'on' : ''}" on:click={() => dispatch("nav", it.id)}>
        {#if it.id === "discover"}
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><circle cx="12" cy="12" r="9" /><polygon points="15.5 8.5 10.5 10.5 8.5 15.5 13.5 13.5" fill="currentColor" stroke="none" /></svg>
        {:else if it.id === "explore"}
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><circle cx="12" cy="12" r="9" /><path d="M3 12h18" /><path d="M12 3c2.6 2.6 2.6 15.4 0 18M12 3c-2.6 2.6-2.6 15.4 0 18" /></svg>
        {:else if it.id === "library"}
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><rect x="4" y="4" width="16" height="5" rx="1.5" /><rect x="4" y="11" width="16" height="5" rx="1.5" /><line x1="7" y1="18.5" x2="17" y2="18.5" /></svg>
        {:else if it.id === "transfers"}
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M8 4v14m0 0l-3-3m3 3l3-3" /><path d="M16 20V6m0 0l3 3m-3-3l-3 3" /></svg>
        {:else}
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><line x1="4" y1="8" x2="20" y2="8" /><circle cx="9" cy="8" r="2.3" fill="var(--surface)" /><line x1="4" y1="16" x2="20" y2="16" /><circle cx="15" cy="16" r="2.3" fill="var(--surface)" /></svg>
        {/if}
        {it.label}
      </button>
    {/each}
  </nav>

  <div class="side-foot">
    {#if active}
      <div class="dl">↓ {totalMbps.toFixed(1)} MB/s</div>
      <div class="muted">
        {activeRows.length} transfer{activeRows.length === 1 ? "" : "s"}…
      </div>
    {:else}
      <div class="muted">idle</div>
    {/if}
    <div class="side-chips">
      <span
        class="pill t-iroh {chipS && chipS.iroh_enabled ? (seeding ? 'hot' : '') : 'dim'}"
        title={"Iroh is " +
          (chipS && chipS.iroh_enabled ? (seeding ? "on — seeding now" : "on") : "off") +
          ". " +
          TRANSPORT_HINTS.iroh}>Iroh</span
      >
      <span
        class="pill t-bt {chipS && chipS.bt_enabled ? '' : 'dim'}"
        title={"BitTorrent is " + (chipS && chipS.bt_enabled ? "on" : "off") + ". " + TRANSPORT_HINTS.bt}
        >BT</span
      >
      <span
        class="pill t-hf {chipS && chipS.allow_hf_download ? '' : 'dim'}"
        title={"Hugging Face fallback is " +
          (chipS && chipS.allow_hf_download ? "on" : "off") +
          ". " +
          TRANSPORT_HINTS.hf}>HF</span
      >
    </div>
    {#if info.version}<div class="ver">v{info.version}</div>{/if}
  </div>
</aside>
