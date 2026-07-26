//! `#[tauri::command]` functions: the webview's only entry into Rust.
//!
//! Per STANDARDS §3.4 these stay *thin*: parse/validate arguments, call into
//! `emfit-core`, map the result to a `Serialize` type or `CommandError`.
//! Long work (scanning, searching, sorting) runs on worker threads that hold
//! clones of the shared state; results come back to the webview as events:
//!
//! - `scan:progress` — [`ScanProgressDto`], throttled by the core sweep
//! - `scan:done`     — [`ScanDoneDto`], one per volume
//! - `view:updated`  — [`ViewUpdatedDto`], after every completed query/sort
//!
//! The row-window contract (features.md §9): the index never crosses IPC.
//! The webview calls [`get_rows`] for the window its viewport shows, and
//! nothing more.
//!
//! Every command added here must be listed in `tauri::generate_handler![...]`
//! in `lib.rs`. A typed wrapper for each lives in `frontend/src/lib/ipc.ts`.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use emfit_core::service::config::Config;
use emfit_core::service::query::Query;
use emfit_core::service::task::{CancellationToken, Progress};
use emfit_core::service::{elevation, presets, scan, search, view, volume};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::dto::{
    PresetDto, RawQueryDto, RowWindowDto, ScanDoneDto, ScanProgressDto, ScanTargetDto,
    SelectionSummaryDto, SortDto, ViewUpdatedDto, VolumeDto,
};
use crate::error::{CommandError, CommandResult};
use crate::state::{AppState, ScannedVolume};

/// The most rows one window may request. The viewport shows a few dozen;
/// anything larger is a bug or an attempt to ship the index across IPC.
const MAX_WINDOW: u64 = 512;

// ---------------------------------------------------------------------------
// volumes & scanning
// ---------------------------------------------------------------------------

/// Every volume on the machine, flagged with whether it is scannable and
/// whether an index is currently loaded.
#[tauri::command]
pub fn list_volumes(state: State<'_, AppState>) -> CommandResult<Vec<VolumeDto>> {
    let volumes = volume::enumerate()?;
    let inner = state.inner.lock().unwrap();
    Ok(volumes
        .iter()
        .map(|info| {
            let scanned = inner
                .volumes
                .iter()
                .any(|scanned| scanned.key == info.display_name());
            VolumeDto::from_info(info, scanned)
        })
        .collect())
}

/// Whether the process can open raw volumes (Administrator).
#[tauri::command]
pub fn elevation_status() -> bool {
    elevation::is_elevated()
}

/// The one-click fix when it can't (features.md §1.1).
#[tauri::command]
pub fn relaunch_elevated() -> CommandResult<()> {
    elevation::relaunch_elevated()?;
    Ok(())
}

/// Scan the given targets — mounted volumes in parallel, then any disk
/// images. Returns immediately; progress and completion arrive as events
/// keyed by the target's `key`.
#[tauri::command]
pub fn start_scan(
    app: AppHandle,
    state: State<'_, AppState>,
    targets: Vec<ScanTargetDto>,
) -> CommandResult<()> {
    let cancel = {
        let mut inner = state.inner.lock().unwrap();
        if inner.scanning {
            return Err(CommandError::Shell("a scan is already running".into()));
        }
        let cancel = CancellationToken::new();
        inner.scanning = true;
        inner.scan_cancel = Some(cancel.clone());
        cancel
    };

    std::thread::spawn(move || {
        run_scan_job(&app, &targets, &cancel);

        let state = app.state::<AppState>();
        let mut inner = state.inner.lock().unwrap();
        inner.scanning = false;
        inner.scan_cancel = None;
        drop(inner);

        // The result set changed; whatever query is active runs against the
        // new indexes.
        requery(&app, false);
    });
    Ok(())
}

