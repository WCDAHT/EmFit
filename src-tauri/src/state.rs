//! Shell-owned application state: scanned volumes and the current view.
//!
//! The Rust side owns truth (STANDARDS sec 3.3). The webview holds no second
//! copy of anything here - it asks for row windows against the current
//! query/sort and renders what it gets. The index itself never crosses IPC
//! (features.md sec 9); it lives in this struct behind an `Arc`, shared with the
//! search worker threads.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use std::time::Duration;

use emfit_core::model::index::Index;
use emfit_core::model::volume::VolumeInfo;
use emfit_core::service::cache::JournalStamp;
use emfit_core::service::fold::CaseFold;
use emfit_core::service::query::RawQuery;
use emfit_core::service::search::Hit;
use emfit_core::service::task::CancellationToken;
use emfit_core::service::update::UpdateStatus;
use emfit_core::service::view::{Sort, SortKey, SortRanks};

/// One volume the app has an index for.
pub struct ScannedVolume {
    /// Display key: `C:`, or an image file name.
    pub key: String,
    pub index: Arc<Index>,
    pub fold: CaseFold,
    /// What it would take to write this volume's scan cache. Absent for disk
    /// images and for volumes with no change journal - a snapshot that could
    /// never be brought up to date is not worth the disk.
    pub cacheable: Option<Cacheable>,
}

/// Everything `cache::save` needs that the index itself does not carry.
#[derive(Clone)]
pub struct Cacheable {
    pub info: VolumeInfo,
    pub journal: JournalStamp,
    pub duration: Duration,
    /// `physical`, `volume`, or `image`.
    pub access_mode: String,
    /// True when this index came out of the snapshot with nothing replayed,
    /// so rewriting it would only churn a few hundred megabytes of disk.
    pub unchanged: bool,
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
    /// Sort orders a cached load brought with it, waiting to be turned into
    /// ranks. Only usable for a single-volume view: ranks are positions in one
    /// ordering, and merging two volumes' orders costs what building them did
    /// (`caching.md` sec 7).
    pub cached_orders: Option<(String, HashMap<SortKey, Vec<u32>>)>,
}

/// The update check's own small piece of state, managed separately from the
/// index (`commands::check_for_update`).
///
/// The full [`UpdateStatus`] is kept here rather than handed to the webview,
/// so the download command reads the release URL from what the *manifest*
/// said, not from what the page asks for.
#[derive(Default)]
pub struct UpdateSlot {
    /// The last completed check.
    pub status: Option<UpdateStatus>,
    /// Where the last successful download landed, for "show" and "run".
    pub downloaded: Option<PathBuf>,
    /// The executable an applied update replaced, once it has. Present is
    /// what makes a restart legal to ask for.
    pub replaced: Option<PathBuf>,
    /// Cancels a download in flight.
    pub cancel: Option<CancellationToken>,
    pub downloading: bool,
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
