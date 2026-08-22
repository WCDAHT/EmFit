<!--
  DatesPanel.svelte - dates and size (advanced-search.md C8).

  Everything's dialog has two more rows here, Date last accessed and Date
  recently changed. EmFit's scanner keeps neither, so they are not offered:
  a control that cannot do anything has no business taking up the space.
-->
<script lang="ts">
  import { advanced, type DateFilter } from "../../advanced.svelte";
  import { SIZE_UNITS } from "../../advanced-syntax";
  import { tip } from "../../tooltip";
</script>

{#snippet dateRow(label: string, filter: DateFilter, hint: string)}
  <div class="row">
    <span class="label">{label}</span>
    <div class="bounds" use:tip={hint}>
      <input
        type="checkbox"
        bind:checked={filter.from.on}
        aria-label={`${label} from, on`}
      />
      <input
        type="date"
        value={filter.from.date}
        onchange={(e) => {
          filter.from.date = e.currentTarget.value;
          filter.from.on = e.currentTarget.value !== "";
        }}
        aria-label={`${label} from`}
      />
      <span class="to">to</span>
      <input
        type="checkbox"
        bind:checked={filter.to.on}
        aria-label={`${label} to, on`}
      />
      <input
        type="date"
        value={filter.to.date}
        onchange={(e) => {
          filter.to.date = e.currentTarget.value;
          filter.to.on = e.currentTarget.value !== "";
        }}
        aria-label={`${label} to`}
      />
    </div>
  </div>
{/snippet}

<section>
  {@render dateRow(
    "Date modified:",
    advanced.modified,
    "When the file was last written. Writes: dm:2024-01-01..2024-06-30",
  )}

  <div class="row">
    <span class="label">Size:</span>
    <div
      class="bounds"
      use:tip={"How big the file is. A folder is measured by everything inside it. Writes: size:1mb..2gb"}
    >
      <input
        bind:value={advanced.size.from}
        placeholder="from"
        inputmode="decimal"
        spellcheck="false"
        aria-label="Size from"
      />
      <select bind:value={advanced.size.fromUnit} aria-label="Size from unit">
        {#each SIZE_UNITS as unit (unit)}<option value={unit}>{unit}</option>{/each}
      </select>
      <span class="to">to</span>
      <input
        bind:value={advanced.size.to}
        placeholder="to"
        inputmode="decimal"
        spellcheck="false"
        aria-label="Size to"
      />
      <select bind:value={advanced.size.toUnit} aria-label="Size to unit">
        {#each SIZE_UNITS as unit (unit)}<option value={unit}>{unit}</option>{/each}
      </select>
    </div>
  </div>

  {@render dateRow(
    "Date created:",
    advanced.created,
    "When the file was created. Writes: dc:2024-01-01..2024-06-30",
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
    grid-template-columns: 140px 1fr;
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

  .to {
    color: var(--text-muted);
    font-size: var(--font-size-caption);
  }

  input:not([type="checkbox"]),
  select {
    height: var(--input-height);
    min-width: 0;
    padding: 0 var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface);
    color: var(--text-primary);
    font-family: inherit;
    font-size: var(--font-size-body);
  }
  input:not([type="checkbox"]) {
    flex: 1;
  }
  select {
    flex: 0 0 auto;
  }
  input:focus,
  select:focus {
    border-color: var(--selection);
  }
</style>
