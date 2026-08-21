<!--
  FileList.svelte - the virtualized result table (features.md sec 3).

  Only the visible window of rows ever exists in the DOM or crosses IPC:
  scrolling asks Rust for `rows(offset, count)` and renders what comes back.

  Deviation note (STANDARDS sec 3.2 suggests a library for virtualized lists):
  the row source here is a Rust-side window API, not a client-side array,
  which is the shape the maintained virtual-list libraries assume. Windowing
  a fixed-row-height table over an async source is ~60 lines below; a library
  adapter around it would be larger than the hand-roll.

  Very large sets exceed the browser's element-height cap (~33M px), so the
  scroller runs in one of two modes:
  - exact: spacer = total x rowHeight, rows positioned absolutely. Pixel-true.
  - proportional: spacer capped, scrollTop maps linearly onto the row range
    and rows render viewport-locked. Engaged only past ~1M rows.
-->
<script lang="ts">
  import { getRows, showContextMenu } from "../ipc";
  import { formatSize } from "../format";
  import {
    session,
    isSelected,
    selectOnly,
    toggleSelect,
    selectRange,
    sortBy,
  } from "../session.svelte";
  import type { RowDto, SortKey } from "../types";
  import Icon from "../components/Icon.svelte";

  const ROW_H = 28;
  const OVERSCAN = 12;
  /** Below the ~33.5M px element-height cap with margin. */
  const MAX_SPACER = 30_000_000;

  // Columns match the folder tree's table design: fixed widths in state
  // (drag the header edge to resize), one shared grid template with a
  // trailing filler, sort on header click with caret indicators.
  interface Column {
    key: SortKey;
    label: string;
    w: number;
    min: number;
    numeric?: boolean;
  }
  const COLUMNS: Column[] = [
    { key: "name", label: "Name", w: 280, min: 140 },
    { key: "size", label: "Size", w: 92, min: 56, numeric: true },
    { key: "allocated", label: "Allocated", w: 92, min: 56, numeric: true },
    { key: "extension", label: "Ext", w: 64, min: 44 },
    { key: "modified", label: "Date Modified", w: 122, min: 80 },
    { key: "kind", label: "Type", w: 92, min: 56 },
    { key: "path", label: "Path", w: 340, min: 120 },
  ];

  let colW = $state<Record<SortKey, number>>(
    Object.fromEntries(COLUMNS.map((c) => [c.key, c.w])) as Record<SortKey, number>,
  );
  const template = $derived(
    COLUMNS.map((c) => `${colW[c.key]}px`).join(" ") + " minmax(0, 1fr)",
  );

  let resizing: { key: SortKey; startX: number; startW: number } | null = $state(null);

  function startResize(e: MouseEvent, key: SortKey) {
    e.preventDefault();
    e.stopPropagation();
    resizing = { key, startX: e.clientX, startW: colW[key] };
  }

  function onResizeMove(e: MouseEvent) {
    if (!resizing) return;
    const col = COLUMNS.find((c) => c.key === resizing!.key);
    if (!col) return;
    colW[resizing.key] = Math.max(col.min, resizing.startW + e.clientX - resizing.startX);
  }

  const KIND_ICONS: Record<string, string> = {
    Folder: "folder2",
    Executable: "gear",
    Archive: "file-earmark-zip",
    Image: "image",
    Video: "film",
    Audio: "music-note-beamed",
    Document: "file-earmark-text",
    Code: "file-earmark-code",
    File: "file-earmark",
  };

  let scroller: HTMLDivElement | undefined = $state();
  let viewH = $state(0);
  let scrollTop = $state(0);

  let rows: RowDto[] = $state([]);
  let windowStart = $state(0);

  const naturalH = $derived(session.total * ROW_H);
  const proportional = $derived(naturalH > MAX_SPACER);
  const spacerH = $derived(Math.min(naturalH, MAX_SPACER));
  const visibleCount = $derived(Math.ceil(viewH / ROW_H) + 1);

  /** The first row the viewport shows, under either scroll mode. */
  const firstVisible = $derived.by(() => {
    if (!proportional) return Math.floor(scrollTop / ROW_H);
    const maxScroll = Math.max(1, spacerH - viewH);
    const maxStart = Math.max(0, session.total - visibleCount);
    return Math.min(maxStart, Math.floor((scrollTop / maxScroll) * maxStart));
  });

  // Refetch whenever the viewport, the scroll position, or the view itself
  // changes. One request in flight at a time; a superseded request re-runs.
  let fetching = false;
  let refetchWanted = false;
  $effect(() => {
    void session.viewEpoch;
    void firstVisible;
    void visibleCount;
    void fetchWindow();
  });

  async function fetchWindow() {
    if (fetching) {
      refetchWanted = true;
      return;
    }
    fetching = true;
    try {
      const first = Math.max(0, firstVisible - OVERSCAN);
      const count = visibleCount + OVERSCAN * 2;
      const win = await getRows(first, count);
      rows = win.rows;
      windowStart = win.offset;
    } finally {
      fetching = false;
      if (refetchWanted) {
        refetchWanted = false;
        void fetchWindow();
      }
    }
  }

  /** Rows to draw and where the layer sits, per scroll mode. */
  const drawn = $derived.by(() => {
    if (!proportional) {
      return { layerY: windowStart * ROW_H, rows, firstRow: windowStart };
    }
    // Proportional: draw from the mapped first row, locked to the viewport.
    const skip = Math.max(0, firstVisible - windowStart);
    return { layerY: scrollTop, rows: rows.slice(skip), firstRow: firstVisible };
  });

  function onScroll() {
    scrollTop = scroller?.scrollTop ?? 0;
  }

  /** Split a name into plain/bold segments along the match ranges (UTF-16
   *  offsets straight from Rust, so `slice` needs no conversion). */
  function segments(
    name: string,
    ranges: [number, number][],
  ): { text: string; hit: boolean }[] {
    if (ranges.length === 0) return [{ text: name, hit: false }];
    const out: { text: string; hit: boolean }[] = [];
    let at = 0;
    for (const [start, end] of ranges) {
      if (start > at) out.push({ text: name.slice(at, start), hit: false });
      out.push({ text: name.slice(start, end), hit: true });
      at = end;
    }
    if (at < name.length) out.push({ text: name.slice(at), hit: false });
    return out;
  }

  function onRowClick(e: MouseEvent, globalRow: number) {
    if (e.shiftKey) {
      selectRange(globalRow);
    } else if (e.ctrlKey || e.metaKey) {
      toggleSelect(globalRow);
    } else {
      selectOnly(globalRow);
    }
  }

