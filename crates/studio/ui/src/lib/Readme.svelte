<script>
  // Native renderer for a parsed model card (noema_core::readme block model).
  // Every block is a purpose-built element — no {@html}, ever.
  import Spans from "./ReadmeSpans.svelte";
  import { copyText } from "./api.js";
  export let blocks = [];

  function isHttp(u) {
    return /^https?:\/\//.test(u || "");
  }
  let copied = null;
  async function copyCode(i, code) {
    if (await copyText(code)) {
      copied = i;
      setTimeout(() => (copied = null), 1200);
    }
  }
  function imgStyle(b) {
    return b.width ? `max-width: min(100%, ${b.width}px);` : "max-width: 100%;";
  }
  function hTag(b) {
    return `h${Math.min(Math.max(b.level || 1, 1), 6)}`;
  }
</script>

<div class="readme">
  {#each blocks as b, i}
    {#if b.kind === "heading"}
      <svelte:element this={hTag(b)}><Spans spans={b.spans} /></svelte:element>
    {:else if b.kind === "paragraph"}
      <p><Spans spans={b.spans} /></p>
    {:else if b.kind === "code"}
      <div class="rcode">
        <div class="rcode-bar">
          <span class="rcode-lang">{b.lang || "code"}</span>
          <button class="btn xs" on:click={() => copyCode(i, b.code)}>
            {copied === i ? "Copied" : "Copy"}
          </button>
        </div>
        <pre><code>{b.code}</code></pre>
      </div>
    {:else if b.kind === "list_item"}
      <div class="rli" style={`padding-left: ${8 + (b.indent || 0) * 18}px`}>
        <span class="rli-marker">{b.ordered != null ? `${b.ordered}.` : "•"}</span>
        <span class="rli-body"><Spans spans={b.spans} /></span>
      </div>
    {:else if b.kind === "quote"}
      <blockquote><Spans spans={b.spans} /></blockquote>
    {:else if b.kind === "table"}
      <div class="rtable-wrap">
        <table>
          <thead>
            <tr>
              {#each b.header as cell}<th><Spans spans={cell} /></th>{/each}
            </tr>
          </thead>
          <tbody>
            {#each b.rows as row}
              <tr>
                {#each row as cell}<td><Spans spans={cell} /></td>{/each}
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {:else if b.kind === "image"}
      {#if isHttp(b.src)}
        <img
          src={b.src}
          alt={b.alt || ""}
          loading="lazy"
          style={imgStyle(b)}
          on:error={(e) => (e.currentTarget.style.display = "none")}
        />
      {:else if b.alt}
        <p class="muted">{b.alt}</p>
      {/if}
    {:else if b.kind === "divider"}
      <hr />
    {/if}
  {/each}
</div>

<style>
  .readme {
    font-size: 13px;
    line-height: 1.55;
  }
  .readme :is(h1, h2, h3, h4, h5, h6) {
    margin: 14px 0 6px;
    line-height: 1.3;
  }
  .readme h1 {
    font-size: 19px;
    border-bottom: 1px solid var(--border);
    padding-bottom: 4px;
  }
  .readme h2 {
    font-size: 16px;
    border-bottom: 1px solid var(--border);
    padding-bottom: 3px;
  }
  .readme h3 {
    font-size: 14px;
  }
  .readme :is(h4, h5, h6) {
    font-size: 13px;
  }
  .readme p {
    margin: 6px 0;
  }
  .rcode {
    border: 1px solid var(--border);
    background: var(--surface-2);
    margin: 8px 0;
  }
  .rcode-bar {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 3px 8px;
    border-bottom: 1px solid var(--border);
  }
  .rcode-lang {
    font-family: var(--font-mono);
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--text-2);
  }
  .rcode pre {
    margin: 0;
    padding: 10px 12px;
    overflow-x: auto;
  }
  .rcode code {
    font-family: var(--font-mono);
    font-size: 12px;
  }
  .rli {
    display: flex;
    gap: 8px;
    margin: 3px 0;
  }
  .rli-marker {
    color: var(--text-2);
    flex: none;
  }
  .rli-body {
    min-width: 0;
  }
  .readme blockquote {
    margin: 8px 0;
    padding: 4px 12px;
    border-left: 3px solid var(--border-2);
    color: var(--text-2);
  }
  .rtable-wrap {
    overflow-x: auto;
    margin: 10px 0;
    border: 1px solid var(--border);
  }
  .readme table {
    border-collapse: collapse;
    font-size: 12px;
    width: 100%;
  }
  .readme th,
  .readme td {
    border: 1px solid var(--border);
    padding: 5px 10px;
    text-align: left;
    vertical-align: top;
  }
  .readme th {
    background: var(--surface-2);
    font-weight: 650;
    white-space: nowrap;
  }
  .readme img {
    display: block;
    margin: 8px 0;
    height: auto;
  }
  .readme hr {
    border: none;
    border-top: 1px solid var(--border);
    margin: 12px 0;
  }
</style>
