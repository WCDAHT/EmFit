// Cross-view shared state (STANDARDS §3.3): the search bar, file list, scan
// bar, and status bar all read and write this one runed module instead of
// threading props through the tree.
//
// This is a *projection* of what the Rust side knows — the shell owns truth.
// Query fields here are just what the inputs hold; pushing them through
// `set_query` is what makes them real, and the `view:updated` event is what
// updates the result metadata.

import { SvelteSet } from "svelte/reactivity";
import { selectionSummary, setQuery, setSort } from "./ipc";
import type {
  NodeRef,
  RawQueryDto,
  ScanTarget,
  SelectionSummaryDto,
  SizeRange,
  SortKey,
  VolumeDto,
} from "./types";

/** Per-volume scan progress, driven by the `scan:progress` / `scan:done`
 *  events. `phase` is what the inline status renders from. */
export interface VolumeScanState {
  phase: "scanning" | "done" | "error";
  message: string;
  done: number;
  total: number | null;
  error?: string;
  summary?: string;
}

export const session = $state({
  // --- which tab is showing ---
  tab: "list" as "list" | "tree",

  // --- query inputs (what the search UI holds) ---
  text: "",
  regex: "",
  size: "",
  modified: "",
  extensions: "",
  preset: "",
  includeHidden: true,
  includeSystem: true,
  caseSensitive: false,
  filtersOpen: false,

  // --- sort ---
  sortKey: "size" as SortKey,
  sortAsc: false,

  // --- result view (from `view:updated`) ---
  generation: 0,
  total: 0,
  elapsedMs: 0,
  warnings: [] as string[],
  volumesTotalDisplay: "",
  /** Bumped whenever the view changes so the list refetches its window. */
  viewEpoch: 0,

  // --- selection (positions in the current view) ---
  selected: new SvelteSet<number>(),
  anchor: -1,
  allSelected: false,
  summary: null as SelectionSummaryDto | null,

  // --- tree view (features.md §4) ---
  /** Treemap drill point; null = all volumes in one map. */
  drill: null as NodeRef | null,
  /** The focused node, shared by the folder tree and the treemap. */
  focus: null as NodeRef | null,
  /** Bumped when focus changes from the treemap so the tree reveals it. */
  revealEpoch: 0,
  treemapDepth: 0,
  /** Coloring rule, mirroring the persisted `config.treemap`: "ranked" =
   *  WizTree's palette assigned to extensions by total-size rank;
   *  "extension" = the category palette (per-extension configurable list is
   *  a future milestone). */
  colorMode: "ranked" as "ranked" | "extension",
  /** Legacy size buckets from config (retired mode); kept only so saves
   *  round-trip the persisted data. */
  sizeRanges: [] as SizeRange[],
  /** Fixed height of the treemap row, set by the splitter on drag release;
   *  null = the default flex split. */
  treemapHeight: null as number | null,
  /** Show logical size or size on disk as the primary number. The treemap's
   *  geometry always uses allocated (features.md §4.2). */
  sizeMode: "allocated" as "allocated" | "logical",
  /** Extensions ticked in the type breakdown; dims the treemap and feeds the
   *  list's extension filter. */
  typeFilter: new SvelteSet<string>(),

  // --- scanning ---
  scanning: false,
  volumes: [] as VolumeDto[],
  /** The scan list the Sources popup manages: volumes and disk images. */
  targets: [] as ScanTarget[],
  sourcesOpen: false,
  scan: {} as Record<string, VolumeScanState>,
  elevated: true,
});

/** Add to the scan list (no duplicates). */
export function addTarget(target: ScanTarget) {
  if (!session.targets.some((t) => t.key === target.key)) {
    session.targets.push(target);
  }
}

export function removeTarget(key: string) {
  session.targets = session.targets.filter((t) => t.key !== key);
  delete session.scan[key];
}

/** Imperative hooks views register so root-level shortcuts can reach them
 *  (STANDARDS §3.7: all shortcuts dispatched from the app root). */
export const hooks: {
  focusSearch?: () => void;
  rescan?: () => void;
} = {};

