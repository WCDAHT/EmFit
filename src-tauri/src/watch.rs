//! Per-volume USN deletion watchers (roadmap M5, rescoped 2026-07-30).
//!
//! One thread per eligible volume polls the change journal on a ~1 s
//! cadence and emits `usn:deleted` events carrying the affected node ids.
//! Marks live entirely in the *frontend* session — nothing here or in the
//! index mutates: a deletion is a red border until the next rescan, by
//! design. That is also why every watcher dies when a scan starts and the
//! whole fleet respawns when it finishes: rescans change volume slots, so
//! the frontend clears all marks on `scan:done` and the watchers start
//! fresh from the journal's new tail.
//!
//! Eligible: a real mounted volume (`X:` key, not a disk image) whose caps
//! declare `live_updates`. Deletions landing between scan completion and
//! watcher start are missed; the next rescan reconciles.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use emfit_core::model::index::Index;
use emfit_core::service::task::CancellationToken;
use emfit_core::service::usn::{UsnPoll, UsnWatcher};
use tauri::{AppHandle, Emitter, Manager};

use crate::dto::{UsnDeletedDto, UsnGapDto};
use crate::state::AppState;

/// How often each watcher drains its journal.
const POLL_EVERY: Duration = Duration::from_secs(1);

/// Cancel the running watcher fleet (called when a scan starts — the
/// indexes those watchers map against are about to be replaced).
pub fn stop_all(state: &AppState) {
    let mut inner = state.inner.lock().unwrap();
    if let Some(cancel) = inner.watch_cancel.take() {
        cancel.cancel();
    }
}

/// Spawn a watcher for every eligible scanned volume, replacing whatever
/// fleet existed. Called after a scan batch completes.
pub fn respawn_all(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut inner = state.inner.lock().unwrap();
    if let Some(old) = inner.watch_cancel.take() {
        old.cancel();
    }
    let cancel = CancellationToken::new();
    inner.watch_cancel = Some(cancel.clone());

    let volumes: Vec<(String, Arc<Index>)> = inner
        .volumes
        .iter()
        .filter(|v| v.index.caps().live_updates && volume_letter(&v.key).is_some())
        .map(|v| (v.key.clone(), Arc::clone(&v.index)))
        .collect();
    drop(inner);

    for (key, index) in volumes {
        spawn_one(app.clone(), key, index, cancel.clone());
    }
}

/// `"C:"` → `'C'`; disk-image keys (paths) don't qualify.
fn volume_letter(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let letter = chars.next()?;
    (letter.is_ascii_alphabetic() && chars.as_str() == ":").then_some(letter)
}

fn spawn_one(app: AppHandle, key: String, index: Arc<Index>, cancel: CancellationToken) {
    let name = format!("usn-watch-{key}");
    if let Err(e) = std::thread::Builder::new()
        .name(name.clone())
        .spawn(move || watch_volume(&app, &key, &index, &cancel))
    {
        tracing::error!(name, error = %e, "usn watcher thread failed to spawn");
    }
}

fn watch_volume(app: &AppHandle, key: &str, index: &Index, cancel: &CancellationToken) {
    let Some(letter) = volume_letter(key) else {
        return;
    };
    let mut watcher = match UsnWatcher::open(letter) {
        Ok(w) => w,
        Err(e) => {
            // Not fatal to anything: without the journal, deletions simply
            // wait for the next rescan (e.g. unelevated runs).
            tracing::info!(key, error = %e, "usn journal unavailable; deletion marking off");
            return;
        }
    };

    // FRN → node id, once. The index is immutable for this watcher's whole
    // life (a rescan cancels it before installing a new index).
    //
    // Keying detail that decides whether marking works AT ALL: the index's
    // native ids are bare MFT record numbers — the builder masks to the low
    // 48 bits (`builder.rs`) — while USN records carry the full file
    // reference number with the sequence counter in the top 16 bits. Both
    // sides must be compared at 48 bits.
    const RECORD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
    let by_native: HashMap<u64, u32> = index
        .ids()
        .filter_map(|id| index.native_id(id).map(|native| (native & RECORD_MASK, id.get())))
        .collect();

    tracing::debug!(key, nodes = by_native.len(), "usn deletion watcher running");

    while !cancel.is_cancelled() {
        match watcher.poll() {
            Ok(UsnPoll::Deletions(frns)) => {
                let ids: Vec<u32> = frns
                    .iter()
                    .filter_map(|frn| by_native.get(&(frn & RECORD_MASK)).copied())
                    .collect();
                if !ids.is_empty() && !cancel.is_cancelled() {
                    // The slot is resolved at emit time: other volumes'
                    // rescans can shift positions (and cancel this watcher
                    // moments later — the frontend clears marks then).
                    let state = app.state::<AppState>();
                    let vol = {
                        let inner = state.inner.lock().unwrap();
                        inner.volumes.iter().position(|v| v.key == key)
                    };
                    if let Some(vol) = vol {
                        let _ = app.emit(
                            "usn:deleted",
                            UsnDeletedDto {
                                vol: vol as u16,
                                ids,
                            },
                        );
                    }
                }
            }
            Ok(UsnPoll::Gap) => {
                tracing::warn!(key, "usn journal gap; deletion marks incomplete until rescan");
                let _ = app.emit(
                    "usn:gap",
                    UsnGapDto {
                        volume: key.to_string(),
                    },
                );
                return;
            }
            Err(e) => {
                tracing::warn!(key, error = %e, "usn poll failed; deletion marking off");
                return;
            }
        }

        // Sleep in short slices so cancellation lands promptly.
        for _ in 0..10 {
            if cancel.is_cancelled() {
                return;
            }
            std::thread::sleep(POLL_EVERY / 10);
        }
    }
}
