<!--
  SearchBar.svelte - the search box and its filters (features.md sec 2).

  One text input carries the whole query grammar: substrings, wildcards,
  `;`-OR terms, backtick path scoping, and the inline `ext:` `size:` `dm:`
  `folder:` operators. The panel below adds the separate filter fields
  (regex, size, date, extensions), the Filters.csv preset dropdown, and the
  hidden/system/case toggles. Parsing is lenient in Rust; anything it skips
  comes back as a warning and renders under the box.
-->
<script lang="ts">
  import { onMount } from "svelte";
  import { listPresets } from "../ipc";
  import {
    session,
    hooks,
    queryChanged,
    filtersActive,
    clearFilters,
  } from "../session.svelte";
  import type { PresetDto } from "../types";
  import Icon from "../components/Icon.svelte";

  let presets: PresetDto[] = $state([]);
  let searchInput: HTMLInputElement | undefined = $state();
  let presetName = $state("");

  onMount(async () => {
    // Ctrl+F at the app root lands here (STANDARDS sec 3.7).
    hooks.focusSearch = () => searchInput?.focus();
    presets = await listPresets();
  });

  function onPresetChange() {
    const preset = presets.find((p) => p.name === presetName);
    session.preset = preset?.search ?? "";
    queryChanged(true);
  }

  const resultLine = $derived(
    session.volumes.some((v) => v.scanned)
      ? `${session.total.toLocaleString()} results in ${session.elapsedMs} ms`
      : "",
  );
</script>

<div class="search">
  <div class="bar">
    <div class="box">
      <Icon name="search" color="var(--text-muted)" />
      <input
        bind:this={searchInput}
        bind:value={session.text}
        oninput={() => queryChanged()}
        type="text"
        spellcheck="false"
        placeholder={"Search - try  *.pdf;*.docx   ext:iso size:>1gb   `C:\\Users` report   (Ctrl+F)"}
        aria-label="Search"
      />
      {#if session.text}
        <button
          class="clear-text"
          title="Clear search (Esc)"
          onclick={() => {
            session.text = "";
            queryChanged(true);
          }}
        >
          <Icon name="x" />
        </button>
      {/if}
    </div>

    <select
      class="preset"
      bind:value={presetName}
      onchange={onPresetChange}
      title="Preset filter (Filters.csv)"
      aria-label="Preset filter"
    >
      <option value="">Everything</option>
      {#each presets.filter((p) => p.search !== "") as p (p.name)}
        <option value={p.name}>{p.name}</option>
      {/each}
    </select>

    <button
      class="filters-toggle"
      class:active={filtersActive()}
      title="More filters"
      onclick={() => (session.filtersOpen = !session.filtersOpen)}
    >
      <Icon name="funnel" />
      {#if filtersActive()}<span class="dot"></span>{/if}
    </button>

    {#if filtersActive()}
      <button class="clear-all" title="Clear all filters" onclick={clearFilters}>
        Clear filters
      </button>
    {/if}
  </div>

  {#if session.filtersOpen}
    <div class="panel">
      <label>
        <span>Regex</span>
        <input
          bind:value={session.regex}
          oninput={() => queryChanged()}
          placeholder="^\d{4}-.*\.log$"
          spellcheck="false"
        />
      </label>
      <label>
        <span>Size</span>
        <input
          bind:value={session.size}
          oninput={() => queryChanged()}
          placeholder=">10MB or 1MB..1GB"
          spellcheck="false"
        />
      </label>
      <label>
        <span>Modified</span>
        <input
          bind:value={session.modified}
          oninput={() => queryChanged()}
          placeholder="2024-01-01..2024-06-30"
          spellcheck="false"
        />
      </label>
      <label>
        <span>Extensions</span>
        <input
          bind:value={session.extensions}
          oninput={() => queryChanged()}
          placeholder="pdf;docx;xlsx"
          spellcheck="false"
        />
      </label>
      <label class="check">
        <input
          type="checkbox"
          bind:checked={session.caseSensitive}
          onchange={() => queryChanged(true)}
        />
        <span>Case sensitive</span>
      </label>
      <label class="check">
        <input
          type="checkbox"
          bind:checked={session.includeHidden}
          onchange={() => queryChanged(true)}
        />
        <span>Hidden files</span>
      </label>
      <label class="check">
        <input
          type="checkbox"
          bind:checked={session.includeSystem}
          onchange={() => queryChanged(true)}
        />
        <span>System files</span>
      </label>
    </div>
  {/if}

  {#if resultLine || session.warnings.length > 0}
    <div class="meta">
      <span class="results">{resultLine}</span>
      {#each session.warnings as warning (warning)}
        <span class="warning" title={warning}>! {warning}</span>
      {/each}
    </div>
  {/if}
</div>

<style>
  .search {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .bar {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .box {
    flex: 1;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    height: var(--button-height);
    padding: 0 var(--space-3);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-small);
    background: var(--surface-raised);
  }
  .box:focus-within {
    border-color: var(--selection);
  }
  .box input {
    flex: 1;
    border: none;
    outline: none;
    background: none;
    color: var(--text-primary);
    font-family: inherit;
    font-size: var(--font-size-body);
  }

  .clear-text {
    display: grid;
    place-items: center;
    border: none;
    background: none;
    color: var(--text-muted);
    cursor: pointer;
  }
  .clear-text:hover {
    color: var(--text-primary);
  }

  .preset {
    height: var(--button-height);
    padding: 0 var(--space-2);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-small);
    background: var(--surface-raised);
    color: var(--text-primary);
    font-family: inherit;
    font-size: var(--font-size-body);
  }

  .filters-toggle {
    position: relative;
    display: grid;
    place-items: center;
    width: var(--button-height);
    height: var(--button-height);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface-raised);
    color: var(--text-secondary);
    cursor: pointer;
  }
  .filters-toggle:hover,
  .filters-toggle.active {
    color: var(--text-primary);
  }
  .dot {
    position: absolute;
    top: var(--space-1);
    right: var(--space-1);
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--accent);
  }

  .clear-all {
    height: var(--button-height);
    padding: 0 var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: none;
    color: var(--text-secondary);
    font-family: inherit;
    font-size: var(--font-size-body);
    cursor: pointer;
  }
  .clear-all:hover {
    color: var(--text-primary);
    background: var(--surface-hover);
  }

  .panel {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface-raised);
  }
  .panel label {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--text-secondary);
    font-size: var(--font-size-body);
  }
  .panel label span {
    white-space: nowrap;
  }
  .panel label:not(.check) input {
    width: 160px;
    height: 24px;
    padding: 0 var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface);
    color: var(--text-primary);
    font-family: inherit;
    font-size: var(--font-size-body);
  }
  .panel .check {
    cursor: pointer;
  }

  .meta {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    font-size: var(--font-size-caption);
  }
  .results {
    color: var(--text-secondary);
    font-variant-numeric: tabular-nums;
  }
  .warning {
    color: var(--warning);
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }
</style>
