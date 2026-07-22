// Shared frontend types that mirror the shell's IPC shapes. Add request /
// outcome interfaces here as commands grow; keep field names in step with the
// Rust `Serialize`/`Deserialize` derives in src-tauri/src/dto.rs and core.

// Mirror of `app_name_core::service::config::Config`. Field names are
// snake_case because the core struct serialises straight to TOML (no camelCase
// rename) and the same serde shape crosses IPC. The frontend round-trips fields
// it doesn't use (e.g. `schema_version`) untouched, so the backend stays the
// source of truth.
export interface AppConfig {
  schema_version: number;
  theme: "dark" | "light" | null;
}

// Mirror of `ProgressDto` in src-tauri/src/dto.rs (a `#[serde(tag = "kind")]`
// enum). Stream it over a Channel<Progress> per STANDARDS §3.5.
export type Progress =
  | { kind: "Started"; message: string; total: number | null }
  | { kind: "Tick"; done: number }
  | { kind: "Finished" }
  | { kind: "Failed"; message: string };
