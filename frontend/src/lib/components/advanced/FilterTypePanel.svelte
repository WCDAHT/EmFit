<!--
  FilterTypePanel.svelte - filter and type (advanced-search.md C9).

  Filter is the `Filters.csv` preset list, the same selection the toolbar
  dropdown holds - it lives in the session, not in this dialog, so the two
  can never disagree. Type is a plain choice of what kind of thing to match,
  and each option says which syntax it writes.
-->
<script lang="ts">
  import { onMount } from "svelte";
  import { advanced, TYPES } from "../../advanced.svelte";
  import { editFilters } from "../../ipc";
  import { queryChanged, reloadFilters, session } from "../../session.svelte";
  import { tip } from "../../tooltip";

  // Opening the dialog is another moment a `Filters.csv` edit should show up.
  onMount(() => void reloadFilters());

  function onPresetChange() {
    const preset = session.filters.presets.find((p) => p.name === session.presetName);
    session.preset = preset?.search ?? "";
    queryChanged(true);
  }

  const typeHint = $derived(
    TYPES.find((t) => t.key === advanced.type)?.terms.join(" ") || "no extra terms",
  );
</script>

<section>
  <div class="row">
    <span class="label">Filter:</span>
    <select
      bind:value={session.presetName}
      onchange={onPresetChange}
      use:tip={"A saved filter from Filters.csv. It applies to every search, not just this one, and shows in the toolbar too."}
      aria-label="Preset filter"
    >
      <option value="">Everything</option>
      {#each session.filters.presets.filter((p) => p.search !== "") as p (p.name)}
        <option value={p.name}>{p.name}</option>
      {/each}
    </select>
    <button
      use:tip={`Filters are a two-column CSV: a name and the search it runs. Opens ${session.filters.path ?? "Filters.csv"}`}
      onclick={() => void editFilters()}
    >
      Edit...
    </button>
  </div>

  <div class="row">
    <span class="label">Type:</span>
    <select
      bind:value={advanced.type}
      use:tip={`What kind of entry to match. Writes: ${typeHint}`}
      aria-label="Type"
    >
      {#each TYPES as t (t.key)}
        <option value={t.key}>{t.label}</option>
      {/each}
    </select>
  </div>
</section>

<style>
  section {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .row {
    display: grid;
    grid-template-columns: 140px 1fr auto;
    align-items: center;
    gap: var(--space-3);
  }

  .label {
    color: var(--text-secondary);
    font-size: var(--font-size-body);
    text-align: right;
  }

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
  select:focus {
    border-color: var(--selection);
  }

  button {
    height: var(--input-height);
    padding: 0 var(--space-3);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-small);
    background: var(--surface);
    color: var(--text-primary);
    font-family: inherit;
    font-size: var(--font-size-body);
    cursor: pointer;
  }
  button:hover {
    background: var(--surface-hover);
  }
</style>
