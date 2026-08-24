//! Application logging (STANDARDS sec 4.2).
//!
//! `tracing` everywhere, funnelled into a detailed, plain-text log file under
//! the OS data directory. This module is copied verbatim into new apps - keep
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
//!   via [`UtcTimer`] - unambiguous across timezones.
//! - **Very detailed.** Level, target, and source file:line are all recorded,
//!   and our own crates log at `debug` by default.
//!
//! A second, human-friendly layer writes to stdout so `tauri dev` still shows
//! logs in the terminal. Both layers share one [`EnvFilter`]; `RUST_LOG`
//! overrides the default (STANDARDS sec 4.4: env vars for deployment knobs only).
//!
//! [`init`] also installs a **panic hook**, because without one a panic in a
//! release build leaves nothing at all to work from: the bundle is built
//! `panic = "abort"` and `windows_subsystem = "windows"`, so there is no
//! unwind, no stderr, and the process is simply gone. Worse, a panic on a path
//! that logs nothing - a row window, say - leaves a log whose last line is
//! from minutes earlier and looks perfectly healthy. The hook writes the
//! message, the source location, and a backtrace before the process dies.
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

/// Where a panic is recorded a second time, in case the subscriber is the
/// thing that is broken. Never rotated: it should stay empty.
const PANIC_FILE_NAME: &str = "panic.log";

/// Filename stem for the rolling log (the date and `.log` are appended).
/// Per-app knob: set to your app's short name.
const LOG_FILE_PREFIX: &str = "emfit";

/// How many daily log files to keep before the oldest is pruned.
const MAX_LOG_FILES: usize = 30;

/// Default verbosity when `RUST_LOG` is unset: `info` for the world, `debug`
/// for our own crates so the file stays detailed without the dependency
/// firehose. Per-app knob: list your crates here. The default names are
/// `emfit_core` (core), `emfit_lib` (the shell library), `emfit`
/// (the binary). Rename in lockstep when you rename the crates. `frontend` is
/// the fixed target for webview console lines forwarded through the console
/// bridge (`src-tauri/src/commands.rs::log_event`); it is not a crate name.
const DEFAULT_FILTER: &str = "info,emfit_core=debug,emfit_lib=debug,emfit=debug,frontend=debug";

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
/// error - the app keeps running, just without a log file.
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
            install_panic_hook(None);
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
            install_panic_hook(None);
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

    install_panic_hook(Some(dir.clone()));
    tracing::info!(log_dir = %dir.display(), "logging initialised");
    Ok(dir)
}

/// Record panics before the process goes, then let the default hook run.
///
/// `dir` is where a copy is appended, for the case where the tracing
/// subscriber itself is what failed. Called by [`init`]; exposed for a binary
/// that sets up its own logging.
pub fn install_panic_hook(dir: Option<PathBuf>) {
    let previous = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|at| format!("{}:{}:{}", at.file(), at.line(), at.column()))
            .unwrap_or_else(|| "unknown location".to_string());
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string();
        let message = panic_message(info);

        // Addresses only in a stripped release build, but the message and the
        // location above are what actually identify the bug, and those survive
        // stripping.
        let backtrace = std::backtrace::Backtrace::force_capture();

        tracing::error!(
            panic = %message,
            location = %location,
            thread = %thread,
            "PANIC - the process is about to abort"
        );
        tracing::error!("panic backtrace:\n{backtrace}");

        if let Some(dir) = &dir {
            let line = format!(
                "{} PANIC in thread `{thread}` at {location}: {message}\n{backtrace}\n\n",
                chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.3fZ"),
            );
            append(&dir.join(PANIC_FILE_NAME), &line);
        }

        previous(info);
    }));
}

/// The panic payload as text. `panic!` produces either a `&str` or a
/// `String`; anything else is a `panic_any` and only its type is knowable.
fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_string();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    "a non-string panic payload".to_string()
}

/// Append to a file, ignoring failure. Called from a panic hook, where there
/// is nothing useful to do about an error and raising one would replace the
/// original panic with a less interesting one.
fn append(path: &std::path::Path, text: &str) {
    use std::io::Write;

    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(text.as_bytes());
        let _ = file.flush();
    }
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
    #[test]
    fn the_panic_hook_writes_the_message_and_location() {
        // The crash this exists for wrote nothing anywhere: a release build
        // aborts without unwinding and has no stderr, and the panicking path
        // logged nothing of its own.
        let dir = std::env::temp_dir().join(format!("emfit-panic-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(super::PANIC_FILE_NAME);
        let _ = std::fs::remove_file(&path);

        super::install_panic_hook(Some(dir.clone()));
        // Indexed through a runtime value, or the compiler refuses to build
        // an index it can prove is out of bounds.
        let caught = std::panic::catch_unwind(|| {
            let empty: Vec<u32> = Vec::new();
            let at = std::hint::black_box(0usize);
            empty[at]
        });
        assert!(caught.is_err(), "the test panic must have happened");

        let written = std::fs::read_to_string(&path).expect("the hook wrote a panic file");
        assert!(
            written.contains("index out of bounds"),
            "the message has to survive: {written}"
        );
        assert!(
            written.contains("logging.rs"),
            "and so does where it happened: {written}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    #[test]
    fn log_dir_is_under_product_data_dir() {
        // The path should resolve and end with our log sub-directory. We don't
        // assert the full prefix (it's OS/user-specific) - just the tail.
        let dir = log_dir().expect("log dir should resolve in a test environment");
        assert!(dir.ends_with(LOG_DIR_NAME), "got {}", dir.display());
        assert!(
            dir.to_string_lossy().contains(app::PRODUCT),
            "log path should contain the product name, got {}",
            dir.display()
        );
    }
}
