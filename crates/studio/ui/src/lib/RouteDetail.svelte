<script>
  import { onMount } from "svelte";
  import { api, copyText } from "./api.js";
  import { TRANSPORT_HINTS } from "./format.js";
  // item: { title, subtitle, sha256, blake3, magnet, iroh, bt, hfSource }
  export let item;
  export let onClose = () => {};
  export let onGet = null;
  export let getLabel = "Download";

  // Live protocol gates from Settings: a disabled transport is shown as Off.
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
  // Per-protocol units, zero sides omitted — never a summed "N peers" claim.
  $: liveParts = [
    iroh > 0 ? `${iroh} Iroh ${iroh === 1 ? "peer" : "peers"}` : "",
    bt > 0 ? `${bt} BitTorrent ${bt === 1 ? "seeder" : "seeders"}` : "",
  ].filter(Boolean);
</script>

<div class="modal-backdrop" on:click|self={onClose}>
  <div class="modal" style="max-width:470px">
    <div class="modal-head">
      <h3>{item.title}</h3>
      <button class="icon-btn" on:click={onClose}>✕</button>
    </div>
    {#if item.subtitle}<p class="muted" style="margin-top:-6px">{item.subtitle}</p>{/if}

    {#if iroh + bt > 0}
      <p class="live-now">● Live now: {liveParts.join(" · ")}</p>
    {:else if item.hfSource && hfAllowed}
      <p class="muted">No P2P peers right now — Studio will fetch from Hugging Face.</p>
    {:else}
      <p class="muted">No peers are online for this file right now.</p>
    {/if}

    <p class="section" style="margin:12px 0 6px">Where you can get it</p>
    <div
      class="route {irohOn && iroh > 0 ? 'live' : ''}"
      title={TRANSPORT_HINTS.iroh + " NAT-traversing — no port setup needed."}
    >
      <div class="grow">
        <div class="rname t-iroh">Iroh</div>
        <div class="muted">
          {!irohOn
            ? "Off — enable Iroh in Settings"
            : "Noema's worldwide peer network — downloads pull verified pieces from every peer at once"}
        </div>
      </div>
      <div class="rstat">{!irohOn ? "Off" : iroh + (iroh === 1 ? " peer" : " peers")}</div>
    </div>
    <div
      class="route {btEnabled && (bt > 0 || hasMagnet) ? 'live' : ''}"
      title={TRANSPORT_HINTS.bt + " DHT + µTP."}
    >
      <div class="grow">
        <div class="rname t-bt">BitTorrent</div>
        <div class="muted">
          {!btEnabled
            ? "Off — enable BitTorrent in Settings"
            : hasMagnet || bt > 0
              ? "Public torrent network — a magnet link is available for this file"
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
      <div class="route {hfAllowed ? 'live' : ''}" title={TRANSPORT_HINTS.hf}>
        <div class="grow">
          <div class="rname t-hf">Hugging Face</div>
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
      <p class="muted" style="margin-top:4px">
        Every byte is verified against this hash — whoever serves it.
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