</script>

<svelte:window
  onmousemove={resizing ? onResizeMove : undefined}
  onmouseup={resizing ? () => (resizing = null) : undefined}
/>

<div class="list">
  <div class="hscroll">
    <div class="inner">
      <div class="header" style:grid-template-columns={template} role="row">
        {#each COLUMNS as col (col.key)}
          <button
            class="head"
            class:numeric={col.numeric}
            title="Sort by {col.label}"
            onclick={() => sortBy(col.key)}
          >
            <span class="head-label">{col.label}</span>
            {#if session.sortKey === col.key}
              <Icon name={session.sortAsc ? "caret-up-fill" : "caret-down-fill"} size={10} />
            {/if}
            <!-- The grip is mouse-only by design: keyboard users don't
                 resize columns, they sort via the header button. -->
            <!-- svelte-ignore a11y_no_static_element_interactions -->
            <!-- svelte-ignore a11y_click_events_have_key_events -->
            <span
              class="grip"
              onmousedown={(e) => startResize(e, col.key)}
              onclick={(e) => e.stopPropagation()}
            ></span>
          </button>
        {/each}
        <span></span>
      </div>

      <div
        class="scroller"
        bind:this={scroller}
        bind:clientHeight={viewH}
        onscroll={onScroll}
        role="grid"
        tabindex="-1"
        aria-rowcount={session.total}
      >
    <div class="spacer" style:height="{spacerH}px"></div>
    <div class="layer" style:transform="translateY({drawn.layerY}px)">
      {#each drawn.rows as row, i (drawn.firstRow + i)}
        {@const globalRow = drawn.firstRow + i}
        <div
          class="row"
          class:selected={isSelected(globalRow)}
          class:dimmed={row.is_hidden || row.is_system}
          class:deleted={session.deletedNodes.has(`${row.vol}:${row.id}`)}
          style:grid-template-columns={template}
          style:height="{ROW_H}px"
          role="row"
          tabindex={-1}
          aria-rowindex={globalRow + 1}
          onmousedown={(e) => onRowClick(e, globalRow)}
          oncontextmenu={(e) => {
            e.preventDefault();
            if (!row.is_synthetic) void showContextMenu(row.vol, row.id);
          }}
        >
          <span class="cell name" title={row.name}>
            <Icon
              name={KIND_ICONS[row.kind_label] ?? "file-earmark"}
              color={row.category > 0 ? `var(--category-${row.category})` : "var(--text-muted)"}
            />
            <span class="label"
              >{#each segments(row.name, row.match_ranges) as seg, i (i)}{#if seg.hit}<b
                    class="match">{seg.text}</b
                  >{:else}{seg.text}{/if}{/each}</span
            >
            {#if row.is_alias}<span class="badge" title="Hard link - bytes counted under another name">link</span>{/if}
            {#if row.is_synthetic}<span class="badge" title="Not a file on the volume">virtual</span>{/if}
            {#if row.is_reparse}<span class="badge" title="Reparse point / junction - not followed">junction</span>{/if}
          </span>
          <span class="cell numeric">{formatSize(row.size)}</span>
          <span class="cell numeric">{formatSize(row.allocated)}</span>
          <span class="cell muted">{row.extension}</span>
          <span class="cell muted">{row.modified_display}</span>
          <span class="cell muted">{row.kind_label}</span>
          <span class="cell muted" title={row.dir_path}>{row.dir_path}</span>
        </div>
      {/each}
    </div>

    {#if session.total === 0}
      <div class="empty">
        {#if session.volumes.some((v) => v.scanned)}
          No matches.
        {:else}
          Scan a drive to get started.
        {/if}
      </div>
    {/if}
      </div>
    </div>
  </div>
</div>

<style>
  .list {
    display: flex;
    flex-direction: column;
    min-height: 0;
    flex: 1;
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface-sunken);
  }

  .hscroll {
    flex: 1;
    min-height: 0;
    overflow-x: auto;
    overflow-y: hidden;
    display: flex;
  }
  .inner {
    flex: 1;
    min-width: min-content;
    display: flex;
    flex-direction: column;
    min-height: 0;
  }

  .header {
    display: grid;
    align-items: stretch;
    border-bottom: 1px solid var(--border);
    background: var(--surface-raised);
    flex: 0 0 auto;
    user-select: none;
  }
  .head {
    position: relative;
    display: flex;
    align-items: center;
    gap: var(--space-1);
    height: 26px;
    padding: 0 var(--space-2);
    border: none;
    border-right: 1px solid var(--border-subtle);
    background: none;
    color: var(--text-secondary);
    font-family: inherit;
    font-size: var(--font-size-caption);
    cursor: pointer;
    overflow: hidden;
    white-space: nowrap;
  }
  .head:hover {
    color: var(--text-primary);
    background: var(--surface-hover);
  }
  .head.numeric {
    justify-content: flex-end;
  }
  .head-label {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .grip {
    position: absolute;
    top: 0;
    right: -3px;
    width: 7px;
    height: 100%;
    cursor: col-resize;
    z-index: 2;
  }

  .scroller {
    position: relative;
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    overflow-x: hidden;
  }

  .spacer {
    width: 1px;
  }

  .layer {
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
    will-change: transform;
  }

  .row {
    display: grid;
    align-items: center;
    border-bottom: 1px solid var(--border-subtle);
    cursor: default;
    user-select: none;
  }
  .row:hover {
    background: var(--surface-hover);
  }
  .row.selected {
    /* Selection blue, not brand orange: "the user picked this" (sec 2.2). */
    background: var(--selection-soft);
    outline: 1px solid var(--selection);
    outline-offset: -1px;
  }
  .row.dimmed .label {
    color: var(--text-secondary);
  }
  /* Observed deleted since the scan (M5): flagged, not removed. */
  .row.deleted .label {
    color: var(--danger);
    text-decoration: line-through;
  }

  .cell {
    padding: 0 var(--space-2);
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
    color: var(--text-primary);
    font-size: var(--font-size-body);
  }
  .cell.numeric {
    text-align: right;
    font-variant-numeric: tabular-nums;
  }
  .cell.muted {
    color: var(--text-secondary);
    font-size: var(--font-size-caption);
  }
  .cell.name {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .cell.name .label {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  /* The spans of the name the query matched. */
  .label .match {
    font-weight: var(--font-weight-bold, 700);
  }

  .badge {
    flex: 0 0 auto;
    padding: 0 var(--space-1);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-small);
    color: var(--text-muted);
    font-size: var(--font-size-caption);
  }

  .empty {
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
    color: var(--text-muted);
    font-size: var(--font-size-body-lg);
    pointer-events: none;
  }
</style>