/// The blocking part of a scan, on its worker thread.
fn run_scan_job(app: &AppHandle, requested: &[ScanTargetDto], cancel: &CancellationToken) {
    let (volume_keys, image_keys): (Vec<&ScanTargetDto>, Vec<&ScanTargetDto>) =
        requested.iter().partition(|t| t.kind != "image");

    // --- mounted volumes, in parallel ---
    let mut targets = Vec::new();
    if !volume_keys.is_empty() {
        match volume::enumerate() {
            Ok(all) => {
                for target in &volume_keys {
                    match all.iter().find(|v| v.display_name() == target.key) {
                        Some(info) => targets.push(info.clone()),
                        None => emit_scan_done(app, &target.key, Some("volume not found")),
                    }
                }
            }
            Err(e) => {
                for target in &volume_keys {
                    emit_scan_done(app, &target.key, Some(&e.to_string()));
                }
            }
        }
    }

    let options = scan::VolumeScanOptions::default();

    if !targets.is_empty() {
        let names: Vec<String> = targets.iter().map(|v| v.display_name()).collect();
        let progress_app = app.clone();
        let progress_names = names.clone();
        let on_progress = move |slot: usize, progress: Progress| {
            let _ = progress_app.emit(
                "scan:progress",
                ScanProgressDto {
                    volume: progress_names[slot].clone(),
                    progress: progress.into(),
                },
            );
        };

        let results = scan::scan_volumes(&targets, &options, &on_progress, cancel);
        for (name, result) in names.iter().zip(results) {
            install_outcome(app, name, result);
        }
    }

    // --- disk images, one after another (they share no disk with anything) ---
    for target in image_keys {
        if cancel.is_cancelled() {
            emit_scan_done(app, &target.key, Some("operation cancelled"));
            continue;
        }
        let key = target.key.clone();
        let progress_app = app.clone();
        let progress_key = key.clone();
        let mut on_progress = move |progress: Progress| {
            let _ = progress_app.emit(
                "scan:progress",
                ScanProgressDto {
                    volume: progress_key.clone(),
                    progress: progress.into(),
                },
            );
        };
        let result = scan::scan_image(
            std::path::Path::new(&key),
            &options,
            &mut on_progress,
            cancel,
        );
        install_outcome(app, &key, result);
    }
}

/// Store a finished index under `key` and announce it, or announce the error.
fn install_outcome(
    app: &AppHandle,
    key: &str,
    result: emfit_core::error::Result<scan::VolumeScanOutcome>,
) {
    match result {
        Ok(outcome) => {
            let root = outcome.index.node(outcome.index.root());
            let done = ScanDoneDto {
                volume: key.to_string(),
                ok: true,
                error: None,
                files: outcome.stats.files,
                directories: outcome.stats.directories,
                total_size: root.total_size(),
                total_display: view::human_size(root.total_size()),
                elapsed_ms: outcome.stats.elapsed.as_millis() as u64,
            };

            let state = app.state::<AppState>();
            let mut inner = state.inner.lock().unwrap();
            inner.volumes.retain(|v| v.key != key);
            inner.volumes.push(ScannedVolume {
                key: key.to_string(),
                index: Arc::new(outcome.index),
                fold: outcome.fold,
            });
            drop(inner);

            let _ = app.emit("scan:done", done);
        }
        Err(e) => emit_scan_done(app, key, Some(&e.to_string())),
    }
}

fn emit_scan_done(app: &AppHandle, volume: &str, error: Option<&str>) {
    let _ = app.emit(
        "scan:done",
        ScanDoneDto {
            volume: volume.to_string(),
            ok: error.is_none(),
            error: error.map(str::to_string),
            files: 0,
            directories: 0,
            total_size: 0,
            total_display: String::new(),
            elapsed_ms: 0,
        },
    );
}

/// Stop the running scan. The core checks the token every chunk, so this
/// lands within a fraction of a second.
#[tauri::command]
pub fn cancel_scan(state: State<'_, AppState>) {
    let inner = state.inner.lock().unwrap();
    if let Some(cancel) = &inner.scan_cancel {
        cancel.cancel();
    }
}

// ---------------------------------------------------------------------------
// query, sort, rows
// ---------------------------------------------------------------------------

/// Replace the active query. Returns immediately; a `view:updated` event
/// follows when the (interruptible) search completes.
#[tauri::command]
pub fn set_query(app: AppHandle, state: State<'_, AppState>, raw: RawQueryDto) {
    {
        let mut inner = state.inner.lock().unwrap();
        inner.view.raw = raw.into();
    }
    requery(&app, false);
}

/// Change the sort order. Re-sorts the current hits without re-filtering
/// (features.md §3: stable, cached sort).
#[tauri::command]
pub fn set_sort(app: AppHandle, state: State<'_, AppState>, sort: SortDto) {
    {
        let mut inner = state.inner.lock().unwrap();
        inner.view.sort = sort.to_sort();
    }
    requery(&app, true);
}

/// One window of the current view: pre-formatted rows for the viewport.
#[tauri::command]
pub fn get_rows(state: State<'_, AppState>, offset: u64, count: u64) -> RowWindowDto {
    let inner = state.inner.lock().unwrap();
    let indices: Vec<&emfit_core::model::index::Index> =
        inner.volumes.iter().map(|v| v.index.as_ref()).collect();

    let count = count.min(MAX_WINDOW) as usize;
    let rows = view::build_rows(&indices, &inner.view.hits, offset as usize, count)
        .into_iter()
        .map(Into::into)
        .collect();

    RowWindowDto {
        generation: inner.view.generation,
        total: inner.view.hits.len() as u64,
        offset,
        rows,
    }
}

