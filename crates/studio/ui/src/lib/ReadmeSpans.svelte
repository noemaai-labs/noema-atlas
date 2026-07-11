<script>
  // One wrapped run of styled inline spans from a parsed model card. Flags
  // combine (bold+italic+code); links open in the system browser, never the
  // app's own webview.
  import { api } from "./api.js";
  export let spans = [];

  function isHttp(u) {
    return /^https?:\/\//.test(u || "");
  }
  function open(e, url) {
    e.preventDefault();
    e.stopPropagation();
    api.openExternal(url);
  }
</script>

<!-- Kept on single lines: whitespace between tags would inject spaces the
     parser never emitted. -->
{#each spans as s}{#if s.link && isHttp(s.link)}<a class="rlink" href={s.link} title={s.link} on:click={(e) => open(e, s.link)}><span class="rspan" class:b={s.bold} class:i={s.italic} class:st={s.strike} class:c={s.code}>{s.text}</span></a>{:else}<span class="rspan" class:b={s.bold} class:i={s.italic} class:st={s.strike} class:c={s.code}>{s.text}</span>{/if}{/each}

<style>
  .rspan {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .b {
    font-weight: 650;
  }
  .i {
    font-style: italic;
  }
  .st {
    text-decoration: line-through;
  }
  .c {
    font-family: var(--font-mono);
    font-size: 0.92em;
    background: var(--surface-2);
    border: 1px solid var(--border);
    padding: 0 4px;
  }
  .rlink {
    color: var(--accent);
    text-decoration: none;
  }
  .rlink:hover .rspan {
    text-decoration: underline;
  }
</style>
