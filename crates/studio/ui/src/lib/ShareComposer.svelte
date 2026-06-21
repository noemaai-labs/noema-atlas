<script>
  import { onDestroy } from "svelte";
  import { api, pickModelFile, onImportProgress } from "./api.js";
  export let model = null;
  export let initialPath = "";
  export let onClose = () => {};
  // Called with a short success message so the parent can flash a toast.
  export let onSaved = () => {};

  const isEdit = !!model;
  let path = initialPath;
  let title = model?.name ?? (initialPath ? initialPath.split("/").pop() : "");
  let family = model?.family ?? "";
  let quant = model?.quant ?? "";
  let license = model?.license ?? "";
  let description = model?.description ?? "";
  let originUrl = model?.origin ?? "";
  let checkHf = false;
  let publish = model ? !!model.shareable : false;
  let busy = false;
  let error = "";
  let phase = "";
  let pct = 0;
  let unlisten = null;

  function stopProgress() {
    if (unlisten) {
      unlisten();
      unlisten = null;
    }
  }
  onDestroy(stopProgress);

  // Button label while working: a live phase ("Hashing… 47%") beats a dead
  // "Working…" so a multi-gigabyte import never looks frozen.
  $: workingLabel =
    phase === "hashing" && pct > 0 ? `Hashing… ${pct}%` : "Working…";

  async function pick() {
    error = "";
    try {
      const p = await pickModelFile();
      if (p) {
        path = p;
        if (!title) title = p.split("/").pop();
      }
    } catch (e) {
      error = String(e);
    }
  }

  async function save() {
    if (!isEdit && !path) {
      error = "Choose a model file first.";
      return;
    }
    busy = true;
    error = "";
    phase = "";
    pct = 0;
    try {
      if (isEdit) {
        await api.editModel({
          manifestId: model.manifest_id,
          title,
          family,
          quant,
          architecture: null,
          license,
          description,
          originUrl,
          publish,
        });
        onSaved("Changes saved");
        onClose();
      } else {
        phase = "hashing";
        unlisten = await onImportProgress((p) => {
          phase = p.phase;
          pct = p.bytes_total
            ? Math.round((p.bytes_done / p.bytes_total) * 100)
            : 0;
        });
        const r = await api.importLocal({
          path,
          title,
          family,
          quant,
          architecture: null,
          license,
          description,
          originUrl,
          skipHfMatch: !checkHf,
          publish,
        });
        onSaved(r.shareable ? "Imported · now sharing worldwide" : "Imported");
        onClose();
      }
    } catch (e) {
      error = String(e);
    } finally {
      stopProgress();
      busy = false;
    }
  }
</script>

<div class="modal-backdrop" on:click|self={onClose}>
  <div class="modal">
    <div class="modal-head">
      <h3>{isEdit ? "Edit model" : "Share a model"}</h3>
      <button class="icon-btn" on:click={onClose} aria-label="Close">✕</button>
    </div>

    {#if !isEdit}
      <p class="muted" style="margin:0 0 12px">
        Import a model file you already have — Atlas hashes it, matches it to its
        origin when possible, and (optionally) shares it worldwide.
      </p>
      <div class="variant">
        <input readonly value={path} placeholder="No file chosen" />
        <button class="btn" on:click={pick}>Choose file…</button>
      </div>
    {/if}

    <label class="field"><span>Title</span><input bind:value={title} placeholder="e.g. Mistral-7B-Instruct-v0.3" /></label>
    <div class="grid2">
      <label class="field"><span>Family</span><input bind:value={family} placeholder="Mistral" /></label>
      <label class="field"><span>Quantization</span><input bind:value={quant} placeholder="Q4_K_M" /></label>
    </div>
    <label class="field"><span>License</span><input bind:value={license} placeholder="apache-2.0 (leave blank if unsure)" /></label>
    <label class="field"><span>Description</span><textarea bind:value={description} rows="2" placeholder="Provenance note (optional)"></textarea></label>
    <label class="field"><span>Where is this from? (optional)</span><input bind:value={originUrl} placeholder="https://huggingface.co/…" /></label>

    {#if !isEdit}
      <label class="check"><input type="checkbox" bind:checked={checkHf} /> Also check Hugging Face for a canonical match</label>
    {/if}
    <label class="check"><input type="checkbox" bind:checked={publish} /> Publish to Explore (share worldwide)</label>

    {#if error}<p class="err">{error}</p>{/if}

    <div class="actions">
      <button class="btn" on:click={onClose}>Close</button>
      <button class="btn primary" on:click={save} disabled={busy}>
        {busy ? workingLabel : isEdit ? "Save changes" : "Import & create link"}
      </button>
    </div>
  </div>
</div>
