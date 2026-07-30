//! Typed wrappers around Tauri's `invoke()`. See STANDARDS §3.4.
//!
//! The rest of the Svelte code calls these functions, never `invoke("...")`
//! with a raw string. Each function pairs with a `#[tauri::command]` in
//! src-tauri/src/commands.rs.

import { invoke } from "@tauri-apps/api/core";
import type {
  AppConfig,
  NodeInfoDto,
  NodeRef,
  PresetDto,
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

/** One window of the current view — the only way rows reach the webview
 *  (features.md §9: the index never crosses IPC). */
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

/** The `Filters.csv` presets (built-in, or the user's own file). */
export function listPresets(): Promise<PresetDto[]> {
  return invoke("list_presets");
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
