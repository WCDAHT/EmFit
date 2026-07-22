//! Tauri shell entry point. See STANDARDS §3 for the shell↔core boundary,
//! command wiring, and the async/event pattern.
//!
//! This crate is deliberately thin: it owns window setup, plugin
//! registration, and the `#[tauri::command]` functions that bridge the
//! webview to `app-name-core`. All real logic lives in the core crate
//! (`cargo check -p app-name-core` must pass with this crate deleted).

mod commands;
mod dto;
mod error;

use std::sync::Mutex;

use app_name_core::service::{config::Config, logging};

pub use error::CommandError;

/// Build and run the application. Called by `main.rs` on desktop and by the
/// generated mobile entry point on iOS/Android.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // STANDARDS §4.2: tracing from day one, into a detailed plain-text log file
    // under appdata (see `app_name_core::service::logging`). Best-effort — falls
    // back to stdout-only if the log directory can't be created.
    let log_dir = logging::init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        ?log_dir,
        "app starting"
    );

    // Load persisted settings once at startup; commands serve and mutate this
    // in-memory copy and write changes through to disk (STANDARDS §3.3/§4.4).
    let config: Mutex<Config> = Mutex::new(Config::load());

    tauri::Builder::default()
        .manage(config)
        // Plugins expose capability-gated APIs to the webview. `opener` opens
        // URLs/paths in the OS default handler; `dialog` is the native
        // open/save file picker (replaces a hand-rolled chooser). Grant their
        // permissions in `capabilities/default.json`. See STANDARDS §3.6.
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        // Every app command the webview may call must be listed here. Unlike
        // plugin/core commands (gated by capabilities, §3.6), your own
        // commands need no permission entry — registering them is enough.
        .invoke_handler(tauri::generate_handler![
            commands::greet,
            commands::log_event,
            commands::get_config,
            commands::set_config
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
