//! Shell-owned application state: scanned volumes and the current view.
//!
//! The Rust side owns truth (STANDARDS sec 3.3). The webview holds no second
//! copy of anything here - it asks for row windows against the current
//! query/sort and renders what it gets. The index itself never crosses IPC
//! (features.md sec 9); it lives in this struct behind an `Arc`, shared with the
//! search worker threads.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use emfit_core::model::index::Index;
use emfit_core::service::fold::CaseFold;
use emfit_core::service::query::RawQuery;
use emfit_core::service::search::Hit;
use emfit_core::service::task::CancellationToken;
use emfit_core::service::view::{Sort, SortKey, SortRanks};

/// One volume the app has an index for.
pub struct ScannedVolume {
    /// Display key: `C:`, or an image file name.
    pub key: String,
    pub index: Arc<Index>,
    pub fold: CaseFold,
}

/// The current result view: the hits the last completed query produced, in
/// their current sort order. Row windows slice this.
pub struct ViewState {
    pub raw: RawQuery,
    pub sort: Sort,
    /// `Arc` so `get_rows` can slice while a new search builds its own list.
    pub hits: Arc<Vec<Hit>>,
    /// Bumped on every query/sort/volume change; a worker only installs its
    /// result if the generation still matches (stale results are dropped -
    /// the "interruptible query" of features.md sec 2).
    pub generation: u64,
    pub search_cancel: Option<CancellationToken>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            raw: RawQuery {
                // A space analyzer that hides hidden/system files under-reports
                // the disk; default to showing everything.
                include_hidden: true,
                include_system: true,
                ..RawQuery::default()
            },
            sort: Sort::default(),
            hits: Arc::new(Vec::new()),
            generation: 0,
            search_cancel: None,
        }
    }
}

/// Everything behind one mutex: state changes are quick pointer swaps, and
/// the slow work (scan, search, sort) happens on workers holding clones.
#[derive(Default)]
pub struct Inner {
    pub volumes: Vec<ScannedVolume>,
    pub scanning: bool,
    pub scan_cancel: Option<CancellationToken>,
    /// Cancels the whole USN deletion-watcher fleet (`watch.rs`). Taken and
    /// cancelled when a scan starts; a fresh fleet spawns when it finishes.
    pub watch_cancel: Option<CancellationToken>,
    pub view: ViewState,
    /// Precomputed per-column orderings (Everything's "fast sort"): sorting
    /// with a cached entry is integer-key work instead of a comparison
    /// sort. Filled by the post-scan warmer; cleared when a scan replaces
    /// the volume set. ~13MB per column per 3M nodes.
    pub sort_ranks: HashMap<SortKey, Arc<SortRanks>>,
    /// Bumped whenever `volumes`/`sort_ranks` are invalidated, so warmers
    /// that started against an older volume set never install stale ranks.
    pub ranks_epoch: u64,
}

pub struct AppState {
    pub inner: Mutex<Inner>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }
}
