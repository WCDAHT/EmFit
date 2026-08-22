//! `#[tauri::command]` functions: the webview's only entry into Rust.
//!
//! Per STANDARDS sec 3.4 these stay *thin*: parse/validate arguments, call into
//! `emfit-core`, map the result to a `Serialize` type or `CommandError`.
//! Long work (scanning, searching, sorting) runs on worker threads that hold
//! clones of the shared state; results come back to the webview as events:
//!
//! - `scan:progress` - [`ScanProgressDto`], throttled by the core sweep
//! - `scan:done`     - [`ScanDoneDto`], one per volume
//! - `view:updated`  - [`ViewUpdatedDto`], after every completed query/sort
//!
//! The row-window contract (features.md sec 9): the index never crosses IPC.
//! The webview calls [`get_rows`] for the window its viewport shows, and
//! nothing more.
//!
//! Every command added here must be listed in `tauri::generate_handler![...]`
//! in `lib.rs`. A typed wrapper for each lives in `frontend/src/lib/ipc.ts`.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use emfit_core::model::index::NodeId;
use emfit_core::model::volume::VolumeInfo;
use emfit_core::service::cache;
use emfit_core::service::config::Config;
use emfit_core::service::fold::CaseFold;
use emfit_core::service::query::Query;
use emfit_core::service::task::{CancellationToken, Progress};
use emfit_core::service::treemap::TreemapOptions;
use emfit_core::service::view::SortKey;
use emfit_core::service::{
    breakdown, elevation, presets, scan, search, tree, treemap, view, volume,
};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::dto::{
    CacheUsageDto, DrillDto, NodeInfoDto, PresetDto, RawQueryDto, RowWindowDto, ScanDoneDto,
    ScanProgressDto, ScanTargetDto, SelectionSummaryDto, SortDto, SyntaxSectionDto, TreeRowDto,
    TreemapRectDto, TypeRowDto, ViewUpdatedDto, VolumeDto,
};
use crate::error::{CommandError, CommandResult};
use crate::state::{AppState, Cacheable, ScannedVolume};

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

/// The one-click fix when it can't (features.md sec 1.1).
#[tauri::command]
pub fn relaunch_elevated() -> CommandResult<()> {
    elevation::relaunch_elevated()?;
    Ok(())
}

/// Scan the given targets - mounted volumes in parallel, then any disk
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
        // A scan run REPLACES the result set (user direction 2026-07-31,
        // WizTree semantics): what you see afterwards is exactly what you
        // just scanned. Multi-volume views come from selecting several
        // targets in one run, not from accumulating runs. The sort-rank
        // cache indexes into these volumes, so it dies with them.
        inner.volumes.clear();
        inner.sort_ranks.clear();
        inner.ranks_epoch += 1;
        let cancel = CancellationToken::new();
        inner.scanning = true;
        inner.scan_cancel = Some(cancel.clone());
        cancel
    };
    // The indexes those watchers map against are gone.
    crate::watch::stop_all(&state);

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

        // Fresh indexes -> fresh deletion watchers (roadmap M5).
        crate::watch::respawn_all(&app);

        // Write the cache first and warm afterwards: the file is what saves
        // the next scan twelve seconds, and it must not depend on the window
        // staying open for the ten seconds of warm-up.
        let epoch = app.state::<AppState>().inner.lock().unwrap().ranks_epoch;
        let saving = save_snapshots(&app, epoch);

        // Everything-style fast sort: precompute every column's ordering in
        // the background so header clicks are integer-rank sorts.
        warm_sort_ranks(&app, saving);
    });
    Ok(())
}

