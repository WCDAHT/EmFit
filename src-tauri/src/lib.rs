//! Tauri shell entry point. See STANDARDS sec 3 for the shell<->core boundary,
//! command wiring, and the async/event pattern.
//!
//! This crate is deliberately thin: it owns window setup, plugin
//! registration, and the `#[tauri::command]` functions that bridge the
//! webview to `emfit-core`. All real logic lives in the core crate
//! (`cargo check -p emfit-core` must pass with this crate deleted).

mod commands;
mod dto;
mod error;
mod shell_menu;
mod state;
mod watch;

use std::sync::Mutex;

use emfit_core::service::{config::Config, logging};
use tauri::Emitter;
use tauri::menu::{MenuBuilder, SubmenuBuilder};

pub use error::CommandError;

/// Build and run the application. Called by `main.rs` on desktop and by the
/// generated mobile entry point on iOS/Android.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // STANDARDS sec 4.2: tracing from day one, into a detailed plain-text log file
    // under appdata (see `emfit_core::service::logging`). Best-effort - falls
    // back to stdout-only if the log directory can't be created.
    let log_dir = logging::init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        ?log_dir,
        "app starting"
    );

    // Load persisted settings once at startup; commands serve and mutate this
    // in-memory copy and write changes through to disk (STANDARDS sec 3.3/sec 4.4).
    let config: Mutex<Config> = Mutex::new(Config::load());

    tauri::Builder::default()
        .manage(config)
        // Scanned indexes and the current search view (state::AppState). The
        // Rust side owns truth; the webview projects it (STANDARDS sec 3.3).
        .manage(state::AppState::new())
        // Plugins expose capability-gated APIs to the webview. `opener` opens
        // URLs/paths in the OS default handler; `dialog` is the native
        // open/save file picker (replaces a hand-rolled chooser). Grant their
        // permissions in `capabilities/default.json`. See STANDARDS sec 3.6.
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        // Native window menu instead of an in-app header - every pixel of the
        // window belongs to the data. Items the webview must react to are
        // forwarded as a `menu` event; the frontend switches on the id.
        .setup(|app| {
            let file = SubmenuBuilder::new(app, "File")
                .text("sources", "Scan sources...")
                .text("rescan", "Rescan\tF5")
                .text("cancel_scan", "Cancel scan\tEsc")
                .separator()
                .text("exit", "Exit")
                .build()?;
            let view = SubmenuBuilder::new(app, "View")
                .text("toggle_theme", "Toggle light/dark")
                .text("settings", "Settings...")
                .build()?;
            let help = SubmenuBuilder::new(app, "Help")
                .text("shortcuts", "Keyboard shortcuts\tF1")
                .text("about", "About EmFit")
                .build()?;
            let menu = MenuBuilder::new(app)
                .items(&[&file, &view, &help])
                .build()?;
            app.set_menu(menu)?;
            Ok(())
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "exit" => app.exit(0),
            id => {
                let _ = app.emit("menu", id.to_string());
            }
        })
        // Every app command the webview may call must be listed here. Unlike
        // plugin/core commands (gated by capabilities, sec 3.6), your own
        // commands need no permission entry - registering them is enough.
        .invoke_handler(tauri::generate_handler![
            commands::log_event,
            commands::get_config,
            commands::set_config,
            commands::cache_usage,
            commands::clear_cache,
            commands::list_volumes,
            commands::elevation_status,
            commands::relaunch_elevated,
            commands::start_scan,
            commands::cancel_scan,
            commands::set_query,
            commands::set_sort,
            commands::get_rows,
            commands::selection_summary,
            commands::list_presets,
            commands::tree_roots,
            commands::tree_children,
            commands::node_lineage,
            commands::treemap_layout,
            commands::type_breakdown,
            commands::node_info,
            commands::show_context_menu
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
