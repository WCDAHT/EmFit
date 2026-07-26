<!--
  FolderTree.svelte — the WizTree-style hierarchical pane (features.md §4.1).

  Rows come from Rust one directory at a time (lazy CSR materialization) and
  are flattened into one array for rendering; the visible slice is windowed
  the same way as the file list, so a directory with 100k children expands
  without stalling. Each row shows an inline proportional bar of its share
  of the parent.

  Sync with the treemap: clicking a row focuses it (highlight in the map),
  double-clicking a directory drills the map into it; a click in the map
  focuses here and expands the path down to the node.
-->
<script lang="ts">
  import { onMount } from "svelte";
  import { nodeLineage, treeChildren, treeRoots } from "../ipc";
  import { session } from "../session.svelte";
  import type { TreeRowDto } from "../types";
  import Icon from "../components/Icon.svelte";

  const ROW_H = 26;
  const OVERSCAN = 10;

  interface FlatRow {
    row: TreeRowDto;
    depth: number;
  }

  let flat: FlatRow[] = $state([]);
  const expanded = new Map<string, TreeRowDto[]>();
  let open = $state(new Set<string>());

  let scroller: HTMLDivElement | undefined = $state();
  let viewH = $state(0);
  let scrollTop = $state(0);

  const keyOf = (r: { vol: number; id: number }) => `${r.vol}:${r.id}`;

  onMount(() => void reload());

  // Rescans invalidate every id in the pane.
  $effect(() => {
    void session.viewEpoch;
    void reload();
  });

  // A treemap click asks the tree to reveal the focused node.
  $effect(() => {
    void session.revealEpoch;
    if (session.focus) void reveal(session.focus.vol, session.focus.id);
  });

  async function reload() {
    expanded.clear();
    open = new Set();
    const roots = await treeRoots();
    // A single volume starts opened one level — the useful state.
    if (roots.length === 1 && roots[0].has_children) {
      await toggleLoad(roots[0]);
      open.add(keyOf(roots[0]));
      open = new Set(open);
    }
    rebuild(roots);
  }

  let rootRows: TreeRowDto[] = [];

  function rebuild(roots?: TreeRowDto[]) {
    if (roots) rootRows = roots;
    const out: FlatRow[] = [];
    const walk = (rows: TreeRowDto[], depth: number) => {
      for (const row of rows) {
        out.push({ row, depth });
        const key = keyOf(row);
        if (open.has(key)) {
          const kids = expanded.get(key);
          if (kids) walk(kids, depth + 1);
        }
      }
    };
    walk(rootRows, 0);
    flat = out;
  }

  async function toggleLoad(row: TreeRowDto) {
    const key = keyOf(row);
    if (!expanded.has(key)) {
      expanded.set(key, await treeChildren(row.vol, row.id));
    }
  }

  async function toggle(row: TreeRowDto) {
    if (!row.has_children) return;
    const key = keyOf(row);
    if (open.has(key)) {
      open.delete(key);
    } else {
      await toggleLoad(row);
      open.add(key);
    }
    open = new Set(open);
    rebuild();
  }

  /** Expand ancestors so `vol:id` is visible, then focus-scroll to it. */
  async function reveal(vol: number, id: number) {
    const chain = await nodeLineage(vol, id);
    if (chain.length === 0) return;

    // Expand each ancestor in order (all but the last id).
    let rows: TreeRowDto[] = rootRows;
    for (let i = 0; i < chain.length - 1; i++) {
      const row = rows.find((r) => r.vol === vol && r.id === chain[i]);
      if (!row || !row.has_children) break;
      const key = keyOf(row);
      if (!expanded.has(key)) {
        expanded.set(key, await treeChildren(vol, row.id));
      }
      open.add(key);
      rows = expanded.get(key) ?? [];
    }
    open = new Set(open);
    rebuild();

    const at = flat.findIndex((f) => f.row.vol === vol && f.row.id === id);
    if (at >= 0 && scroller) {
      const y = at * ROW_H;
      if (y < scroller.scrollTop || y > scroller.scrollTop + viewH - ROW_H) {
        scroller.scrollTop = Math.max(0, y - viewH / 2);
      }
    }
  }

  function onRowClick(row: TreeRowDto) {
    session.focus = { vol: row.vol, id: row.id };
  }

  function onRowDblClick(row: TreeRowDto) {
    if (row.is_dir) {
      session.drill = { vol: row.vol, id: row.id };
    }
  }

  function onKeydown(e: KeyboardEvent) {
    if (!session.focus) return;
    const at = flat.findIndex(
      (f) => f.row.vol === session.focus?.vol && f.row.id === session.focus?.id,
    );
    if (at < 0) return;
    const row = flat[at].row;

    if (e.key === "ArrowDown" && at + 1 < flat.length) {
      e.preventDefault();
      session.focus = { vol: flat[at + 1].row.vol, id: flat[at + 1].row.id };
    } else if (e.key === "ArrowUp" && at > 0) {
      e.preventDefault();
      session.focus = { vol: flat[at - 1].row.vol, id: flat[at - 1].row.id };
    } else if (e.key === "ArrowRight" && row.has_children && !open.has(keyOf(row))) {
      e.preventDefault();
      void toggle(row);
    } else if (e.key === "ArrowLeft" && open.has(keyOf(row))) {
      e.preventDefault();
      void toggle(row);
    } else if (e.key === "Enter" && row.is_dir) {
      e.preventDefault();
      session.drill = { vol: row.vol, id: row.id };
    }
  }

  const first = $derived(Math.max(0, Math.floor(scrollTop / ROW_H) - OVERSCAN));
  const count = $derived(Math.ceil(viewH / ROW_H) + OVERSCAN * 2);
  const windowRows = $derived(flat.slice(first, first + count));

  function primary(row: TreeRowDto): string {
    return session.sizeMode === "allocated" ? row.allocated_display : row.size_display;
  }
