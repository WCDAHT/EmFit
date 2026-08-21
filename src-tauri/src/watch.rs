//! Per-volume USN deletion watchers (roadmap M5, rescoped 2026-07-30).
//!
//! One thread per eligible volume polls the change journal on a ~1 s
//! cadence and emits `usn:deleted` events carrying the affected node ids.
//! Marks live entirely in the *frontend* session - nothing here or in the
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

use std::collections::HashSet;

use emfit_core::model::index::Index;
use emfit_core::service::task::CancellationToken;
use emfit_core::service::usn::{REASON_FILE_DELETE, REASON_RENAME_NEW_NAME, UsnPoll, UsnWatcher};
use tauri::{AppHandle, Emitter, Manager};

use crate::dto::{UsnDeletedDto, UsnGapDto};
use crate::state::AppState;

/// How often each watcher drains its journal.
const POLL_EVERY: Duration = Duration::from_secs(1);

/// MFT record numbers live in the low 48 bits; the index's native ids are
/// stored masked (`builder.rs`), USN FRNs arrive with the sequence counter
/// in the top 16. Every comparison happens at 48 bits.
const RECORD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// The record numbers of every directory under the volume's Recycle Bin -
/// a rename whose new parent is one of these IS a deletion, user-wise.
/// (`RECYCLER` is the pre-Vista spelling.) Limitation: a SID subfolder
/// created after the scan isn't in the index, so the first-ever recycle by
/// a brand-new user profile is missed until a rescan.
fn recycle_bin_dirs(index: &Index) -> HashSet<u64> {
    let mut out = HashSet::new();
    let mut stack: Vec<_> = index
        .children(index.root())
        .iter()
        .copied()
        .filter(|&id| {
            let name = index.name(id);
            name.eq_ignore_ascii_case("$recycle.bin") || name.eq_ignore_ascii_case("recycler")
        })
        .collect();
    while let Some(id) = stack.pop() {
        if !index.node(id).is_directory() {
            continue;
        }
        if let Some(native) = index.native_id(id) {
            out.insert(native & RECORD_MASK);
        }
        stack.extend(index.children(id).iter().copied());
    }
    out
}

/// Cancel the running watcher fleet (called when a scan starts - the
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
        .inspect(|v| {
            tracing::info!(
                key = %v.key,
                live_updates = v.index.caps().live_updates,
                drive_letter = ?volume_letter(&v.key),
                "usn: watcher eligibility"
            );
        })
        .filter(|v| v.index.caps().live_updates && volume_letter(&v.key).is_some())
        .map(|v| (v.key.clone(), Arc::clone(&v.index)))
        .collect();
    drop(inner);

    tracing::info!(count = volumes.len(), "usn: spawning deletion watchers");
    for (key, index) in volumes {
        spawn_one(app.clone(), key, index, cancel.clone());
    }
}

/// `"C:"` -> `'C'`; disk-image keys (paths) don't qualify.
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
    tracing::info!(key, %letter, "usn: opening journal");
    let mut watcher = match UsnWatcher::open(letter) {
        Ok(w) => w,
        Err(e) => {
            // Not fatal to anything: without the journal, deletions simply
            // wait for the next rescan (e.g. unelevated runs).
            tracing::warn!(key, error = %e, "usn: journal unavailable; deletion marking OFF");
            return;
        }
    };

    // FRN -> node id, once. The index is immutable for this watcher's whole
    // life (a rescan cancels it before installing a new index).
    //
    // Keying detail that decides whether marking works AT ALL: the index's
    // native ids are bare MFT record numbers - the builder masks to the low
    // 48 bits (`builder.rs`) - while USN records carry the full file
    // reference number with the sequence counter in the top 16 bits. Both
    // sides must be compared at 48 bits.
    let by_native: HashMap<u64, u32> = index
        .ids()
        .filter_map(|id| {
            index
                .native_id(id)
                .map(|native| (native & RECORD_MASK, id.get()))
        })
        .collect();

    // Plain Del is a RENAME into `$Recycle.Bin\<SID>\`, not a delete - the
    // record numbers of the bin's directories are what identify it.
    let recycle_dirs = recycle_bin_dirs(index);

    tracing::info!(
        key,
        mapped_nodes = by_native.len(),
        recycle_dirs = recycle_dirs.len(),
        "usn: watcher running"
    );

    let mut polls: u64 = 0;
    while !cancel.is_cancelled() {
        polls += 1;
        if polls.is_multiple_of(30) {
            tracing::trace!(key, polls, "usn: heartbeat (still polling)");
        }
        match watcher.poll() {
            Ok(UsnPoll::Events(events)) => {
                let mut ids: Vec<u32> = Vec::new();
                let (mut deletes, mut recycles, mut plain_renames) = (0u32, 0u32, 0u32);
                for event in &events {
                    let is_delete = event.reason & REASON_FILE_DELETE != 0;
                    let is_recycle = !is_delete
                        && event.reason & REASON_RENAME_NEW_NAME != 0
                        && recycle_dirs.contains(&(event.parent_frn & RECORD_MASK));
                    if is_delete {
                        deletes += 1;
                    } else if is_recycle {
                        recycles += 1;
                    } else {
                        plain_renames += 1;
                        continue; // ordinary rename/move: invisible by design
                    }
                    if let Some(&id) = by_native.get(&(event.frn & RECORD_MASK)) {
                        ids.push(id);
                    }
                }
                if !events.is_empty() {
                    tracing::trace!(
                        key,
                        events = events.len(),
                        deletes,
                        recycles,
                        ignored_renames = plain_renames,
                        matched_nodes = ids.len(),
                        sample_frn = format_args!("{:#018x}", events[0].frn),
                        sample_parent = format_args!("{:#018x}", events[0].parent_frn),
                        "usn: events observed"
                    );
                    if ids.is_empty() && deletes + recycles > 0 {
                        tracing::warn!(
                            key,
                            "usn: deletions/recycles observed but NONE matched the index - \
                             the FRN<->native-id mapping is the suspect"
                        );
                    }
                }
                if !ids.is_empty() && !cancel.is_cancelled() {
                    // The slot is resolved at emit time: other volumes'
                    // rescans can shift positions (and cancel this watcher
                    // moments later - the frontend clears marks then).
                    let state = app.state::<AppState>();
                    let vol = {
                        let inner = state.inner.lock().unwrap();
                        inner.volumes.iter().position(|v| v.key == key)
                    };
                    match vol {
                        Some(vol) => {
                            tracing::trace!(key, vol, ids = ids.len(), "usn: emitting usn:deleted");
                            let _ = app.emit(
                                "usn:deleted",
                                UsnDeletedDto {
                                    vol: vol as u16,
                                    ids,
                                },
                            );
                        }
                        None => tracing::warn!(
                            key,
                            "usn: volume key no longer in state; deletions dropped"
                        ),
                    }
                }
            }
            Ok(UsnPoll::Gap) => {
                tracing::warn!(
                    key,
                    "usn journal gap; deletion marks incomplete until rescan"
                );
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
