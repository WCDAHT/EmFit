<!--
  ExtensionPanel.svelte - extension and attributes (advanced-search.md C10).

  Everything's dialog also has Match case and Match diacritics beside the
  extension box, and fourteen attributes rather than six. An extension list
  compares under the volume's own rule with no room for a per-term override,
  and the other eight attribute bits are not in the index, so neither is
  offered here.
-->
<script lang="ts">
  import { advanced, ATTRIBUTES } from "../../advanced.svelte";
  import { tip } from "../../tooltip";
</script>

<section>
  <div class="row">
    <span class="label">Extension:</span>
    <input
      bind:value={advanced.extensions}
      use:tip={"Match any of these extensions, separated by semicolons. Writes: ext:pdf;docx"}
      placeholder="pdf;docx;xlsx"
      spellcheck="false"
      aria-label="Extension"
    />
  </div>
  <h3>Attributes:</h3>
  <ul class="attributes">
    {#each ATTRIBUTES as a (a.letter)}
      <li>
        <label use:tip={`Only entries with the ${a.label} bit set. Writes: attrib:${a.letter}`}>
          <input type="checkbox" bind:checked={advanced.attributes[a.letter]} />
          <span>{a.label}</span>
        </label>
      </li>
    {/each}
  </ul>
</section>

<style>
  section {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .row {
    display: grid;
    grid-template-columns: 140px 1fr;
    align-items: center;
    gap: var(--space-3);
  }

  .label {
    color: var(--text-secondary);
    font-size: var(--font-size-body);
    text-align: right;
  }

  input:not([type="checkbox"]) {
    min-width: 0;
    height: var(--input-height);
    padding: 0 var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface);
    color: var(--text-primary);
    font-family: inherit;
    font-size: var(--font-size-body);
  }
  input:focus {
    border-color: var(--selection);
  }

  h3 {
    margin: var(--space-2) 0 0;
    font-size: var(--font-size-body);
    font-weight: var(--font-weight-semibold);
    color: var(--text-primary);
  }

  .attributes {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(180px, 1fr));
    gap: var(--space-1) var(--space-3);
    margin: 0;
    padding: 0;
    list-style: none;
  }

  label {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--text-secondary);
    font-size: var(--font-size-caption);
    cursor: pointer;
  }
</style>
