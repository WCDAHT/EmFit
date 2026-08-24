//! Typed wrappers around Tauri's `invoke()`. See STANDARDS sec 3.4.
//!
//! The rest of the Svelte code calls these functions, never `invoke("...")`
//! with a raw string. Each function pairs with a `#[tauri::command]` in
//! src-tauri/src/commands.rs.

import { invoke } from "@tauri-apps/api/core";
import type {
  AppConfig,
  CacheUsage,
  NodeInfoDto,
  NodeRef,
  BackgroundStatus,
  FiltersDto,
  SyntaxSectionDto,
  RawQueryDto,
  RowWindowDto,
  ScanTarget,
  SelectionSummaryDto,
  SortKey,
  TreeRowDto,
  TreemapRectDto,
  TypeRowDto,
  VolumeDto,
} from "./types";

/** Forward a webview log line into the Rust `tracing` pipeline (same log file
 *  + format as backend logs). Used by the console bridge; fire-and-forget. */
export function logEvent(level: string, message: string): Promise<void> {
  return invoke("log_event", { level, message });
}

/** Read the persisted application config (theme and any other settings). */
export function getConfig(): Promise<AppConfig> {
  return invoke("get_config");
}

/** Persist the whole application config. Send the value returned by
 *  {@link getConfig} with the changed fields mutated. */
export function setConfig(config: AppConfig): Promise<void> {
  return invoke("set_config", { newConfig: config });
}

/** How much disk the cached scans take, and how many there are. */
export function cacheUsage(): Promise<CacheUsage> {
  return invoke("cache_usage");
}

/** Delete every cached scan; resolves with what is left (nothing). The next
 *  scan of a volume reads its file table the long way and caches that. */
export function clearCache(): Promise<CacheUsage> {
  return invoke("clear_cache");
}

/** Every volume on the machine, with scannability and scanned state. */
export function listVolumes(): Promise<VolumeDto[]> {
  return invoke("list_volumes");
}

/** Whether the process runs elevated (raw volume access needs it). */
export function elevationStatus(): Promise<boolean> {
  return invoke("elevation_status");
}

/** Relaunch the app with a UAC prompt. */
export function relaunchElevated(): Promise<void> {
  return invoke("relaunch_elevated");
}

/** Scan the given targets (volumes in parallel, then disk images).
 *  Progress arrives as `scan:progress` / `scan:done` events keyed by
 *  each target's `key`. */
export function startScan(targets: ScanTarget[]): Promise<void> {
  return invoke("start_scan", {
    targets: targets.map(({ kind, key }) => ({ kind, key })),
  });
}

/** Stop the running scan. */
export function cancelScan(): Promise<void> {
  return invoke("cancel_scan");
}

/** Replace the active query. A `view:updated` event follows when the
 *  interruptible search completes. */
export function setQuery(raw: RawQueryDto): Promise<void> {
  return invoke("set_query", { raw });
}

/** Change the sort column/direction; re-sorts the current results. */
export function setSort(key: SortKey, ascending: boolean): Promise<void> {
  return invoke("set_sort", { sort: { key, ascending } });
}

/** One window of the current view - the only way rows reach the webview
 *  (features.md sec 9: the index never crosses IPC). */
export function getRows(offset: number, count: number): Promise<RowWindowDto> {
  return invoke("get_rows", { offset, count });
}

/** Selection totals. `picks` are positions in the current view; pass
 *  `all: true` for select-all instead of shipping millions of indexes. */
export function selectionSummary(
  picks: number[],
  all: boolean,
): Promise<SelectionSummaryDto> {
  return invoke("selection_summary", { picks, all });
}

/** The `Filters.csv` presets, re-read from disk on every call. */
export function listPresets(): Promise<FiltersDto> {
  return invoke("list_presets");
}

/** Open `Filters.csv` in the OS editor, creating a starter file if needed.
 *  Resolves to the path it opened. */
export function editFilters(): Promise<string> {
  return invoke("edit_filters");
}

/** Register or remove the scheduled background-scan task, to match the saved
 *  config. Prompts for Administrator; resolves to whether it is registered
 *  afterwards. */
export function syncBackgroundTask(): Promise<boolean> {
  return invoke("sync_background_task");
}

/** Whether the task exists, and what the last background run did. */
export function backgroundStatus(): Promise<BackgroundStatus> {
  return invoke("background_status");
}

/** Start the background scan now instead of waiting for its interval. */
export function runBackgroundNow(): Promise<void> {
  return invoke("run_background_now");
}

/** The query language, for the Search syntax dialog. */
export function searchSyntax(): Promise<SyntaxSectionDto[]> {
  return invoke("search_syntax");
}

/** Folder-tree top level: one row per scanned volume. */
export function treeRoots(): Promise<TreeRowDto[]> {
  return invoke("tree_roots");
}

/** One directory's children, largest first (lazy CSR materialization). */
export function treeChildren(vol: number, id: number): Promise<TreeRowDto[]> {
  return invoke("tree_children", { vol, id });
}

/** Root-to-node id chain, for "reveal in tree". */
export function nodeLineage(vol: number, id: number): Promise<number[]> {
  return invoke("node_lineage", { vol, id });
}

/** The squarified treemap for this canvas and drill level, laid out in Rust. */
export function treemapLayout(
  drill: NodeRef | null,
  width: number,
  height: number,
  depth: number,
  showFreeSpace: boolean,
): Promise<TreemapRectDto[]> {
  return invoke("treemap_layout", { drill, width, height, depth, showFreeSpace });
}

/** Aggregate every file by extension. */
export function typeBreakdown(limit: number): Promise<TypeRowDto[]> {
  return invoke("type_breakdown", { limit });
}

/** Tooltip / breadcrumb details for one node. */
export function nodeInfo(vol: number, id: number): Promise<NodeInfoDto | null> {
  return invoke("node_info", { vol, id });
}

/** Native shell context menu for a node, at the cursor (fire-and-forget -
 *  the menu is hosted on a dedicated OS thread). */
export function showContextMenu(vol: number, id: number): Promise<void> {
  return invoke("show_context_menu", { vol, id });
}