/// Build the per-column sort-rank tables on a background thread, one column
/// at a time, aborting the moment a new scan invalidates the volume set.
/// Order matters: the default sort first, then the columns whose uncached
/// sorts are the most expensive (the string-keyed ones).
///
/// A cached load arrives with those expensive columns already built, so this
/// installs them and warms only what is left - then writes the snapshot back,
/// orders included, so the next load skips the warm-up entirely.
fn warm_sort_ranks(app: &AppHandle, saving: Option<std::thread::JoinHandle<()>>) {
    let state = app.state::<AppState>();
    let (volumes, epoch) = {
        let inner = state.inner.lock().unwrap();
        let volumes: Vec<Arc<emfit_core::model::index::Index>> =
            inner.volumes.iter().map(|v| v.index.clone()).collect();
        (volumes, inner.ranks_epoch)
    };
    if volumes.is_empty() {
        return;
    }

    install_cached_orders(app, epoch);

    let app = app.clone();
    std::thread::spawn(move || {
        use emfit_core::service::view::SortKey as K;

        // Rayon's pool is process-global; building ranks on it starved
        // interactive sorts (a 20 ms rank sort stretched to ~1 s while the
        // Path table was building). A quarter of the cores in a private
        // pool keeps warm-up slightly slower and the UI unaffected.
        let threads = std::thread::available_parallelism()
            .map(|n| (n.get() / 4).max(1))
            .unwrap_or(1);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("rank-warm-{i}"))
            .build();

        for key in [
            K::Size,
            K::Name,
            K::Path,
            K::Extension,
            K::Kind,
            K::Allocated,
            K::Modified,
        ] {
            {
                let state = app.state::<AppState>();
                let inner = state.inner.lock().unwrap();
                if inner.ranks_epoch != epoch {
                    return; // a new scan owns the volumes now
                }
                if inner.sort_ranks.contains_key(&key) {
                    continue;
                }
            }
            let started = Instant::now();
            let refs: Vec<&emfit_core::model::index::Index> =
                volumes.iter().map(|v| v.as_ref()).collect();
            let ranks = Arc::new(match &pool {
                Ok(pool) => pool.install(|| view::build_ranks(&refs, key)),
                Err(_) => view::build_ranks(&refs, key),
            });

            let state = app.state::<AppState>();
            let mut inner = state.inner.lock().unwrap();
            if inner.ranks_epoch != epoch {
                return;
            }
            inner.sort_ranks.insert(key, ranks);
            tracing::info!(
                ?key,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "sort ranks warmed"
            );
        }

        // The snapshot has to exist before columns can be appended to it.
        if let Some(saving) = saving
            && saving.join().is_err()
        {
            tracing::warn!("cache: the snapshot writer panicked; not appending orders");
            return;
        }
        append_orders(&app, epoch);
    });
}

/// Turn the sort orders a cached load brought with it into rank tables.
///
/// Single-volume views only, which is what scanning one drive produces: a rank
/// is a position in one global ordering, and interleaving two volumes' orders
/// means comparing their keys - for `Path`, exactly the seven seconds that
/// caching them was meant to avoid (`caching.md` sec 7).
fn install_cached_orders(app: &AppHandle, epoch: u64) {
    let state = app.state::<AppState>();
    let (key, orders) = {
        let mut inner = state.inner.lock().unwrap();
        if inner.ranks_epoch != epoch {
            return;
        }
        let Some((key, orders)) = inner.cached_orders.take() else {
            return;
        };

        // A stored order lists node ids, and a replay that added or dropped
        // anything renumbers them - so an order from a snapshot that was
        // patched on the way in points at the wrong rows, and sorting by it
        // would index past the end of the index. Repairing one (binary-search
        // the new nodes in, splice the dropped ones out) is the eventual
        // answer; until then a changed volume warms from scratch.
        let usable = inner
            .volumes
            .iter()
            .find(|v| v.key == key)
            .and_then(|v| v.cacheable.as_ref())
            .is_some_and(|meta| meta.unchanged);
        if !usable {
            tracing::debug!(key = %key, "cache: the snapshot was patched; warming its orders");
            return;
        }
        (key, orders)
    };

    let started = Instant::now();
    let built: Vec<(SortKey, Arc<view::SortRanks>)> = orders
        .into_iter()
        .map(|(sort_key, order)| (sort_key, Arc::new(view::ranks_from_order(&order))))
        .collect();

    let mut inner = state.inner.lock().unwrap();
    if inner.ranks_epoch != epoch {
        return;
    }
    if inner.volumes.len() != 1 || inner.volumes[0].key != key {
        tracing::debug!(
            volumes = inner.volumes.len(),
            "cache: sort orders apply to a single-volume view only; warming instead"
        );
        return;
    }
    // Every rank column is indexed by node id on the sort path, so one that is
    // the wrong length is a panic waiting for a header click.
    let nodes = inner.volumes[0].index.len();
    if built.iter().any(|(_, ranks)| ranks[0].len() != nodes) {
        tracing::warn!(key = %key, "cache: sort orders do not match the index; warming instead");
        return;
    }
    let columns = built.len();
    for (sort_key, ranks) in built {
        inner.sort_ranks.insert(sort_key, ranks);
    }
    tracing::info!(
        columns,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "cache: sort orders installed without warming"
    );
}

