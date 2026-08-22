<!--
  PatternPanel.svelte - regex, name length, folder depth (advanced-search.md
  C11). Three conditions that are about the shape of a name rather than what
  it says.
-->
<script lang="ts">
  import { advanced, type CountFilter } from "../../advanced.svelte";
  import { tip } from "../../tooltip";
</script>

{#snippet countRow(label: string, filter: CountFilter, hint: string)}
  <div class="row">
    <span class="label">{label}</span>
    <div class="bounds" use:tip={hint}>
      <input
        bind:value={filter.from}
        placeholder="from"
        inputmode="numeric"
        spellcheck="false"
        aria-label={`${label} from`}
      />
      <span class="to">to</span>
      <input
        bind:value={filter.to}
        placeholder="to"
        inputmode="numeric"
        spellcheck="false"
        aria-label={`${label} to`}
      />
    </div>
  </div>
{/snippet}

<section>
  <div class="row">
    <span class="label">Matches this regular expression:</span>
    <input
      bind:value={advanced.regex.text}
      use:tip={"A regular expression over the name. Writes: regex:^\d{4}-.*\.log$"}
      placeholder="^\d{4}-.*\.log$"
      spellcheck="false"
      aria-label="Regular expression"
    />
  </div>
  <div class="row">
    <span class="label"></span>
    <label use:tip={"Uppercase and lowercase count as different letters. Writes: case:regex:"}>
      <input type="checkbox" bind:checked={advanced.regex.matchCase} />
      <span>Match case</span>
    </label>
  </div>

  {@render countRow(
    "File name length:",
    advanced.length,
    "How many characters the name has. Writes: len:8..64",
  )}
  <div class="row">
    <span class="label"></span>
    <label use:tip={"Measure the whole path instead of just the name. Writes: path:len:"}>
      <input type="checkbox" bind:checked={advanced.lengthOnPath} />
      <span>Include path</span>
    </label>
  </div>

  {@render countRow(
    "Folder depth:",
    advanced.depth,
    "How many folders sit above this one. A drive root is 0. Writes: parents:1..3",
  )}
</section>

<style>
  section {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .row {
    display: grid;
    grid-template-columns: 220px 1fr;
    align-items: center;
    gap: var(--space-3);
  }

  .label {
    color: var(--text-secondary);
    font-size: var(--font-size-body);
    text-align: right;
  }

  .bounds {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-width: 0;
  }
  .bounds input {
    flex: 1;
    max-width: 120px;
  }

  .to {
    color: var(--text-muted);
    font-size: var(--font-size-caption);
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

  label {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--text-secondary);
    font-size: var(--font-size-caption);
    cursor: pointer;
  }
</style>
