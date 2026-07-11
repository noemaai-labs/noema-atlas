<script>
  import { onMount } from "svelte";
  import { api } from "../api.js";
  import { fmtSize } from "../format.js";
  export let applyTheme;
  // Provided by App.svelte: runs the Tauri updater check, returns
  // "available" | "current" | "error:<msg>".
  export let checkUpdates = async () => "current";

  let updChecking = false;
  async function onAutoUpdateToggle() {
    await save();
    // Enabling it mid-session should check right away, not wait for next launch.
    if (s.auto_update) checkNow();
  }
  async function checkNow() {
    updChecking = true;
    try {
      const r = await checkUpdates();
      if (r === "available") flash("Update available — see the banner at the top.");
      else if (r === "current") flash("You're on the latest version.");
      else flash("Update check failed: " + r.replace(/^error:/, ""));
    } finally {
      updChecking = false;
    }
  }

  let s = null;
  let saving = false;
  let saved = false;
  let error = "";
  let toast = "";

  let hasToken = false;
  let tokenInput = "";
  let sharing = false;
  let cacheBytes = 0;
  let cacheCount = 0;

  function flash(m) {
    toast = m;
    setTimeout(() => (toast = ""), 2500);
  }

  // Bandwidth schedule helpers. Time inputs are "HH:MM"; the engine stores minutes
  // since local midnight. Days are a bitmask (bit 0 = Mon … bit 6 = Sun).
  const DAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
  function minToTime(min) {
    const m = ((Number(min) || 0) % 1440 + 1440) % 1440;
    const h = Math.floor(m / 60);
    const r = m % 60;
    return String(h).padStart(2, "0") + ":" + String(r).padStart(2, "0");
  }
  function timeToMin(v) {
    const [h, m] = String(v || "0:0").split(":").map((x) => parseInt(x, 10) || 0);
    return (h * 60 + m) % 1440;
  }
  function hasDay(i) {
    return (s.bt_schedule_days & (1 << i)) !== 0;
  }
  function toggleDay(i) {
    s.bt_schedule_days ^= 1 << i;
    saveSchedule();
  }
  function randomPort() {
    s.bt_port = 1024 + Math.floor(Math.random() * 64512);
    save();
  }
  async function saveSchedule() {
    try {
      await api.saveSettings(s);
    } catch (e) {
      error = String(e);
    }
  }

  onMount(async () => {
    try {
      s = await api.getSettings();
    } catch (e) {
      error = String(e);
    }
    try {
      hasToken = await api.tokenStatus();
    } catch (e) {}
    try {
      sharing = (await api.worldwideStatus()).sharing;
    } catch (e) {}
    try {
      const c = await api.cache();
      cacheCount = c.length;
      cacheBytes = c.reduce((a, b) => a + (b.size_bytes || 0), 0);
    } catch (e) {}
  });

  async function save() {
    saving = true;
    saved = false;
    error = "";
    try {
      await api.saveSettings(s);
      applyTheme(s.theme);
      saved = true;
    } catch (e) {
      error = String(e);
    } finally {
      saving = false;
    }
  }

  // Keeps the live seeder in sync with (iroh_enabled && share_worldwide); seeding applies live, download route on next launch.
  async function reconcileIroh() {
    try {
      await api.saveSettings(s);
      if (s.iroh_enabled && s.share_worldwide) {
        await api.startWorldwide();
        sharing = true;
        flash("Seeding worldwide over Iroh");
      } else {
        await api.stopWorldwide();
        sharing = false;
        flash(s.iroh_enabled ? "Stopped seeding over Iroh" : "Iroh off");
      }
    } catch (e) {
      error = String(e);
    }
  }

  async function saveToken() {
    try {
      await api.setToken(tokenInput.trim());
      tokenInput = "";
      hasToken = await api.tokenStatus();
      flash("Token saved to your keychain");
    } catch (e) {
      error = String(e);
    }
  }
  async function forgetToken() {
    try {
      await api.clearToken();
      hasToken = await api.tokenStatus();
      flash("Token removed");
    } catch (e) {
      error = String(e);
    }
  }

  async function applyDevice() {
    try {
      await api.applyIdentity(s.device_name);
      flash("Device identity updated");
    } catch (e) {
      error = String(e);
    }
  }

  // The download-routing preference applies live (no restart), then persists.
  async function applyPreference() {
    try {
      await api.setDownloadPreference(s.download_preference);
      await api.saveSettings(s);
      flash("Download preference updated");
    } catch (e) {
      error = String(e);
    }
  }

  async function clearCache() {
    try {
      const freed = await api.clearCache();
      const c = await api.cache();
      cacheCount = c.length;
      cacheBytes = c.reduce((a, b) => a + (b.size_bytes || 0), 0);
      flash(`Freed ${fmtSize(freed)}`);
    } catch (e) {
      error = String(e);
    }
  }
  async function exportDiag() {
    try {
      const d = await api.exportDiagnostics();
      await navigator.clipboard.writeText(JSON.stringify(d, null, 2));
      flash("Diagnostics copied to clipboard");
    } catch (e) {
      error = String(e);
    }
  }