/// One volume's snapshot, waiting to be written.
struct SnapshotJob {
    key: String,
    index: Arc<emfit_core::model::index::Index>,
    fold: CaseFold,
    meta: Cacheable,
}

/// Write the scan cache for every volume that wants one, on its own thread.
///
/// Started as soon as the scan finishes and **not** waiting for the sort-rank
/// warm-up: the truth costs half a second and the orders cost ten, so a file
/// that waited for both was a file that never got written when the window was
/// closed promptly. The orders are appended later by [`append_orders`].
///
/// Returns the thread's handle so the warm-up can join it before appending.
/// Failures are logged and dropped: a missing cache costs a sweep next time.
fn save_snapshots(app: &AppHandle, epoch: u64) -> Option<std::thread::JoinHandle<()>> {
    let state = app.state::<AppState>();
    let jobs: Vec<SnapshotJob> = {
        let inner = state.inner.lock().unwrap();
        if inner.ranks_epoch != epoch {
            return None;
        }
        inner
            .volumes
            .iter()
            .filter_map(|volume| {
                let meta = volume.cacheable.as_ref()?;
                if meta.unchanged {
                    // It came out of this very file with nothing replayed onto
                    // it. Rewriting 150 MB to change a timestamp helps nobody.
                    tracing::debug!(key = %volume.key, "cache: already current; not rewriting");
                    return None;
                }
                Some(SnapshotJob {
                    key: volume.key.clone(),
                    index: volume.index.clone(),
                    fold: volume.fold.clone(),
                    meta: meta.clone(),
                })
            })
            .collect()
    };
    if jobs.is_empty() {
        return None;
    }

    let app = app.clone();
    Some(std::thread::spawn(move || {
        for job in jobs {
            let manifest = cache::manifest_for(
                &job.meta.info,
                &job.index,
                job.meta.journal,
                job.meta.duration,
                &job.meta.access_mode,
            );
            // Truth only. The sort orders follow when they exist.
            match cache::save(&job.index, &job.fold, &manifest, &[]) {
                Ok(path) => {
                    tracing::info!(key = %job.key, path = %path.display(), "cache: saved")
                }
                Err(e) => tracing::warn!(key = %job.key, error = %e, "cache: save failed"),
            }
        }

        let budget_mb = app.state::<Mutex<Config>>().lock().unwrap().cache_budget_mb;
        match cache::evict(budget_mb.saturating_mul(1 << 20)) {
            Ok(0) => {}
            Ok(n) => {
                tracing::info!(
                    removed = n,
                    budget_mb,
                    "cache: evicted to stay within budget"
                )
            }
            Err(e) => tracing::warn!(error = %e, "cache: eviction failed"),
        }
    }))
}

/// One volume's warmed columns, ready to become sort orders on disk.
type OrderJob = (
    String,
    VolumeInfo,
    usize,
    Vec<(SortKey, Arc<view::SortRanks>)>,
);

