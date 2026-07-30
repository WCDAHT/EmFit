<!--
  FolderTree.svelte — the WizTree-style hierarchical pane (features.md §4.1),
  rendered as a full tree TABLE: Name, Allocated, Size, % of Parent (bar),
  Items, Files, Folders, Modified, Attributes. Every column sorts (click the
  header) and resizes (drag the header edge). Both size columns are always
  visible — there is no allocated/logical toggle.

  Hand-rolled on purpose: as of 2026-07 no widely-known tree-table library
  is production-ready on Svelte 5 (AG Grid tree data is Enterprise-only;
  TanStack Table's Svelte adapter is Svelte-4-era, its v9 an alpha), and the
  hard parts — row virtualization and lazy child materialization from CSR
  ranges — were already built here. Rows come from Rust one directory at a
  time and are flattened into one array; the visible slice is windowed, so a
  directory with 100k children expands without stalling. Sorting reorders
  each sibling group client-side (the data is already resident per level).

  Sync with the treemap: clicking a row focuses it (highlight in the map),
  a click in the map focuses here and expands the path down to the node.
-->
<script lang="ts">
  import { onMount } from "svelte";
  import { nodeLineage, treeChildren, treeRoots } from "../ipc";
  import { session } from "../session.svelte";
  import { formatSize } from "../format";
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

  // ---------------------------------------------------------------------
  // columns: widths (resizable) + sort
  // ---------------------------------------------------------------------

  type ColKey =
    | "name"
    | "allocated"
    | "size"
    | "percent"
    | "items"
    | "files"
    | "dirs"
    | "modified"
    | "attributes";

  interface Col {
    key: ColKey;
    label: string;
    w: number;
    min: number;
    numeric?: boolean;
    /** Direction a fresh click sorts in. Sizes/counts start descending
     *  (WizTree's default); text and dates start ascending. */
    descFirst?: boolean;
  }

  const COLS: Col[] = [
    { key: "name", label: "Name", w: 280, min: 140 },
    { key: "allocated", label: "Allocated", w: 92, min: 56, numeric: true, descFirst: true },
    { key: "size", label: "Size", w: 92, min: 56, numeric: true, descFirst: true },
    { key: "percent", label: "% of Parent", w: 138, min: 40, numeric: true, descFirst: true },
    { key: "items", label: "Items", w: 80, min: 48, numeric: true, descFirst: true },
    { key: "files", label: "Files", w: 80, min: 48, numeric: true, descFirst: true },
    { key: "dirs", label: "Folders", w: 80, min: 48, numeric: true, descFirst: true },
    { key: "modified", label: "Modified", w: 118, min: 80 },
    { key: "attributes", label: "Attributes", w: 78, min: 48 },
  ];

  let colW = $state<Record<ColKey, number>>(
    Object.fromEntries(COLS.map((c) => [c.key, c.w])) as Record<ColKey, number>,
  );
  /** One shared grid template keeps the header and every row aligned; the
   *  trailing 1fr soaks up leftover width. */
  const template = $derived(COLS.map((c) => `${colW[c.key]}px`).join(" ") + " minmax(0, 1fr)");

  let resizing: { key: ColKey; startX: number; startW: number } | null = $state(null);

  function startResize(e: MouseEvent, key: ColKey) {
    e.preventDefault();
    e.stopPropagation();
    resizing = { key, startX: e.clientX, startW: colW[key] };
  }

  function onResizeMove(e: MouseEvent) {
    if (!resizing) return;
    const col = COLS.find((c) => c.key === resizing!.key);
    if (!col) return;
    colW[resizing.key] = Math.max(col.min, resizing.startW + e.clientX - resizing.startX);
  }

  let sortKey = $state<ColKey>("allocated");
  let sortAsc = $state(false);

  function sortBy(key: ColKey) {
    if (sortKey === key) {
      sortAsc = !sortAsc;
    } else {
      sortKey = key;
      sortAsc = !COLS.find((c) => c.key === key)?.descFirst;
    }
    rebuild();
  }

  /** Order one sibling group by the active column. Ties fall back to
   *  allocated descending, so equal cells keep the size-dominant order. */
  function sorted(rows: TreeRowDto[]): TreeRowDto[] {
    const dir = sortAsc ? 1 : -1;
    const cmp = (a: TreeRowDto, b: TreeRowDto): number => {
      let d: number;
      switch (sortKey) {
        case "name":
          d = a.name.localeCompare(b.name, undefined, { sensitivity: "base" });
          break;
        case "size":
          d = a.size - b.size;
          break;
        case "percent":
          d = a.percent_of_parent - b.percent_of_parent;
          break;
        case "items":
          d = a.items - b.items;
          break;
        case "files":
          d = a.files - b.files;
          break;
        case "dirs":
          d = a.dirs - b.dirs;
          break;
        case "modified":
          d = a.modified - b.modified;
          break;
        case "attributes":
          d = a.attributes.localeCompare(b.attributes);
          break;
        default:
          d = a.allocated - b.allocated;
      }
      return d * dir || b.allocated - a.allocated;
    };
    return [...rows].sort(cmp);
  }

  // ---------------------------------------------------------------------
  // data: lazy load + flatten
  // ---------------------------------------------------------------------

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
      for (const row of sorted(rows)) {
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

  // Double-click deliberately does NOT drill the treemap (UI direction
  // 2026-07-26): the map re-roots only from its own gestures. Double-click
  // just expands/collapses, like every other tree control.
  function onRowDblClick(row: TreeRowDto) {
    void toggle(row);
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
    } else if (e.key === "Enter" && row.has_children) {
      e.preventDefault();
      void toggle(row);
    }
  }

  const first = $derived(Math.max(0, Math.floor(scrollTop / ROW_H) - OVERSCAN));
  const count = $derived(Math.ceil(viewH / ROW_H) + OVERSCAN * 2);
  const windowRows = $derived(flat.slice(first, first + count));
</script>

<svelte:window
  onmousemove={resizing ? onResizeMove : undefined}
  onmouseup={resizing ? () => (resizing = null) : undefined}
/>

<div
  class="tree"
  role="tree"
  tabindex="0"
  aria-label="Folder tree"
  onkeydown={onKeydown}
>
  <div class="hscroll">
    <div class="inner">
      <div class="header" style:grid-template-columns={template} role="row">
        {#each COLS as col (col.key)}
          <button
            class="head"
            class:numeric={col.numeric}
            onclick={() => sortBy(col.key)}
            title="Sort by {col.label}"
          >
            <span class="head-label">{col.label}</span>
            {#if sortKey === col.key}
              <Icon name={sortAsc ? "caret-up-fill" : "caret-down-fill"} size={10} />
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
              style:grid-template-columns={template}
              role="treeitem"
              aria-selected={focused}
              aria-expanded={row.has_children ? open.has(keyOf(row)) : undefined}
              tabindex={-1}
              onmousedown={() => onRowClick(row)}
              ondblclick={() => onRowDblClick(row)}
            >
              <span class="cell name-cell">
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
              </span>
              <span class="cell numeric">{formatSize(row.allocated)}</span>
              <span class="cell numeric">{formatSize(row.size)}</span>
              <span class="cell percent" title="{row.percent_of_parent.toFixed(1)}% of parent">
                <span class="bar">
                  <span class="fill" style:width="{Math.min(100, row.percent_of_parent)}%"></span>
                </span>
                {#if colW.percent >= 92}
                  <span class="pct">{row.percent_of_parent.toFixed(1)}%</span>
                {/if}
              </span>
              <span class="cell numeric muted">
                {row.is_dir ? row.items.toLocaleString() : ""}
              </span>
              <span class="cell numeric muted">
                {row.is_dir ? row.files.toLocaleString() : ""}
              </span>
              <span class="cell numeric muted">
                {row.is_dir ? row.dirs.toLocaleString() : ""}
              </span>
              <span class="cell muted">{row.modified_display}</span>
              <span class="cell muted">{row.attributes}</span>
            </div>
          {/each}
        </div>

        {#if flat.length === 0}
          <div class="empty">Scan a drive to populate the tree.</div>
        {/if}
      </div>
    </div>
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

  .cell {
    display: flex;
    align-items: center;
    min-width: 0;
    padding: 0 var(--space-2);
    overflow: hidden;
    white-space: nowrap;
  }
  .cell.numeric {
    justify-content: flex-end;
    font-variant-numeric: tabular-nums;
  }
  .cell.muted {
    color: var(--text-secondary);
    font-size: var(--font-size-caption);
  }

  .name-cell {
    gap: var(--space-2);
    padding-left: var(--space-2);
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
    min-width: 0;
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }

  .percent {
    gap: var(--space-2);
  }
  .bar {
    flex: 1 1 auto;
    min-width: 12px;
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
    flex: 0 0 44px;
    text-align: right;
    color: var(--text-secondary);
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