</script>

<div class="view">
  <h2>Settings</h2>
  {#if error}<p class="err">{error}</p>{/if}

  {#if s}
    <div class="section">Iroh</div>
    <div class="row">
      <div>
        <div>Use Iroh</div>
        <div class="muted">
          NAT-traversing peer-to-peer networking — no ports to open. Turn it off to disable
          Iroh entirely; while it's on, the switches below choose whether you download from
          peers, seed back over Iroh, or both. Seeding reacts immediately; the download
          route applies on next launch.
          {#if sharing}<span class="pill ok">seeding</span>{/if}
        </div>
      </div>
      <label class="switch"><input type="checkbox" bind:checked={s.iroh_enabled} on:change={reconcileIroh} /><span></span></label>
    </div>
    <div class="subgroup">
      <div class="row">
        <div><div>Download over Iroh</div><div class="muted">Fetch model bytes from peers. Applies on next launch.</div></div>
        <label class="switch"><input type="checkbox" bind:checked={s.iroh_download} on:change={save} disabled={!s.iroh_enabled} /><span></span></label>
      </div>
      <div class="row">
        <div><div>Seed my models to the world over Iroh</div><div class="muted">Share verified, openly-licensed models so peers can find them in Discover</div></div>
        <label class="switch"><input type="checkbox" bind:checked={s.share_worldwide} on:change={reconcileIroh} disabled={!s.iroh_enabled} /><span></span></label>
      </div>
      <div class="row">
        <div><div>Also share gated / licensed</div><div class="muted">Off by default — opt in deliberately</div></div>
        <label class="switch"><input type="checkbox" bind:checked={s.share_gated} on:change={save} disabled={!s.iroh_enabled} /><span></span></label>
      </div>
    </div>

    <div class="section">BitTorrent</div>
    <div class="row">
      <div>
        <div>Use BitTorrent</div>
        <div class="muted">Connect to the BitTorrent swarm. While connected, the switches below choose whether you download from the swarm, seed back to it, or both — and how it finds and connects to peers. Applies on next launch.</div>
      </div>
      <label class="switch"><input type="checkbox" bind:checked={s.bt_enabled} on:change={save} /><span></span></label>
    </div>
    <div class="subgroup">
      <div class="row">
        <div><div>Download over BitTorrent</div><div class="muted">Fetch model bytes from the swarm. Applies on next launch.</div></div>
        <label class="switch"><input type="checkbox" bind:checked={s.bt_download} on:change={save} disabled={!s.bt_enabled} /><span></span></label>
      </div>
      <div class="row">
        <div>
          <div>Seed openly-licensed models</div>
          <div class="muted">Re-share verified, openly-licensed blobs back over BitTorrent. Applies on next launch.</div>
        </div>
        <label class="switch"><input type="checkbox" bind:checked={s.bt_seed} on:change={save} disabled={!s.bt_enabled} /><span></span></label>
      </div>
      <div class="row">
        <div>
          <div>Use public BitTorrent trackers</div>
          <div class="muted">Find more peers via well-known public trackers, in addition to the DHT</div>
        </div>
        <label class="switch"><input type="checkbox" bind:checked={s.bt_use_public_trackers} on:change={save} disabled={!s.bt_enabled} /><span></span></label>
      </div>
      <div class="row">
        <div>
          <div>Enable DHT (decentralized network)</div>
          <div class="muted">Find peers without any tracker via the mainline DHT. Off leaves trackers and Peer Exchange only — magnet fetches then need a reachable tracker. Applies on next launch.</div>
        </div>
        <label class="switch"><input type="checkbox" bind:checked={s.bt_dht} on:change={save} disabled={!s.bt_enabled} /><span></span></label>
      </div>
      <div class="row">
        <div>
          <div>Enable Local Peer Discovery</div>
          <div class="muted">Find peers on your LAN via multicast. Peer Exchange (PeX) with connected peers is always on for public torrents. Applies on next launch.</div>
        </div>
        <label class="switch"><input type="checkbox" bind:checked={s.bt_lsd} on:change={save} disabled={!s.bt_enabled} /><span></span></label>
      </div>
      <div class="row">
        <div>
          <div>Use UPnP port forwarding</div>
          <div class="muted">Ask the router to map the listen port so peers can reach you behind NAT (UPnP/IGD; NAT-PMP isn't supported). Applies on next launch.</div>
        </div>
        <label class="switch"><input type="checkbox" bind:checked={s.bt_upnp} on:change={save} disabled={!s.bt_enabled} /><span></span></label>
      </div>
      <div class="row">
        <div>
          <div>Anonymous mode</div>
          <div class="muted">Hide the client identity: peers and trackers see a blank client name and an unbranded peer id. Your IP address stays visible to the swarm — only a SOCKS5 proxy hides that. Applies on next launch.</div>
        </div>
        <label class="switch"><input type="checkbox" bind:checked={s.bt_anonymous} on:change={save} disabled={!s.bt_enabled} /><span></span></label>
      </div>
    </div>
    <p class="muted" style="margin-top:6px">
      Privacy: BitTorrent announces your IP address and the model's info-hash to the DHT
      and, when enabled above, to public trackers — so peers and tracker operators can see
      that your IP is downloading or sharing that file. A SOCKS5 proxy routes this through
      the proxy; any other proxy still exposes your real IP to BitTorrent peers.
    </p>
    <div class="field">
      <span>Preferred listen port (0 = inbound off)</span>
      <div style="display:flex; gap:8px; align-items:center">
        <input type="number" min="0" max="65535" bind:value={s.bt_port} on:change={save} disabled={!s.bt_enabled} style="flex:1" />
        <button class="btn sm" type="button" on:click={randomPort} disabled={!s.bt_enabled}>Random</button>
      </div>
    </div>
    <label class="field">
      <span>Peer connection protocol</span>
      <select bind:value={s.bt_protocol} on:change={save} disabled={!s.bt_enabled}>
        <option value={0}>TCP and µTP</option>
        <option value={1}>TCP</option>
        <option value={2}>µTP</option>
      </select>
    </label>
    <label class="field"><span>Max connections per torrent (0 = unlimited)</span><input type="number" min="0" max="10000" bind:value={s.bt_max_peers} on:change={save} disabled={!s.bt_enabled} /></label>
    <label class="field"><span>Upload cap (Mbps, 0 = unlimited)</span><input type="number" min="0" bind:value={s.bt_up_cap_mbps} on:change={save} disabled={!s.bt_enabled} /></label>
    <label class="field"><span>Download cap (Mbps, 0 = unlimited)</span><input type="number" min="0" bind:value={s.bt_down_cap_mbps} on:change={save} disabled={!s.bt_enabled} /></label>
    <label class="field"><span>Max concurrent transfers (applies on next launch)</span><input type="number" min="1" max="32" bind:value={s.bt_max_concurrent} on:change={save} /></label>
    <label class="field"><span>Stop seeding at ratio (0 = unlimited)</span><input type="number" min="0" step="0.1" bind:value={s.bt_max_ratio} on:change={save} disabled={!s.bt_enabled} /></label>
    <div class="row">
      <div>
        <div>Sequential download</div>
        <div class="muted">Fetch pieces front-to-back (e.g. for streaming) instead of rarest-first. Applies live.</div>
      </div>
      <label class="switch"><input type="checkbox" bind:checked={s.bt_sequential} on:change={save} disabled={!s.bt_enabled} /><span></span></label>
    </div>
    <p class="muted" style="margin-top:6px">BitTorrent changes apply on next launch — the session binds ports at startup. Inbound peers depend on your network (no relay guarantee).</p>

    <div class="section">Bandwidth schedule</div>
    <div class="row">
      <div>
        <div>Alternative speed limits on a schedule</div>
        <div class="muted">Apply the alternative caps below during a daily window (e.g. throttle by day, open up overnight). Applies live.</div>
      </div>
      <label class="switch"><input type="checkbox" bind:checked={s.bt_schedule_enabled} on:change={saveSchedule} /><span></span></label>
    </div>
    {#if s.bt_schedule_enabled}
      <div class="variant">
        <label class="field"><span>From</span><input type="time" value={minToTime(s.bt_schedule_from_min)} on:change={(e) => { s.bt_schedule_from_min = timeToMin(e.target.value); saveSchedule(); }} /></label>
        <label class="field"><span>To</span><input type="time" value={minToTime(s.bt_schedule_to_min)} on:change={(e) => { s.bt_schedule_to_min = timeToMin(e.target.value); saveSchedule(); }} /></label>
      </div>
      <div class="days">
        {#each DAYS as d, i}
          <button class="btn xs {hasDay(i) ? 'primary' : ''}" type="button" on:click={() => toggleDay(i)}>{d}</button>
        {/each}
      </div>
      <p class="muted" style="margin-top:4px">No days selected = every day. A window that ends before it starts wraps past midnight.</p>
      <label class="field"><span>Alt upload cap (Mbps, 0 = unlimited)</span><input type="number" min="0" bind:value={s.bt_alt_up_cap_mbps} on:change={saveSchedule} /></label>
      <label class="field"><span>Alt BitTorrent download cap (Mbps, 0 = unlimited)</span><input type="number" min="0" bind:value={s.bt_alt_down_cap_mbps} on:change={saveSchedule} /></label>
      <label class="field"><span>Alt HTTP download cap (Mbps, 0 = unlimited)</span><input type="number" min="0" bind:value={s.alt_download_cap_mbps} on:change={saveSchedule} /></label>
    {/if}

    <div class="section">This device</div>
    <label class="field"><span>Device name (shown to peers)</span><input bind:value={s.device_name} on:blur={applyDevice} /></label>

    <div class="section">Hugging Face account</div>
    <div class="row">
      <div>
        <div>{hasToken ? "Token saved" : "Not signed in"}</div>
        <div class="muted">Needed for gated models. Stored in your OS keychain.</div>
      </div>
      {#if hasToken}<button class="btn sm" on:click={forgetToken}>Forget token</button>{/if}
    </div>
    <div class="variant">
      <input type="password" bind:value={tokenInput} placeholder="hf_…" />
      <button class="btn primary" on:click={saveToken} disabled={!tokenInput.trim()}>Save securely</button>
    </div>
    <a href="https://huggingface.co/settings/tokens" class="muted">Get a token →</a>

    <div class="section">Downloads</div>
    <div class="row">
      <div><div>Allow Hugging Face as a download source</div></div>
      <label class="switch"><input type="checkbox" bind:checked={s.allow_hf_download} on:change={save} /><span></span></label>
    </div>
    <label class="field"><span>Download speed cap (Mbps, 0 = unlimited)</span><input type="number" min="0" bind:value={s.download_cap_mbps} on:change={save} /></label>
    <label class="field"><span>Parallel connections (1–16)</span><input type="number" min="1" max="16" bind:value={s.download_connections} on:change={save} /></label>
    <label class="field">
      <span>Download preference</span>
      <select bind:value={s.download_preference} on:change={applyPreference}>
        <option value={0}>Auto (balanced)</option>
        <option value={1}>Prefer peers (Iroh + BitTorrent)</option>
        <option value={2}>Prefer BitTorrent</option>
        <option value={3}>Save data (single mirror)</option>
      </select>
    </label>

    <div class="section">Network</div>
    <div class="row">
      <div><div>Use a Hugging Face mirror</div></div>
      <label class="switch"><input type="checkbox" bind:checked={s.hf_mirror_enabled} on:change={save} /><span></span></label>
    </div>
    {#if s.hf_mirror_enabled}
      <label class="field"><span>Mirror URL</span><input bind:value={s.hf_mirror_url} on:change={save} placeholder="https://hf-mirror.com" /></label>
    {/if}
    <div class="row">
      <div><div>Route traffic through a proxy</div></div>
      <label class="switch"><input type="checkbox" bind:checked={s.proxy_enabled} on:change={save} /><span></span></label>
    </div>
    {#if s.proxy_enabled}
      <label class="field"><span>Proxy URL</span><input bind:value={s.proxy_url} on:change={save} placeholder="socks5://127.0.0.1:1080" /></label>
    {/if}
    <label class="field"><span>Tracker URL</span><input bind:value={s.tracker_url} on:change={save} /></label>

    <div class="section">Storage</div>
    <div class="row">
      <div><div>Cache</div><div class="muted">{cacheCount} blobs · {fmtSize(cacheBytes)}</div></div>
      <button class="btn sm" on:click={clearCache}>Clear unused</button>
    </div>

    <div class="section">Appearance</div>
    <label class="field">
      <span>Theme</span>
      <select bind:value={s.theme} on:change={() => { applyTheme(s.theme); save(); }}>
        <option value="system">Match system</option>
        <option value="light">Light</option>
        <option value="dark">Dark</option>
      </select>
    </label>

    <div class="section">Updates</div>
    <div class="row">
      <div>
        <div>Check for updates automatically</div>
        <div class="muted">
          On launch, anonymously check whether a newer Studio build is available.
          Updates are signature-verified before installing.
        </div>
      </div>
      <label class="switch"><input type="checkbox" bind:checked={s.auto_update} on:change={onAutoUpdateToggle} /><span></span></label>
    </div>
    <button class="btn sm" on:click={checkNow} disabled={updChecking}>
      {updChecking ? "Checking…" : "Check now"}
    </button>

    <div class="section">About</div>
    <button class="btn sm" on:click={exportDiag}>Export diagnostics</button>
    <p class="muted" style="margin-top:6px">Proxy, mirror and tracker changes apply on next launch.</p>

    <div class="actions">
      <button class="btn primary" on:click={save} disabled={saving}>{saving ? "Saving…" : "Save all settings"}</button>
      {#if saved}<span class="ok">Saved.</span>{/if}
    </div>
  {/if}
</div>

{#if toast}<div class="toast">{toast}</div>{/if}