/// Put the warmed sort columns into the snapshots that are missing them.
///
/// Recovering an order from a rank table is an integer sort, so this costs
/// nothing next to having built the ranks in the first place. Skips a column
/// the file already has, which is what makes it safe to run after every scan.
fn append_orders(app: &AppHandle, epoch: u64) {
    let state = app.state::<AppState>();
    let jobs: Vec<OrderJob> = {
        let inner = state.inner.lock().unwrap();
        if inner.ranks_epoch != epoch {
            return;
        }
        inner
            .volumes
            .iter()
            .enumerate()
            .filter_map(|(slot, volume)| {
                let meta = volume.cacheable.as_ref()?;
                let ranks: Vec<(SortKey, Arc<view::SortRanks>)> = cache::CACHED_ORDERS
                    .iter()
                    .filter_map(|&key| {
                        let ranks = inner.sort_ranks.get(&key)?;
                        // A rank table built over several volumes has a column
                        // per volume; this one's is what the snapshot wants.
                        ranks.get(slot)?;
                        Some((key, ranks.clone()))
                    })
                    .collect();
                (!ranks.is_empty()).then(|| {
                    (
                        volume.key.clone(),
                        meta.info.clone(),
                        volume.index.len(),
                        ranks,
                    )
                })
            })
            .collect()
    };

    for (key, info, nodes, ranks) in jobs {
        let slot = {
            let inner = state.inner.lock().unwrap();
            match inner.volumes.iter().position(|v| v.key == key) {
                Some(slot) => slot,
                None => continue,
            }
        };
        let orders: Vec<(SortKey, Vec<u32>)> = ranks
            .iter()
            .filter_map(|(sort_key, ranks)| {
                Some((*sort_key, view::order_from_ranks(ranks.get(slot)?)))
            })
            .collect();
        let stamp = cache::VolumeStamp::of(&info);
        match cache::append_orders(&stamp, nodes, &orders) {
            Ok(0) => {}
            Ok(n) => tracing::info!(key = %key, columns = n, "cache: sort orders added"),
            Err(e) => tracing::warn!(key = %key, error = %e, "cache: appending orders failed"),
        }
    }
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

    let options = scan::VolumeScanOptions {
        // The user asked for these volumes; answering from the snapshot and
        // replaying the journal onto it is the same answer, faster
        // (`caching.md` sec 6). Nothing is ever loaded unasked.
        cache: if cache_enabled(app) {
            scan::CachePolicy::Use
        } else {
            scan::CachePolicy::Ignore
        },
        ..scan::VolumeScanOptions::default()
    };

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
        for ((name, info), result) in names.iter().zip(targets.iter()).zip(results) {
            install_outcome(app, name, Some(info), result);
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
        install_outcome(app, &key, None, result);
    }
}

/// Whether the user has left the scan cache switched on.
// The `State` handle has to outlive the lock guard, so the binding is not the
// redundant one clippy takes it for.
#[allow(clippy::let_and_return, reason = "the State handle outlives the guard")]
fn cache_enabled(app: &AppHandle) -> bool {
    let config = app.state::<Mutex<Config>>();
    let enabled = config.lock().unwrap().cache_enabled;
    enabled
}

/// Store a finished index under `key` and announce it, or announce the error.
///
/// `info` is the volume it came from, absent for disk images. Without it there
/// is nothing to file a snapshot under, so images are never cached.
fn install_outcome(
    app: &AppHandle,
    key: &str,
    info: Option<&VolumeInfo>,
    result: emfit_core::error::Result<scan::VolumeScanOutcome>,
) {
    match result {
        Ok(outcome) => {
            let root = outcome.index.node(outcome.index.root());
            let (from_cache, note) = describe_source(&outcome);
            let done = ScanDoneDto {
                volume: key.to_string(),
                ok: true,
                error: None,
                files: outcome.stats.files,
                directories: outcome.stats.directories,
                total_size: root.total_size(),
                total_display: view::human_size(root.total_size()),
                elapsed_ms: outcome.stats.elapsed.as_millis() as u64,
                from_cache,
                source_note: note,
            };

            // A volume with no journal position can never be brought up to
            // date, so writing its snapshot would only be a way to show stale
            // data later.
            let cacheable = info.zip(outcome.journal).map(|(info, journal)| Cacheable {
                info: info.clone(),
                journal,
                duration: outcome.stats.elapsed,
                access_mode: outcome.mode.as_str().to_string(),
                unchanged: matches!(
                    &outcome.source,
                    scan::OutcomeSource::Cached { replay, .. } if replay.records == 0
                ),
            });
            let orders = match outcome.source {
                scan::OutcomeSource::Cached { orders, .. } => orders,
                scan::OutcomeSource::Scanned => Default::default(),
            };

            let state = app.state::<AppState>();
            let mut inner = state.inner.lock().unwrap();
            inner.volumes.retain(|v| v.key != key);
            inner.volumes.push(ScannedVolume {
                key: key.to_string(),
                index: Arc::new(outcome.index),
                fold: outcome.fold,
                cacheable,
            });
            if !orders.is_empty() {
                inner.cached_orders = Some((key.to_string(), orders));
            }
            drop(inner);

            let _ = app.emit("scan:done", done);
        }
        Err(e) => emit_scan_done(app, key, Some(&e.to_string())),
    }
}

