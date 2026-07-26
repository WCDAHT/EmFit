// Shared frontend types that mirror the shell's IPC shapes. Field names are
// snake_case because the same serde shapes cross IPC unrenamed — keep these
// in step with the Rust `Serialize`/`Deserialize` derives in
// src-tauri/src/dto.rs and core's `service::config`.

// Mirror of `emfit_core::service::config::Config`.
export interface AppConfig {
  schema_version: number;
  theme: "dark" | "light" | null;
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

/** A `Filters.csv` preset. */
export interface PresetDto {
  name: string;
  search: string;
}
