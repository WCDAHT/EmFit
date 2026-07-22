//! Application logging (STANDARDS §4.2).
//!
//! `tracing` everywhere, funnelled into a detailed, plain-text log file under
//! the OS data directory. This module is copied verbatim into new apps — keep
//! it project-agnostic (identity comes from [`crate::app`], the only per-app
//! knobs are the two constants below).
//!
//! What the file layer guarantees:
//! - **On disk, in appdata.** `%LOCALAPPDATA%\<COMPANY>\<PRODUCT>\data\logs\`
//!   on Windows (resolved via the `directories` crate), one file per day,
//!   `<prefix>.YYYY-MM-DD.log`, the newest [`MAX_LOG_FILES`] kept (older pruned).
//! - **No colour symbols.** ANSI escapes are disabled on the file layer, so
//!   the log is greppable plain text, not terminal-control gibberish.
//! - **Timestamped.** Every line is prefixed with a millisecond UTC timestamp
//!   via [`UtcTimer`] — unambiguous across timezones.
//! - **Very detailed.** Level, target, and source file:line are all recorded,
//!   and our own crates log at `debug` by default.
//!
//! A second, human-friendly layer writes to stdout so `tauri dev` still shows
//! logs in the terminal. Both layers share one [`EnvFilter`]; `RUST_LOG`
//! overrides the default (STANDARDS §4.4: env vars for deployment knobs only).
//!
//! Call [`init`] once, as early as possible in the shell's `run()`. It is
//! best-effort: if the log directory can't be created it falls back to
//! stdout-only logging and the app still runs.

use std::fmt;
use std::path::PathBuf;

use directories::ProjectDirs;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::prelude::*;

use crate::app;
use crate::error::{Error, Result};

/// Sub-directory of the OS data dir that holds log files.
const LOG_DIR_NAME: &str = "logs";

/// Filename stem for the rolling log (the date and `.log` are appended).
/// Per-app knob: set to your app's short name.
const LOG_FILE_PREFIX: &str = "app-name";

/// How many daily log files to keep before the oldest is pruned.
const MAX_LOG_FILES: usize = 30;

/// Default verbosity when `RUST_LOG` is unset: `info` for the world, `debug`
/// for our own crates so the file stays detailed without the dependency
/// firehose. Per-app knob: list your crates here. The default names are
/// `app_name_core` (core), `app_name_lib` (the shell library), `app_name`
/// (the binary). Rename in lockstep when you rename the crates. `frontend` is
/// the fixed target for webview console lines forwarded through the console
/// bridge (`src-tauri/src/commands.rs::log_event`); it is not a crate name.
const DEFAULT_FILTER: &str =
    "info,app_name_core=debug,app_name_lib=debug,app_name=debug,frontend=debug";

/// The directory log files are written to, seeded by the [`crate::app`]
/// identity constants. On Windows: `%LOCALAPPDATA%\<COMPANY>\<PRODUCT>\data\logs\`.
pub fn log_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from(app::QUALIFIER, app::COMPANY, app::PRODUCT).ok_or_else(|| {
        Error::Config {
            message: "could not determine the OS data directory (no valid home directory)"
                .to_string(),
        }
    })?;
    Ok(dirs.data_local_dir().join(LOG_DIR_NAME))
}

/// Millisecond UTC timestamp, e.g. `2026-06-22 14:02:11.482Z`. Hand-rolled on
/// `chrono` (already a dependency) so we needn't enable tracing-subscriber's
/// `time`/`chrono` feature just for a timestamp format. UTC (not local) keeps
/// the log unambiguous; it also avoids chrono's `clock` feature, which the
/// workspace doesn't enable.
struct UtcTimer;

impl FormatTime for UtcTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> fmt::Result {
        write!(w, "{}", chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.3fZ"))
    }
}

/// Initialise global logging. Idempotent in practice (the underlying registry
/// can only be set once); call exactly once at startup.
///
/// Returns the resolved log directory on success so the caller can report it.
/// On failure to set up the file (e.g. unresolvable/uncreatable directory) it
/// logs a warning to stdout, installs a stdout-only subscriber, and returns the
/// error — the app keeps running, just without a log file.
pub fn init() -> Result<PathBuf> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    // Stdout layer: present in both the file-OK and file-failed paths so dev
    // runs always see logs.
    //
    // ANSI is disabled here too, not just on the file layer. A span's fields
    // are formatted once and cached in the span's extensions, then shared by
    // every fmt layer; if this layer rendered them with ANSI, those escape
    // codes would bleed into the (supposedly plain-text) file layer's span
    // prefixes. Keeping both layers ANSI-free guarantees a clean log file.
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_timer(UtcTimer)
        .with_target(true)
        .with_file(true)
        .with_line_number(true);

    let dir = match prepare_log_dir() {
        Ok(d) => d,
        Err(e) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stdout_layer)
                .init();
            tracing::warn!(error = %e, "logging: file logging unavailable; stdout only");
            return Err(e);
        }
    };

    // Daily rotation, retaining the most recent MAX_LOG_FILES days. The
    // appender is a `MakeWriter`, so the fmt layer writes through it directly
    // (no non-blocking worker / WorkerGuard lifetime to manage).
    let appender = match RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .filename_suffix("log")
        .max_log_files(MAX_LOG_FILES)
        .build(&dir)
    {
        Ok(a) => a,
        Err(e) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stdout_layer)
                .init();
            tracing::warn!(error = %e, dir = %dir.display(), "logging: could not open log file; stdout only");
            return Err(Error::Config {
                message: format!("could not open log file in {}: {e}", dir.display()),
            });
        }
    };

    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false) // plain text: no colour symbols in the file
        .with_timer(UtcTimer)
        .with_target(true)
        .with_level(true)
        .with_file(true)
        .with_line_number(true)
        .with_writer(appender);

    tracing_subscriber::registry()
        .with(filter)
        .with(stdout_layer)
        .with(file_layer)
        .init();

    tracing::info!(log_dir = %dir.display(), "logging initialised");
    Ok(dir)
}

/// Resolve and create the log directory.
fn prepare_log_dir() -> Result<PathBuf> {
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
        path: dir.clone(),
        source,
    })?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_dir_is_under_product_data_dir() {
        // The path should resolve and end with our log sub-directory. We don't
        // assert the full prefix (it's OS/user-specific) — just the tail.
        let dir = log_dir().expect("log dir should resolve in a test environment");
        assert!(dir.ends_with(LOG_DIR_NAME), "got {}", dir.display());
        assert!(
            dir.to_string_lossy().contains(app::PRODUCT),
            "log path should contain the product name, got {}",
            dir.display()
        );
    }
}