/// A line for the status bar saying how this index came to be.
fn describe_source(outcome: &scan::VolumeScanOutcome) -> (bool, String) {
    match &outcome.source {
        scan::OutcomeSource::Scanned => (false, String::new()),
        scan::OutcomeSource::Cached { age, replay, .. } => {
            let ago = format_age(*age);
            let note = if replay.records == 0 {
                format!("loaded from a {ago} cache; nothing had changed")
            } else {
                format!(
                    "loaded from a {ago} cache; {} changed record(s) re-read",
                    replay.records
                )
            };
            (true, note)
        }
    }
}

/// `3 h`, `2 d`, `moments` - enough to judge a cached scan at a glance.
fn format_age(age: std::time::Duration) -> String {
    let secs = age.as_secs();
    match secs {
        0..=59 => "moments-old".to_string(),
        60..=3599 => format!("{}-minute-old", secs / 60),
        3600..=86_399 => format!("{}-hour-old", secs / 3600),
        _ => format!("{}-day-old", secs / 86_400),
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
            from_cache: false,
            source_note: String::new(),
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
/// (features.md sec 3: stable, cached sort).
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

    // Match-highlighting spans, computed only for this visible window with
    // the same folding/needle semantics the matcher used.
    let query = Query::parse(&inner.view.raw);
    let highlightable =
        !query.expr.is_all() || query.regex.is_some() || !query.extensions.is_empty();

    let count = count.min(MAX_WINDOW) as usize;
    let rows = view::build_rows(&indices, &inner.view.hits, offset as usize, count)
        .into_iter()
        .map(|row| {
            let mut dto: crate::dto::RowDto = row.into();
            if highlightable {
                let volume = &inner.volumes[dto.vol as usize];
                dto.match_ranges = search::highlight_ranges(
                    &dto.name,
                    &volume.fold,
                    volume.index.caps().case_sensitive,
                    &query,
                );
            }
            dto
        })
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

/// The query language, for the Search syntax dialog. Generated from the
/// parser's own tables, so it cannot list something the engine ignores.
#[tauri::command]
pub fn search_syntax() -> Vec<SyntaxSectionDto> {
    emfit_core::service::syntax::reference()
        .into_iter()
        .map(Into::into)
        .collect()
}

// ---------------------------------------------------------------------------
// space analysis (the Tree view tab - features.md sec 4)
// ---------------------------------------------------------------------------

/// The folder tree's top level: one row per scanned volume.
#[tauri::command]
pub fn tree_roots(state: State<'_, AppState>) -> Vec<TreeRowDto> {
    let inner = state.inner.lock().unwrap();
    let indices: Vec<&emfit_core::model::index::Index> =
        inner.volumes.iter().map(|v| v.index.as_ref()).collect();
    tree::roots(&indices).into_iter().map(Into::into).collect()
}

/// One directory's children, largest first - lazy materialization from the
/// CSR ranges, so expanding a 100k-child folder is one call.
#[tauri::command]
pub fn tree_children(state: State<'_, AppState>, vol: u16, id: u32) -> Vec<TreeRowDto> {
    let inner = state.inner.lock().unwrap();
    let indices: Vec<&emfit_core::model::index::Index> =
        inner.volumes.iter().map(|v| v.index.as_ref()).collect();
    tree::children(&indices, vol, id)
        .into_iter()
        .map(Into::into)
        .collect()
}

/// Root-to-node id chain, for "reveal in tree".
#[tauri::command]
pub fn node_lineage(state: State<'_, AppState>, vol: u16, id: u32) -> Vec<u32> {
    let inner = state.inner.lock().unwrap();
    let indices: Vec<&emfit_core::model::index::Index> =
        inner.volumes.iter().map(|v| v.index.as_ref()).collect();
    tree::lineage(&indices, vol, id)
}

/// The squarified treemap for the current canvas and drill level, computed
/// in Rust and returned as a flat rectangle list (features.md sec 9).
#[tauri::command]
pub fn treemap_layout(
    state: State<'_, AppState>,
    drill: Option<DrillDto>,
    width: f32,
    height: f32,
    depth: u8,
    show_free_space: bool,
) -> Vec<TreemapRectDto> {
    let inner = state.inner.lock().unwrap();
    let indices: Vec<&emfit_core::model::index::Index> =
        inner.volumes.iter().map(|v| v.index.as_ref()).collect();

    // Depth 0 means "max": recursion is bounded by pixel size either way,
    // so unlimited depth costs no more than the canvas can show.
    let options = TreemapOptions {
        width,
        height,
        max_depth: if depth == 0 {
            u8::MAX
        } else {
            depth.clamp(1, 64)
        },
        show_free_space,
        ..TreemapOptions::default()
    };
    treemap::layout(&indices, drill.map(|d| (d.vol, d.id)), &options)
        .into_iter()
        .map(Into::into)
        .collect()
}

/// Aggregate every file by extension (features.md sec 4.3).
#[tauri::command]
pub fn type_breakdown(state: State<'_, AppState>, limit: usize) -> Vec<TypeRowDto> {
    let inner = state.inner.lock().unwrap();
    let indices: Vec<&emfit_core::model::index::Index> =
        inner.volumes.iter().map(|v| v.index.as_ref()).collect();
    breakdown::by_extension(&indices, limit.clamp(10, 500))
        .into_iter()
        .map(Into::into)
        .collect()
}

/// Tooltip / breadcrumb details for one node, fetched on demand so the
/// treemap payload stays lean.
#[tauri::command]
pub fn node_info(state: State<'_, AppState>, vol: u16, id: u32) -> Option<NodeInfoDto> {
    let inner = state.inner.lock().unwrap();
    let index = inner.volumes.get(vol as usize)?.index.as_ref();
    if (id as usize) >= index.len() {
        return None;
    }
    let node_id = NodeId::new(id);
    let node = index.node(node_id);
    let (size, allocated) = if node.is_directory() {
        (node.total_size(), node.total_allocated())
    } else {
        (node.size(), node.allocated())
    };
    Some(NodeInfoDto {
        path: index.path(node_id),
        size_display: view::human_size(size),
        allocated_display: view::human_size(allocated),
        files: node.file_count(),
        dirs: node.dir_count(),
    })
}

/// Show the native shell context menu for a node, at the cursor (roadmap
/// M4). The menu is tracked on the main thread with the app window as its
/// owner - the window is foreground from the very right-click, which is
/// what keeps the menu open (see `shell_menu`). Verbs - including any the
/// user picks that mutate the filesystem - are the shell's business, and
/// the index does not react until a rescan (deletion marking arrives with
/// M5's journal watcher).
#[tauri::command]
pub fn show_context_menu(
    app: AppHandle,
    state: State<'_, AppState>,
    vol: u16,
    id: u32,
) -> CommandResult<()> {
    let (path, is_dir, zoom_id) = {
        let inner = state.inner.lock().unwrap();
        let Some(volume) = inner.volumes.get(vol as usize) else {
            return Ok(());
        };
        let index = volume.index.as_ref();
        if (id as usize) >= index.len() {
            return Ok(());
        }
        let node_id = NodeId::new(id);
        let node = index.node(node_id);
        if node.is_synthetic() {
            return Ok(()); // free space / placeholders: nothing on disk
        }
        // A folder zooms to itself; a file zooms to its parent directory
        // (and the menu item is labelled accordingly).
        let zoom_id = if node.is_directory() {
            id
        } else {
            node.parent().get()
        };
        (index.path(node_id), node.is_directory(), zoom_id)
    };
    // A bare drive ("C:") parses as drive-relative; the shell needs "C:\".
    let path = if path.ends_with(':') {
        format!("{path}\\")
    } else {
        path
    };

    let Some(window) = app.webview_windows().into_values().next() else {
        return Ok(());
    };
    #[cfg(windows)]
    {
        // Raw isize handle, so tauri's and our `windows` crate versions
        // never need to agree on an HWND type.
        let hwnd = window
            .hwnd()
            .map_err(|e| CommandError::Shell(format!("window handle: {e}")))?
            .0 as isize;
        let _ = window.run_on_main_thread(move || {
            let action = crate::shell_menu::show_at(hwnd, &path, is_dir);
            if action == crate::shell_menu::MenuAction::ZoomIn {
                let _ = app.emit("menu:zoom", crate::dto::MenuZoomDto { vol, id: zoom_id });
            }
        });
    }
    #[cfg(not(windows))]
    {
        let _ = (window, app, zoom_id);
        crate::shell_menu::show_at(0, &path, is_dir);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the search worker
// ---------------------------------------------------------------------------

/// Re-run the view pipeline on a worker thread: parse -> (search) -> sort ->
/// install -> `view:updated`.
///
/// `sort_only` skips the filter pass and re-sorts the current hits. A worker
/// only installs its result when the generation it started under is still
/// current - that is what makes the query interruptible: the next keystroke
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
    let cached_ranks = inner.sort_ranks.get(&sort.key).cloned();
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
        let search_ms = started.elapsed().as_millis() as u64;

        let sort_started = Instant::now();
        let indices: Vec<&emfit_core::model::index::Index> =
            volumes.iter().map(|(index, _)| index.as_ref()).collect();
        // Fast path: a warmed rank table turns the sort into integer-key
        // work; the comparison sort only runs before the warmer gets there.
        let ranked = match &cached_ranks {
            Some(ranks) => {
                view::sort_by_ranks(&mut hits, ranks, sort.ascending);
                true
            }
            None => {
                view::sort_hits(&indices, &mut hits, sort);
                false
            }
        };

        // Per-stage timing in the log: the first place to look when a query
        // feels slow (the UI's elapsed number is the whole pipeline).
        tracing::info!(
            hits = hits.len(),
            search_ms,
            sort_ms = sort_started.elapsed().as_millis() as u64,
            ranked,
            sort_only,
            "view pipeline timings"
        );

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
/// What the cache directory currently holds, for the settings dialog.
///
/// Cheap: only each snapshot's header is read, never its columns.
#[tauri::command]
pub fn cache_usage() -> CacheUsageDto {
    let entries = cache::list().unwrap_or_default();
    let bytes: u64 = entries.iter().map(|e| e.bytes).sum();
    CacheUsageDto {
        count: entries.len() as u64,
        bytes,
        display: view::human_size(bytes),
    }
}

/// Delete every cached scan. Returns what the directory holds afterwards.
///
/// Nothing but disk is lost - the next scan of a volume reads its table the
/// long way and writes a fresh snapshot.
#[tauri::command]
pub fn clear_cache() -> CommandResult<CacheUsageDto> {
    let removed = cache::clear()?;
    tracing::info!(removed, "cache: cleared from the settings dialog");
    Ok(cache_usage())
}

#[tauri::command]
pub fn set_config(new_config: Config, config: State<'_, Mutex<Config>>) -> CommandResult<()> {
    let mut guard = config.lock().unwrap();
    *guard = new_config;
    guard.save()?;
    Ok(())
}
