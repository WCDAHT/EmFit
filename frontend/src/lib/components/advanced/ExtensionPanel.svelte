<!--
  ExtensionPanel.svelte - extension and attributes (advanced-search.md C10).

  Everything's dialog offers Match case and Match diacritics beside the
  extension box. EmFit's `ext:` list compares under the volume's own rule and
  has nowhere to put a per-term override, so those two are disabled and say
  so - eight of the fourteen attributes are disabled for the same kind of
  reason, and for the same reason are shown rather than hidden.
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
  <div class="row">
    <span class="label"></span>
    <div class="toggles">
      <label use:tip={"Extensions compare the way the volume itself does, so this cannot be set per search."}>
        <input type="checkbox" disabled />
        <span>Match case</span>
      </label>
      <label use:tip={"Extensions compare the way the volume itself does, so this cannot be set per search."}>
        <input type="checkbox" disabled />
        <span>Match diacritics</span>
      </label>
    </div>
  </div>

  <h3>Attributes:</h3>
  <ul class="attributes">
    {#each ATTRIBUTES as a (a.letter)}
      <li>
        <label
          use:tip={a.recorded
            ? `Only entries with the ${a.label} bit set. Writes: attrib:${a.letter}`
            : `EmFit does not record ${a.label}, so there is nothing to filter on.`}
        >
          <input
            type="checkbox"
            bind:checked={advanced.attributes[a.letter]}
            disabled={!a.recorded}
          />
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

  .toggles {
    display: flex;
    gap: var(--space-4);
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
  label:has(input:disabled) {
    cursor: default;
    color: var(--text-muted);
  }
</style>
