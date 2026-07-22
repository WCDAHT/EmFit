//! Typed wrappers around Tauri's `invoke()`. See STANDARDS §3.4.
//!
//! The rest of the Svelte code calls these functions, never `invoke("...")`
//! with a raw string. This keeps command names in one place, gives every
//! call a return type the compiler checks, and makes the full surface the
//! frontend depends on greppable. Each function here pairs with a
//! `#[tauri::command]` in src-tauri/src/commands.rs.

import { invoke } from "@tauri-apps/api/core";
import type { AppConfig } from "./types";

/** Canonical example bridging to the core crate. Replace as the app grows. */
export function greet(name: string): Promise<string> {
  return invoke<string>("greet", { name });
}

/** Forward a webview log line into the Rust `tracing` pipeline (same log file +
 *  format as backend logs). Used by the console bridge; fire-and-forget. */
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
