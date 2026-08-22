<!--
  DatesPanel.svelte - dates and size (advanced-search.md C8).

  Two of the five rows in Everything's dialog have nothing behind them here:
  EmFit does not keep a date accessed, and has no notion of a "recently
  changed" date at all. They are rendered disabled and say why, rather than
  being left out (a missing row reads as an oversight) or left enabled (a
  control that quietly does nothing is worse than one that admits it).
-->
<script lang="ts">
  import { advanced, type DateFilter } from "../../advanced.svelte";
  import { SIZE_UNITS } from "../../advanced-syntax";
  import { tip } from "../../tooltip";

  const UNRECORDED =
    "EmFit does not record this, so there is nothing to search. It would need a change to the scanner.";
</script>

{#snippet dateRow(label: string, filter: DateFilter | undefined, hint: string)}
  <div class="row">
    <span class="label">{label}</span>
    <div class="bounds" use:tip={hint}>
      <input
        type="checkbox"
        checked={filter?.from.on ?? false}
        disabled={!filter}
        onchange={(e) => filter && (filter.from.on = e.currentTarget.checked)}
        aria-label={`${label} from, on`}
      />
      <input
        type="date"
        value={filter?.from.date ?? ""}
        disabled={!filter}
        onchange={(e) => {
          if (!filter) return;
          filter.from.date = e.currentTarget.value;
          filter.from.on = e.currentTarget.value !== "";
        }}
        aria-label={`${label} from`}
      />
      <span class="to">to</span>
      <input
        type="checkbox"
        checked={filter?.to.on ?? false}
        disabled={!filter}
        onchange={(e) => filter && (filter.to.on = e.currentTarget.checked)}
        aria-label={`${label} to, on`}
      />
      <input
        type="date"
        value={filter?.to.date ?? ""}
        disabled={!filter}
        onchange={(e) => {
          if (!filter) return;
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
  {@render dateRow("Date last accessed:", undefined, UNRECORDED)}
  {@render dateRow("Date recently changed:", undefined, UNRECORDED)}
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