/// Selection totals for the status bar. `picks` are positions in the current
/// view; `all` summarizes the whole result set (select-all without shipping
/// millions of indexes across IPC).
#[tauri::command]
pub fn selection_summary(
    state: State<'_, AppState>,
    picks: Vec<u32>,
    all: bool,
) -> SelectionSummaryDto {
    let inner = state.inner.lock().unwrap();
    let indices: Vec<&emfit_core::model::index::Index> =
        inner.volumes.iter().map(|v| v.index.as_ref()).collect();

    let summary = if all {
        view::summarize_all(&indices, &inner.view.hits)
    } else {
        view::summarize(&indices, &inner.view.hits, &picks)
    };
    summary.into()
}

/// The preset filters (`Filters.csv`): built-ins, or the user's own file
/// beside the app config when present.
#[tauri::command]
pub fn list_presets() -> Vec<PresetDto> {
    presets::load().into_iter().map(Into::into).collect()
}

// ---------------------------------------------------------------------------
// the search worker
// ---------------------------------------------------------------------------

/// Re-run the view pipeline on a worker thread: parse → (search) → sort →
/// install → `view:updated`.
///
/// `sort_only` skips the filter pass and re-sorts the current hits. A worker
/// only installs its result when the generation it started under is still
/// current — that is what makes the query interruptible: the next keystroke
/// bumps the generation and cancels the token, and the stale worker's work
/// is discarded.
fn requery(app: &AppHandle, sort_only: bool) {
    let state = app.state::<AppState>();
    let mut inner = state.inner.lock().unwrap();

    inner.view.generation += 1;
    let generation = inner.view.generation;
    if let Some(cancel) = inner.view.search_cancel.take() {
        cancel.cancel();
    }
    let cancel = CancellationToken::new();
    inner.view.search_cancel = Some(cancel.clone());

    let raw = inner.view.raw.clone();
    let sort = inner.view.sort;
    let volumes: Vec<(
        Arc<emfit_core::model::index::Index>,
        emfit_core::service::fold::CaseFold,
    )> = inner
        .volumes
        .iter()
        .map(|v| (v.index.clone(), v.fold.clone()))
        .collect();
    let previous_hits = sort_only.then(|| inner.view.hits.clone());
    drop(inner);

    let app = app.clone();
    std::thread::spawn(move || {
        let started = Instant::now();
        let query = Query::parse(&raw);
        let warnings = query.warnings.clone();

        let mut hits = match previous_hits {
            Some(hits) => (*hits).clone(),
            None => {
                let refs: Vec<(&emfit_core::model::index::Index, _)> = volumes
                    .iter()
                    .map(|(index, fold)| (index.as_ref(), fold))
                    .collect();
                match search::search_all(&refs, &query, &cancel) {
                    Some(hits) => hits,
                    // Interrupted: a newer query owns the view now.
                    None => return,
                }
            }
        };

        let indices: Vec<&emfit_core::model::index::Index> =
            volumes.iter().map(|(index, _)| index.as_ref()).collect();
        view::sort_hits(&indices, &mut hits, sort);

        let volumes_total: u64 = indices
            .iter()
            .map(|index| index.node(index.root()).total_size())
            .sum();

        let update = ViewUpdatedDto {
            generation,
            total: hits.len() as u64,
            elapsed_ms: started.elapsed().as_millis() as u64,
            warnings,
            volumes_total_bytes: volumes_total,
            volumes_total_display: view::human_size(volumes_total),
        };

        let state = app.state::<AppState>();
        let mut inner = state.inner.lock().unwrap();
        if inner.view.generation != generation {
            return; // superseded while we worked
        }
        inner.view.hits = Arc::new(hits);
        drop(inner);

        let _ = app.emit("view:updated", update);
    });
}

// ---------------------------------------------------------------------------
// template plumbing (config, logging)
// ---------------------------------------------------------------------------

/// Emit a log line that originated in the webview into the Rust `tracing`
/// pipeline, so frontend and backend logs share one file and one format.
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

/// Read the current application config.
#[tauri::command]
pub fn get_config(config: State<'_, Mutex<Config>>) -> Config {
    config.lock().unwrap().clone()
}

/// Replace the application config and persist it to disk.
#[tauri::command]
pub fn set_config(new_config: Config, config: State<'_, Mutex<Config>>) -> CommandResult<()> {
    let mut guard = config.lock().unwrap();
    *guard = new_config;
    guard.save()?;
    Ok(())
}
