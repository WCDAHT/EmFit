//! `#[tauri::command]` functions: the webview's only entry into Rust.
//!
//! These are the Tauri analogue of the old Slint callback wiring. Per
//! STANDARDS §3.4 they stay *thin*: parse/validate arguments, call into
//! `app-name-core`, map the result to a `Serialize` type or `CommandError`.
//! No business logic lives here — if a command grows past a few lines of
//! glue, the logic belongs in a core `service`.
//!
//! Every command added here must be listed in `tauri::generate_handler![...]`
//! in `lib.rs` — that alone makes it invokable. App commands need NO capability
//! entry; `capabilities/default.json` (§3.6) gates only plugin/core commands.
//!
//! A typed wrapper for each command lives on the frontend in
//! `src/lib/ipc.ts`, so the rest of the Svelte code never touches the raw
//! `invoke()` string name.

use std::sync::Mutex;

use app_name_core::service::config::Config;
use tauri::State;

use crate::error::CommandResult;

/// Canonical "hello world" command demonstrating the shell↔core bridge.
///
/// Replace with real commands as the app grows. The shape to copy:
/// take serde-deserializable args, return a `CommandResult<T>` where `T:
/// Serialize`, and delegate the actual work to a core service.
#[tauri::command]
pub fn greet(name: &str) -> CommandResult<String> {
    tracing::info!(name, "greet called");
    Ok(format!("Hello, {name}! The shell↔core bridge works."))
}

/// Emit a log line that originated in the webview into the Rust `tracing`
/// pipeline, so frontend and backend logs share one file and one format (the
/// webview has no log file of its own). The frontend console bridge
/// (`src/lib/log.ts`) calls this for every `console.*`, giving a single
/// traceable chain of actions.
///
/// `level` is the JS console level (`error`/`warn`/`info`/`debug`/`trace`);
/// anything else maps to `info`. The `frontend` target keeps these lines easy to
/// grep and is filtered at `debug` by default (see `logging::DEFAULT_FILTER`).
#[tauri::command]
pub fn log_event(level: String, message: String) {
    match level.as_str() {
        "error" => tracing::error!(target: "frontend", "{message}"),
        "warn" => tracing::warn!(target: "frontend", "{message}"),
        "debug" => tracing::debug!(target: "frontend", "{message}"),
        "trace" => tracing::trace!(target: "frontend", "{message}"),
        _ => tracing::info!(target: "frontend", "{message}"),
    }
}

/// Read the current application config. The shell holds the loaded `Config` in
/// managed state (the Rust side owns truth, STANDARDS §3.3); this hands the
/// webview a snapshot to project into the UI.
#[tauri::command]
pub fn get_config(config: State<'_, Mutex<Config>>) -> Config {
    let cfg = config.lock().unwrap().clone();
    tracing::debug!(theme = ?cfg.theme, "config requested by webview");
    cfg
}

/// Replace the application config and persist it to disk. The webview loads the
/// config, mutates a setting, and sends the whole value back — so adding a new
/// setting needs no new command, just a new field on `Config`. The full value
/// round-trips (including `schema_version`) so the frontend never has to
/// understand fields it doesn't use.
#[tauri::command]
pub fn set_config(new_config: Config, config: State<'_, Mutex<Config>>) -> CommandResult<()> {
    let mut guard = config.lock().unwrap();
    tracing::info!(theme = ?new_config.theme, "updating application config");
    *guard = new_config;
    guard.save()?;
    tracing::debug!("config persisted to disk");
    Ok(())
}
