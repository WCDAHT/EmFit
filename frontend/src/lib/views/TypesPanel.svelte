<!--
  TypesPanel.svelte — WizTree's "File Types" tab (features.md §4.3).

  What kind of thing is eating the disk, aggregated by extension in Rust.
  Ticking types dims everything else in the treemap and applies the same
  extensions as a filter on the List tab, so one selection answers the
  question in both views.
-->
<script lang="ts">
  import { typeBreakdown } from "../ipc";
  import { session, queryChanged } from "../session.svelte";
  import type { TypeRowDto } from "../types";

  let rows: TypeRowDto[] = $state([]);

  $effect(() => {
    void session.viewEpoch;
    void (async () => {
      rows = await typeBreakdown(60);
    })();
  });

  function toggle(ext: string) {
    if (session.typeFilter.has(ext)) {
      session.typeFilter.delete(ext);
    } else {
      session.typeFilter.add(ext);
    }
    // The same selection filters the file list.
    session.extensions = [...session.typeFilter].join(";");
    queryChanged(true);
  }

  function clearAll() {
    session.typeFilter.clear();
    session.extensions = "";
    queryChanged(true);
  }

  function label(row: TypeRowDto): string {
    if (row.extension === "") return "(none)";
    return row.extension;
  }

  function selectable(row: TypeRowDto): boolean {
    return row.extension !== "(other)";
  }
</script>

<div class="types">
  <div class="head">
    <span>File types</span>
    {#if session.typeFilter.size > 0}
      <button class="clear" onclick={clearAll}>Clear ({session.typeFilter.size})</button>
    {/if}
  </div>

  <div class="rows">
    {#each rows as row (row.extension)}
      <label class="row" class:muted={!selectable(row)}>
        <input
          type="checkbox"
          disabled={!selectable(row)}
          checked={session.typeFilter.has(row.extension)}
          onchange={() => toggle(row.extension)}
        />
        <span
          class="dot"
          style:background={row.category > 0
            ? `var(--category-${row.category})`
            : "var(--text-muted)"}
        ></span>
        <span class="ext" title={row.kind_label}>{label(row)}</span>
        <span class="count">{row.count.toLocaleString()}</span>
        <span class="bar">
          <span class="fill" style:width="{Math.min(100, row.percent)}%"></span>
        </span>
        <span class="alloc">{row.allocated_display}</span>
        <span class="pct">{row.percent.toFixed(1)}%</span>
      </label>
    {/each}
    {#if rows.length === 0}
      <div class="empty">No files yet.</div>
    {/if}
  </div>
</div>

<style>
  .types {
    display: flex;
    flex-direction: column;
    min-height: 0;
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface-sunken);
  }

  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: var(--space-1) var(--space-2);
    border-bottom: 1px solid var(--border);
    background: var(--surface-raised);
    color: var(--text-secondary);
    font-size: var(--font-size-caption);
    font-weight: var(--font-weight-semibold);
    text-transform: uppercase;
  }
  .clear {
    border: none;
    background: none;
    color: var(--accent);
    font-family: inherit;
    font-size: var(--font-size-caption);
    cursor: pointer;
  }

  .rows {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }

  .row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) var(--space-2);
    border-bottom: 1px solid var(--border-subtle);
    font-size: var(--font-size-body);
    color: var(--text-primary);
    cursor: pointer;
  }
  .row:hover {
    background: var(--surface-hover);
  }
  .row.muted {
    color: var(--text-muted);
    cursor: default;
  }

  .dot {
    flex: 0 0 auto;
    width: 9px;
    height: 9px;
    border-radius: 2px;
  }
  .ext {
    flex: 0 0 64px;
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }
  .count {
    flex: 0 0 64px;
    text-align: right;
    color: var(--text-secondary);
    font-size: var(--font-size-caption);
    font-variant-numeric: tabular-nums;
  }
  .bar {
    flex: 1 1 auto;
    height: 8px;
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface);
    overflow: hidden;
  }
  .fill {
    display: block;
    height: 100%;
    background: var(--accent);
    opacity: 0.75;
  }
  .alloc {
    flex: 0 0 72px;
    text-align: right;
    font-variant-numeric: tabular-nums;
  }
  .pct {
    flex: 0 0 48px;
    text-align: right;
    color: var(--text-secondary);
    font-size: var(--font-size-caption);
    font-variant-numeric: tabular-nums;
  }

  .empty {
    padding: var(--space-3);
    color: var(--text-muted);
    text-align: center;
  }
</style>
