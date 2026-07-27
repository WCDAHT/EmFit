<!--
  TreeView.svelte — the space-analysis tab (roadmap M3): folder tree on top,
  treemap below, file types beside it. Selection is shared through
  `session.focus`: click a rectangle and the tree reveals it. Drilling the
  map happens only from the map itself (double-click / breadcrumb) — the
  tree never re-roots it.

  The horizontal splitter resizes the treemap row; the new height applies on
  drag *release* (the canvas relayouts once, not per mousemove).
-->
<script lang="ts">
  import FolderTree from "./FolderTree.svelte";
  import Treemap from "./Treemap.svelte";
  import TypesPanel from "./TypesPanel.svelte";
  import TreemapSettings from "../components/TreemapSettings.svelte";
  import { session, queryChanged, sortBy } from "../session.svelte";

  let colorsOpen = $state(false);

  /** The file-types panel is parked until its UX is designed (user call,
   *  2026-07-27). The component and its plumbing stay alive behind this. */
  const SHOW_TYPES_PANEL = false;

  /** "Top files" / "Top folders" (features.md §4.4): the List tab already is
   *  that view once kind-filtered and size-sorted — jump it there. */
  function topN(kind: "file" | "folder") {
    session.text = `${kind}:`;
    session.tab = "list";
    if (session.sortKey !== "size" || session.sortAsc) {
      session.sortKey = "path"; // force sortBy to reset to size desc
      sortBy("size");
    }
    queryChanged(true);
  }

  // --- splitter: applies on release only ---
  let dragging = $state(false);
  let ghostY = $state(0);
  let bottomEl: HTMLDivElement | undefined = $state();

  function startDrag(e: MouseEvent) {
    e.preventDefault();
    dragging = true;
    ghostY = e.clientY;
  }

  function onDragMove(e: MouseEvent) {
    if (dragging) ghostY = e.clientY;
  }

  function onDragEnd(e: MouseEvent) {
    if (!dragging) return;
    dragging = false;
    const bottom = bottomEl?.getBoundingClientRect();
    if (!bottom) return;
    // New treemap-row height = distance from the release point to its bottom.
    const height = Math.round(bottom.bottom - e.clientY);
    session.treemapHeight = Math.max(140, Math.min(height, window.innerHeight - 240));
  }
</script>

<svelte:window
  onmousemove={dragging ? onDragMove : undefined}
  onmouseup={dragging ? onDragEnd : undefined}
/>

<div class="controls">
  <label>
    Depth
    <select bind:value={session.treemapDepth}>
      {#each [2, 3, 4, 5, 6, 8] as d (d)}
        <option value={d}>{d}</option>
      {/each}
      <!-- 0 = unlimited; recursion is pixel-bounded, so "Max" is safe. -->
      <option value={0}>Max</option>
    </select>
  </label>
  <label>
    Color by
    <select
      bind:value={session.colorMode}
      onchange={() => (session.viewEpoch = session.viewEpoch)}
    >
      <option value="size">Size</option>
      <option value="extension">Extension</option>
    </select>
  </label>
  <button class="quick" onclick={() => (colorsOpen = true)}>Colors…</button>
  <label>
    Show
    <select bind:value={session.sizeMode}>
      <option value="allocated">Size on disk</option>
      <option value="logical">Logical size</option>
    </select>
  </label>
  <span class="flex"></span>
  <button class="quick" onclick={() => topN("file")}>Top files</button>
  <button class="quick" onclick={() => topN("folder")}>Top folders</button>
</div>

<FolderTree />

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div
  class="splitter"
  class:active={dragging}
  role="separator"
  aria-orientation="horizontal"
  aria-label="Resize treemap"
  tabindex="-1"
  onmousedown={startDrag}
></div>

<div
  class="bottom"
  bind:this={bottomEl}
  style:flex={session.treemapHeight === null ? "1.2 1 0" : "0 0 auto"}
  style:height={session.treemapHeight === null ? undefined : `${session.treemapHeight}px`}
>
  <Treemap />
  {#if SHOW_TYPES_PANEL}
    <div class="side">
      <TypesPanel />
    </div>
  {/if}
</div>

{#if dragging}
  <div class="ghost" style:top="{ghostY}px"></div>
{/if}

<TreemapSettings open={colorsOpen} onClose={() => (colorsOpen = false)} />

<style>
  .controls {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }
  .controls label {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--text-secondary);
    font-size: var(--font-size-body);
  }
  .controls select {
    height: 24px;
    padding: 0 var(--space-1);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface-raised);
    color: var(--text-primary);
    font-family: inherit;
    font-size: var(--font-size-body);
  }
  .flex {
    flex: 1;
  }
  .quick {
    height: 24px;
    padding: 0 var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface-raised);
    color: var(--text-secondary);
    font-family: inherit;
    font-size: var(--font-size-caption);
    cursor: pointer;
  }
  .quick:hover {
    color: var(--text-primary);
    background: var(--surface-hover);
  }

  .splitter {
    flex: 0 0 auto;
    height: 5px;
    margin: -2px 0;
    cursor: row-resize;
    border-radius: var(--radius-small);
  }
  .splitter:hover,
  .splitter.active {
    background: var(--selection);
    opacity: 0.5;
  }

  .ghost {
    position: fixed;
    left: 0;
    right: 0;
    height: 2px;
    background: var(--selection);
    z-index: 90;
    pointer-events: none;
  }

  .bottom {
    display: flex;
    gap: var(--space-2);
    min-height: 140px;
    min-width: 0;
  }
  .side {
    display: flex;
    flex-direction: column;
    min-height: 0;
    flex: 0 0 340px;
  }
</style>