// ---------------------------------------------------------------------------
// query
// ---------------------------------------------------------------------------

function rawQuery(): RawQueryDto {
  return {
    text: session.text,
    regex: session.regex,
    size: session.size,
    modified: session.modified,
    extensions: session.extensions,
    preset: session.preset,
    include_hidden: session.includeHidden,
    include_system: session.includeSystem,
    case_sensitive: session.caseSensitive ? true : null,
  };
}

let queryTimer: ReturnType<typeof setTimeout> | undefined;

/** Push the query after a short debounce — the Rust search is interruptible,
 *  but not starting a doomed search at all is cheaper still. */
export function queryChanged(immediate = false) {
  clearTimeout(queryTimer);
  if (immediate) {
    void setQuery(rawQuery());
  } else {
    queryTimer = setTimeout(() => void setQuery(rawQuery()), 50);
  }
}

/** True when anything beyond plain text constrains the view — drives the
 *  "filters active" indicator (features.md §2). */
export function filtersActive(): boolean {
  return (
    session.regex !== "" ||
    session.size !== "" ||
    session.modified !== "" ||
    session.extensions !== "" ||
    session.preset !== "" ||
    session.caseSensitive ||
    !session.includeHidden ||
    !session.includeSystem
  );
}

/** The "clear all filters" action. Keeps the text — clearing what the user
 *  is actively typing would be hostile. */
export function clearFilters() {
  session.regex = "";
  session.size = "";
  session.modified = "";
  session.extensions = "";
  session.preset = "";
  session.caseSensitive = false;
  session.includeHidden = true;
  session.includeSystem = true;
  queryChanged(true);
}

export function sortBy(key: SortKey) {
  if (session.sortKey === key) {
    session.sortAsc = !session.sortAsc;
  } else {
    session.sortKey = key;
    // Size-like columns start descending (largest first); text ascending.
    session.sortAsc = !(key === "size" || key === "allocated" || key === "modified");
  }
  void setSort(session.sortKey, session.sortAsc);
}

// ---------------------------------------------------------------------------
// selection
// ---------------------------------------------------------------------------

/** How many rows a shift-range may cover. Beyond this, select-all semantics
 *  are what the user wants anyway. */
const MAX_RANGE = 1_000_000;

export function clearSelection() {
  session.selected.clear();
  session.allSelected = false;
  session.anchor = -1;
  session.summary = null;
}

export function selectOnly(row: number) {
  session.selected.clear();
  session.allSelected = false;
  session.selected.add(row);
  session.anchor = row;
  refreshSummary();
}

export function toggleSelect(row: number) {
  session.allSelected = false;
  if (session.selected.has(row)) {
    session.selected.delete(row);
  } else {
    session.selected.add(row);
  }
  session.anchor = row;
  refreshSummary();
}

export function selectRange(row: number) {
  session.allSelected = false;
  const anchor = session.anchor >= 0 ? session.anchor : row;
  const lo = Math.min(anchor, row);
  const hi = Math.min(Math.max(anchor, row), lo + MAX_RANGE);
  session.selected.clear();
  for (let i = lo; i <= hi; i++) session.selected.add(i);
  refreshSummary();
}

export function selectAll() {
  session.selected.clear();
  session.allSelected = true;
  refreshSummary();
}

export function selectionCount(): number {
  return session.allSelected ? session.total : session.selected.size;
}

export function isSelected(row: number): boolean {
  return session.allSelected || session.selected.has(row);
}

let summaryTimer: ReturnType<typeof setTimeout> | undefined;

/** Ask Rust for the selection totals, debounced — shift-drag selection would
 *  otherwise fire a summary per row. */
export function refreshSummary() {
  clearTimeout(summaryTimer);
  summaryTimer = setTimeout(async () => {
    if (!session.allSelected && session.selected.size === 0) {
      session.summary = null;
      return;
    }
    session.summary = await selectionSummary(
      session.allSelected ? [] : [...session.selected],
      session.allSelected,
    );
  }, 100);
}