</script>

<div
  class="tree"
  role="tree"
  tabindex="0"
  aria-label="Folder tree"
  onkeydown={onKeydown}
>
  <div
    class="scroller"
    bind:this={scroller}
    bind:clientHeight={viewH}
    onscroll={() => (scrollTop = scroller?.scrollTop ?? 0)}
  >
    <div class="spacer" style:height="{flat.length * ROW_H}px"></div>
    <div class="layer" style:transform="translateY({first * ROW_H}px)">
      {#each windowRows as f (keyOf(f.row))}
        {@const row = f.row}
        {@const focused = session.focus?.vol === row.vol && session.focus?.id === row.id}
        <div
          class="row"
          class:focused
          class:synthetic={row.synthetic}
          style:height="{ROW_H}px"
          role="treeitem"
          aria-selected={focused}
          aria-expanded={row.has_children ? open.has(keyOf(row)) : undefined}
          tabindex={-1}
          onmousedown={() => onRowClick(row)}
          ondblclick={() => onRowDblClick(row)}
        >
          <span class="indent" style:width="{f.depth * 16}px"></span>
          <button
            class="arrow"
            class:hidden={!row.has_children}
            tabindex={-1}
            onmousedown={(e) => e.stopPropagation()}
            onclick={() => void toggle(row)}
          >
            <Icon name={open.has(keyOf(row)) ? "chevron-down" : "chevron-right"} size={11} />
          </button>
          <Icon
            name={row.is_dir ? "folder2" : "file-earmark"}
            color={row.is_dir ? "var(--category-1)" : "var(--text-muted)"}
          />
          <span class="name" title={row.name}>{row.name}</span>
          <span class="bar" title="{row.percent_of_parent.toFixed(1)}% of parent">
            <span class="fill" style:width="{Math.min(100, row.percent_of_parent)}%"></span>
          </span>
          <span class="pct">{row.percent_of_parent.toFixed(1)}%</span>
          <span class="size">{primary(row)}</span>
          <span class="counts">
            {#if row.is_dir}{row.files.toLocaleString()} files{/if}
          </span>
        </div>
      {/each}
    </div>

    {#if flat.length === 0}
      <div class="empty">Scan a drive to populate the tree.</div>
    {/if}
  </div>
</div>

<style>
  .tree {
    display: flex;
    flex-direction: column;
    min-height: 0;
    flex: 1;
    border: 1px solid var(--border);
    border-radius: var(--radius-small);
    background: var(--surface-sunken);
    outline: none;
  }
  .tree:focus-visible {
    border-color: var(--selection);
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
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: 0 var(--space-2);
    border-bottom: 1px solid var(--border-subtle);
    font-size: var(--font-size-body);
    color: var(--text-primary);
    cursor: default;
    user-select: none;
  }
  .row:hover {
    background: var(--surface-hover);
  }
  .row.focused {
    background: var(--selection-soft);
    outline: 1px solid var(--selection);
    outline-offset: -1px;
  }
  .row.synthetic .name {
    color: var(--text-muted);
    font-style: italic;
  }

  .indent {
    flex: 0 0 auto;
  }

  .arrow {
    flex: 0 0 auto;
    display: grid;
    place-items: center;
    width: 16px;
    height: 16px;
    border: none;
    background: none;
    color: var(--text-secondary);
    cursor: pointer;
  }
  .arrow.hidden {
    visibility: hidden;
  }

  .name {
    flex: 1 1 auto;
    min-width: 80px;
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }

  .bar {
    flex: 0 0 120px;
    height: 10px;
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

  .pct {
    flex: 0 0 52px;
    text-align: right;
    color: var(--text-secondary);
    font-size: var(--font-size-caption);
    font-variant-numeric: tabular-nums;
  }

  .size {
    flex: 0 0 84px;
    text-align: right;
    font-variant-numeric: tabular-nums;
  }

  .counts {
    flex: 0 0 90px;
    text-align: right;
    color: var(--text-muted);
    font-size: var(--font-size-caption);
    font-variant-numeric: tabular-nums;
  }

  .empty {
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
    color: var(--text-muted);
    pointer-events: none;
  }
</style>
