<script>
  import { onMount } from "svelte";
  import { api } from "../api.js";
  import { fmtSize } from "../format.js";
  export let applyTheme;

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

  async function toggleWorldwide() {
    try {
      await api.saveSettings(s);
      if (s.share_worldwide) {
        await api.startWorldwide();
        sharing = true;
        flash("Sharing worldwide");
      } else {
        await api.stopWorldwide();
        sharing = false;
        flash("Stopped sharing");
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
      await api.applyIdentity(s.device_name, s.group_code);
      flash("Device identity updated");
    } catch (e) {
      error = String(e);
    }
  }
  async function makeGroup() {
    try {
      s.group_code = await api.createGroup();
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
    <div class="section">Sharing</div>
    <div class="row">
      <div>
        <div>Share downloads worldwide</div>
        <div class="muted">
          Seed verified, openly-licensed models to peers over the mesh
          {#if sharing}<span class="pill ok">live</span>{/if}
        </div>
      </div>
      <label class="switch"><input type="checkbox" bind:checked={s.share_worldwide} on:change={toggleWorldwide} /><span></span></label>
    </div>
    <div class="row">
      <div><div>Also share gated / licensed</div><div class="muted">Off by default — opt in deliberately</div></div>
      <label class="switch"><input type="checkbox" bind:checked={s.share_gated} on:change={save} /><span></span></label>
    </div>

    <div class="section">This device</div>
    <label class="field"><span>Device name (shown to peers)</span><input bind:value={s.device_name} on:blur={applyDevice} /></label>
    <label class="field">
      <span>Link my own devices — shared group code</span>
      <input bind:value={s.group_code} placeholder="paste the same code on each device" on:blur={applyDevice} />
    </label>
    <button class="btn sm" on:click={makeGroup}>Generate a group code</button>

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
    <label class="check"><input type="checkbox" bind:checked={s.skip_download_confirm} on:change={save} /> Download share links & Explore models immediately (skip confirm)</label>

    <div class="section">Network</div>
    <div class="row">
      <div><div>Use a Hugging Face mirror</div></div>
      <label class="switch"><input type="checkbox" bind:checked={s.hf_mirror_enabled} /><span></span></label>
    </div>
    {#if s.hf_mirror_enabled}
      <label class="field"><span>Mirror URL</span><input bind:value={s.hf_mirror_url} placeholder="https://hf-mirror.com" /></label>
    {/if}
    <div class="row">
      <div><div>Route traffic through a proxy</div></div>
      <label class="switch"><input type="checkbox" bind:checked={s.proxy_enabled} /><span></span></label>
    </div>
    {#if s.proxy_enabled}
      <label class="field"><span>Proxy URL</span><input bind:value={s.proxy_url} placeholder="socks5://127.0.0.1:1080" /></label>
    {/if}
    <label class="field"><span>Tracker URL</span><input bind:value={s.tracker_url} /></label>

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
