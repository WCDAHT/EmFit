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
  import { session, queryChanged, sortBy } from "../session.svelte";

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

  /** Space the rest of the view keeps at minimum — controls, a usable
   *  slice of the folder tree, status bar. Must match the drag clamp. */
  const RESERVED_ABOVE = 240;
  const MIN_MAP_H = 140;

  let innerH = $state(typeof window === "undefined" ? 800 : window.innerHeight);

  /** The splitter stores the DESIRED height; what renders is re-clamped
   *  against the live window height, so shrinking the window can never let
   *  the map swallow the folder tree — and growing it back restores the
   *  user's chosen height untouched. */
  const mapHeight = $derived(
    session.treemapHeight === null
      ? null
      : Math.max(MIN_MAP_H, Math.min(session.treemapHeight, innerH - RESERVED_ABOVE)),
  );

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
    session.treemapHeight = Math.max(
      MIN_MAP_H,
      Math.min(height, window.innerHeight - RESERVED_ABOVE),
    );
  }
</script>

<svelte:window
  bind:innerHeight={innerH}
  onmousemove={dragging ? onDragMove : undefined}
  onmouseup={dragging ? onDragEnd : undefined}
/>

<!-- Depth, color mode, and units now live in the Settings dialog (native
     View menu) — the toolbar keeps only view jumps. The treemap-depth and
     size-mode session state stays alive underneath. -->
<div class="controls">
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
  style:flex={mapHeight === null ? "1.2 1 0" : "0 0 auto"}
  style:height={mapHeight === null ? undefined : `${mapHeight}px`}
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

<style>
  .controls {
    display: flex;
    align-items: center;
    gap: var(--space-3);
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
