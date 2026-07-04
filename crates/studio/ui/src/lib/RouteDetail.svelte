<script>
  import { onMount } from "svelte";
  import { api, copyText } from "./api.js";
  // item: { title, subtitle, sha256, blake3, magnet, iroh, bt, hfSource }
  export let item;
  export let onClose = () => {};
  export let onGet = null;
  export let getLabel = "Download";

  // Live protocol gates from Settings: a disabled transport is shown as Off
  // rather than pretending it's a usable path.
  let btEnabled = true;
  let hfAllowed = true;
  let irohOn = true;
  onMount(async () => {
    try {
      const s = await api.getSettings();
      btEnabled = !!(s.bt_enabled && s.bt_download);
      hfAllowed = !!s.allow_hf_download;
      irohOn = !!(s.iroh_enabled && s.iroh_download);
    } catch (e) {}
  });

  $: iroh = item.iroh || 0;
  $: bt = item.bt || 0;
  $: hasMagnet = !!(item.magnet && String(item.magnet).length);
</script>

<div class="modal-backdrop" on:click|self={onClose}>
  <div class="modal" style="max-width:470px">
    <div class="modal-head">
      <h3>{item.title}</h3>
      <button class="icon-btn" on:click={onClose}>✕</button>
    </div>
    {#if item.subtitle}<p class="muted" style="margin-top:-6px">{item.subtitle}</p>{/if}

    {#if iroh + bt > 0}
      <p class="live-now">
        ● Live now: {iroh + bt}
        {iroh + bt === 1 ? "peer" : "peers"} seeding across protocols
      </p>
    {:else if item.hfSource && hfAllowed}
      <p class="muted">No P2P peers right now — Studio will fetch from Hugging Face.</p>
    {:else}
      <p class="muted">No peers are online for this file right now.</p>
    {/if}

    <p class="section" style="margin:12px 0 6px">Where you can get it</p>
    <div class="route {irohOn && iroh > 0 ? 'live' : ''}">
      <div class="grow">
        <div class="rname">Iroh</div>
        <div class="muted">
          {!irohOn
            ? "Off — enable Iroh in Settings"
            : "Worldwide P2P — NAT-traversing, downloads stripe across every peer"}
        </div>
      </div>
      <div class="rstat">{!irohOn ? "Off" : iroh + (iroh === 1 ? " peer" : " peers")}</div>
    </div>
    <div class="route {btEnabled && (bt > 0 || hasMagnet) ? 'live' : ''}">
      <div class="grow">
        <div class="rname">BitTorrent</div>
        <div class="muted">
          {!btEnabled
            ? "Off — enable BitTorrent in Settings"
            : hasMagnet || bt > 0
              ? "Swarm over DHT + µTP — a magnet is available for this file"
              : "No magnet announced for this file yet"}
        </div>
      </div>
      <div class="rstat">
        {!btEnabled
          ? "Off"
          : bt > 0
            ? bt + (bt === 1 ? " seeder" : " seeders")
            : hasMagnet
              ? "Magnet ready"
              : "—"}
      </div>
    </div>
    {#if item.hfSource}
      <div class="route {hfAllowed ? 'live' : ''}">
        <div class="grow">
          <div class="rname">Hugging Face</div>
          <div class="muted">
            {hfAllowed
              ? "Origin download, verified against the hash"
              : "Off — allow in Settings as a last-resort fallback"}
          </div>
        </div>
        <div class="rstat">{hfAllowed ? "On" : "Off"}</div>
      </div>
    {/if}
    <p class="muted" style="margin-top:6px">
      Studio tries routes in order and switches automatically if one stalls.
    </p>

    {#if item.sha256}
      <p class="muted cid">
        <span class="mono">Content ID · {item.sha256.slice(0, 20)}…</span>
        <button class="btn xs" on:click={() => copyText(item.sha256)}>Copy</button>
      </p>
    {/if}
    <div class="actions">
      {#if hasMagnet}
        <button
          class="btn"
          title="Paste into any BitTorrent client to join the swarm"
          on:click={() => copyText(item.magnet)}>Copy magnet</button
        >
      {/if}
      <div style="flex:1"></div>
      <button class="btn" on:click={onClose}>Close</button>
      {#if onGet}
        <button class="btn primary" on:click={onGet}>{getLabel}</button>
      {/if}
    </div>
  </div>
</div>
