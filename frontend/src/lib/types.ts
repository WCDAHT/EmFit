// Shared frontend types that mirror the shell's IPC shapes. Field names are
// snake_case because the same serde shapes cross IPC unrenamed — keep these
// in step with the Rust `Serialize`/`Deserialize` derives in
// src-tauri/src/dto.rs and core's `service::config`.

// Mirror of `emfit_core::service::config::Config`.
export interface AppConfig {
  schema_version: number;
  theme: "dark" | "light" | null;
  /** Size-unit setting consumed by `lib/format.ts` ("dynamic" or a fixed unit). */
  size_unit: string;
  treemap: TreemapConfig;
}

/** Mirror of `TreemapConfig`: how the treemap colors rectangles. */
export interface TreemapConfig {
  color_mode: string; // "ranked" | "extension"
  /** Draw the synthetic free-space block (off by default). */
  show_free_space: boolean;
}

// Mirror of `ProgressDto` (a `#[serde(tag = "kind")]` enum).
export type ProgressDto =
  | { kind: "Started"; message: string; total: number | null }
  | { kind: "Tick"; done: number }
  | { kind: "Finished" }
  | { kind: "Failed"; message: string };

/** Payload of the `scan:progress` event (ProgressDto flattened beside `volume`). */
export type ScanProgressEvent = { volume: string } & ProgressDto;

/** Payload of the `scan:done` event, one per volume. */
export interface ScanDoneEvent {
  volume: string;
  ok: boolean;
  error: string | null;
  files: number;
  directories: number;
  total_size: number;
  total_display: string;
  elapsed_ms: number;
}

/** One volume for the drive-selection UI. */
export interface VolumeDto {
  name: string;
  filesystem: string;
  label: string | null;
  total_bytes: number;
  free_bytes: number;
  total_display: string;
  free_display: string;
  raw_scannable: boolean;
  scanned: boolean;
}

/** The raw query strings the search UI holds; parsed leniently in Rust. */
export interface RawQueryDto {
  text: string;
  regex: string;
  size: string;
  modified: string;
  extensions: string;
  preset: string;
  include_hidden: boolean;
  include_system: boolean;
  case_sensitive: boolean | null;
}

/** Sort keys, mirroring `SortDto::to_sort` in the shell. */
export type SortKey =
  | "name"
  | "path"
  | "size"
  | "allocated"
  | "modified"
  | "extension"
  | "kind";

/** One display row: pre-formatted strings plus ids (features.md §9). */
export interface RowDto {
  vol: number;
  id: number;
  name: string;
  dir_path: string;
  size: number;
  allocated: number;
  size_display: string;
  allocated_display: string;
  modified_display: string;
  extension: string;
  kind_label: string;
  category: number;
  is_directory: boolean;
  is_hidden: boolean;
  is_system: boolean;
  is_alias: boolean;
  is_synthetic: boolean;
  is_reparse: boolean;
}

/** One window of the current view. */
export interface RowWindowDto {
  generation: number;
  total: number;
  offset: number;
  rows: RowDto[];
}

/** Payload of the `view:updated` event. */
export interface ViewUpdatedEvent {
  generation: number;
  total: number;
  elapsed_ms: number;
  warnings: string[];
  volumes_total_bytes: number;
  volumes_total_display: string;
}

/** Selection totals for the status bar. */
export interface SelectionSummaryDto {
  count: number;
  bytes: number;
  allocated: number;
  bytes_display: string;
  allocated_display: string;
}

/** Payload of the `usn:deleted` event: nodes newly observed deleted (M5 —
 *  marks only; every view shows them flagged until the next rescan). */
export interface UsnDeletedEvent {
  vol: number;
  ids: number[];
}

/** Payload of the `usn:gap` event: the change journal lost history, so the
 *  deletion marks on this volume are incomplete until a rescan. */
export interface UsnGapEvent {
  volume: string;
}

/** Payload of the `menu:zoom` event: "Zoom in" was picked on a folder in
 *  the shell context menu — drill the treemap into it. */
export interface MenuZoomEvent {
  vol: number;
  id: number;
}

/** A `Filters.csv` preset. */
export interface PresetDto {
  name: string;
  search: string;
}

/** One entry in the scan list: a mounted volume or a disk image file.
 *  `kind`+`key` mirror the shell's `ScanTargetDto`; `label` is display-only. */
export interface ScanTarget {
  kind: "volume" | "image";
  key: string;
  label: string;
}

/** One folder-tree row (mirror of `TreeRowDto`). */
export interface TreeRowDto {
  vol: number;
  id: number;
  name: string;
  is_dir: boolean;
  synthetic: boolean;
  size: number;
  allocated: number;
  size_display: string;
  allocated_display: string;
  percent_of_parent: number;
  files: number;
  dirs: number;
  /** files + dirs for a directory; 0 for a file (renders blank). */
  items: number;
  /** Raw mtime, ns since the Unix epoch (0 = unknown) — the sort key. */
  modified: number;
  modified_display: string;
  /** Attribute letters: H hidden, S system, R reparse, C compressed,
   *  P sparse, L hard-link alias. */
  attributes: string;
  has_children: boolean;
}

/** One treemap rectangle (mirror of `TreemapRectDto`). */
export interface TreemapRectDto {
  vol: number;
  id: number;
  x: number;
  y: number;
  w: number;
  h: number;
  depth: number;
  is_dir: boolean;
  synthetic: boolean;
  category: number;
  branch: number;
  headed: boolean;
  expanded: boolean;
  aggregate: boolean;
  name: string;
  size: number;
  allocated: number;
  size_display: string;
  allocated_display: string;
}

/** One extension's share (mirror of `TypeRowDto`). */
export interface TypeRowDto {
  extension: string;
  kind_label: string;
  category: number;
  count: number;
  size: number;
  allocated: number;
  size_display: string;
  allocated_display: string;
  percent: number;
}

/** Tooltip / breadcrumb details (mirror of `NodeInfoDto`). */
export interface NodeInfoDto {
  path: string;
  size_display: string;
  allocated_display: string;
  files: number;
  dirs: number;
}

/** A node handle: which volume slot, which node id. */
export interface NodeRef {
  vol: number;
  id: number;
}
