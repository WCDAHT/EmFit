//! Bringing a cached snapshot up to date from the USN journal.
//!
//! The journal is read as a **change list and nothing else**: it says which
//! MFT records were touched, and those records are then re-read off the volume
//! and believed. No meaning is taken from the reason bits beyond "touched" and
//! "deleted", and no field is ever taken from a journal record.
//!
//! That is the whole trick, and it is what makes replay safe (`caching.md`
//! sec 2.2). Reason bits arrive coalesced and out of order, and a journal
//! record carries no size at all, so a design that applied events would have
//! to be right about every ordering case. Re-reading is right about all of
//! them at once: whatever the record says now is what the index gets. Three
//! consequences worth stating, because they are the cases that usually break
//! incremental indexes:
//!
//! - **A record slot reused by a different file** comes out correct, because
//!   the node keyed by that record is replaced wholesale rather than edited.
//! - **A renamed or moved directory** costs one node update. Parents are held
//!   as filesystem ids and paths are walked, so the whole subtree follows and
//!   NTFS never has to emit a per-descendant event.
//! - **A hard link added or removed** comes out correct, because every node of
//!   a record is dropped together and the record's current set of names is
//!   inserted in their place.
//!
//! What replay does *not* do is decide anything. It produces a
//! [`Patch`](crate::service::cache::Patch); rebuilding the index from it is
//! [`crate::service::cache::snapshot::rebuild`]'s job, and it re-runs the same
//! link and rollup passes a scan does rather than patching totals along the
//! parent chain. At about 150 ms for three million nodes, an incremental
//! rollup would not be worth the class of bug it invites.

use std::collections::HashSet;

use crate::error::Result;
use crate::model::index::FS_OBJECT_MASK;
use crate::model::sink::RecordingSink;
use crate::parser::ntfs::bootstrap::MftLayout;
use crate::parser::ntfs::live::LiveRecords;
use crate::parser::ntfs::scanner;
use crate::service::cache::Patch;
use crate::service::cache::manifest::JournalStamp;
use crate::service::task::CancellationToken;
use crate::service::usn::{self, JournalRead, REASON_FILE_DELETE};

/// Fewest records worth replaying rather than rescanning, whatever the volume
/// size. Below this the arithmetic never favours a sweep.
///
/// Records are fetched one ioctl at a time, because that is the only way to
/// see what the filesystem currently believes ([`crate::parser::ntfs::live`]),
/// so the cost is per record rather than per run: call it ten microseconds
/// each, against about thirteen seconds to sweep a three-million-file volume.
/// Estimates until the BenchLog holds both sides of the crossover.
const MIN_RECORD_BUDGET: u64 = 100_000;

/// Share of a volume's nodes past which a sweep is the better deal.
const RECORD_BUDGET_SHARE: u64 = 40;

/// Journal records to look at before giving up. Generous next to the record
/// budget because one record is touched many times: created, written, closed.
const EVENT_BUDGET_FACTOR: u64 = 8;

/// Why a snapshot could not be brought up to date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    /// The snapshot carries no journal position - a disk image, or a volume
    /// whose journal was off when it was scanned.
    NoPosition,
    /// The journal was recreated or wrapped past the saved position. What
    /// happened in between is unknowable.
    Gap,
    /// More changes than replaying could pay for.
    TooMuch { records: u64, events: u64 },
    /// Records whose attributes live in a non-resident `$ATTRIBUTE_LIST`. Only
    /// a full sweep can follow those, and emitting the record without them
    /// would report a wrong size (`caching.md` sec 6.4).
    Opaque { records: u64 },
    /// Records that would not parse. One torn record is not worth guessing
    /// about when a sweep is the correct answer and costs seconds.
    Unparsable { records: u64 },
    /// The journal could not be read at all: no journal on the volume, or no
    /// Administrator rights.
    Unavailable { reason: String },
}

impl Blocked {
    /// One line for the log and the status bar.
    pub fn explain(&self) -> String {
        match self {
            Self::NoPosition => "the cached scan recorded no journal position".to_string(),
            Self::Gap => "the change journal wrapped or was recreated".to_string(),
            Self::TooMuch { records, events } => {
                format!("too much changed ({records} records, {events} journal entries)")
            }
            Self::Opaque { records } => {
                format!("{records} records need a full read of their attribute lists")
            }
            Self::Unparsable { records } => format!("{records} records would not parse"),
            Self::Unavailable { reason } => format!("the change journal is unavailable: {reason}"),
        }
    }
}

/// What replaying the journal did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReplayStats {
    pub events: u64,
    /// Records the journal named, plus whatever their attribute lists pointed
    /// at.
    pub records: u64,
    /// Entries the re-read produced. Fewer than `records` when files were
    /// deleted, more when they have several names.
    pub entries: u64,
    pub reads: u64,
    pub bytes_read: u64,
}

/// The result of trying to bring a snapshot up to date.
pub enum Replay {
    /// Nothing has changed since the snapshot was taken.
    Unchanged { journal: JournalStamp },
    /// Changes, turned into a patch to rebuild with.
    Changed {
        patch: Patch,
        journal: JournalStamp,
        stats: ReplayStats,
    },
    /// Not possible; the caller has to scan.
    Blocked(Blocked),
}

/// Read the journal since `from` and re-read whatever it says changed.
///
/// `nodes` is the cached index's size, which sets how much change is worth
/// replaying before a sweep becomes the cheaper answer.
pub fn replay(
    drive_letter: char,
    layout: &MftLayout,
    from: JournalStamp,
    nodes: usize,
    cancel: &CancellationToken,
) -> Result<Replay> {
    let record_budget = MIN_RECORD_BUDGET.max(nodes as u64 / RECORD_BUDGET_SHARE);
    let event_budget = record_budget.saturating_mul(EVENT_BUDGET_FACTOR);

    let read = match usn::read_since(drive_letter, from.id, from.next_usn, event_budget, cancel) {
        Ok(read) => read,
        Err(crate::error::Error::Cancelled) => return Err(crate::error::Error::Cancelled),
        Err(e) => {
            return Ok(Replay::Blocked(Blocked::Unavailable {
                reason: e.to_string(),
            }));
        }
    };

    let (events, next_usn) = match read {
        JournalRead::Gap => return Ok(Replay::Blocked(Blocked::Gap)),
        JournalRead::Flooded { seen } => {
            return Ok(Replay::Blocked(Blocked::TooMuch {
                records: 0,
                events: seen,
            }));
        }
        JournalRead::Records { events, next_usn } => (events, next_usn),
    };

    let journal = JournalStamp {
        id: from.id,
        next_usn,
    };

    // The reason bits are read for exactly one thing: nothing. A delete and a
    // write both mean "look at this record again".
    let mut touched: HashSet<u64> = HashSet::with_capacity(events.len());
    let mut deletes = 0u64;
    for event in &events {
        if event.reason & REASON_FILE_DELETE != 0 {
            deletes += 1;
        }
        touched.insert(event.frn & FS_OBJECT_MASK);
    }

    if touched.is_empty() {
        tracing::info!(
            events = events.len(),
            "replay: journal had nothing that names a record"
        );
        return Ok(Replay::Unchanged { journal });
    }
    if touched.len() as u64 > record_budget {
        return Ok(Replay::Blocked(Blocked::TooMuch {
            records: touched.len() as u64,
            events: events.len() as u64,
        }));
    }

    let mut records: Vec<u64> = touched.into_iter().collect();
    records.sort_unstable();

    // Through the filesystem, never off the platter. The records the journal
    // just named are exactly the ones NTFS has not written down yet, so a raw
    // read of them shows a created file's slot still free, a deleted file's
    // still in use, and a written file's old size (see `live`). If the driver
    // will not serve records, a sweep is the honest answer.
    let mut live = match LiveRecords::open(
        drive_letter,
        layout.boot.bytes_per_sector,
        layout.boot.bytes_per_record,
    ) {
        Ok(live) => live,
        Err(e) => {
            return Ok(Replay::Blocked(Blocked::Unavailable {
                reason: format!("the filesystem will not serve file records ({e})"),
            }));
        }
    };

    let mut sink = RecordingSink::new();
    let outcome = scanner::read_records(
        &mut scanner::RecordSource::Driver(&mut live),
        layout,
        &records,
        &mut sink,
        cancel,
    )?;
    if outcome.stats.cancelled {
        return Err(crate::error::Error::Cancelled);
    }
    if !outcome.opaque.is_empty() {
        return Ok(Replay::Blocked(Blocked::Opaque {
            records: outcome.opaque.len() as u64,
        }));
    }
    // A record that fails its fixup check has been read wrong, and a replay
    // that quietly dropped it would delete a live file from the index.
    if outcome.stats.failed_fixup > 0 || outcome.stats.bad_records > 0 {
        return Ok(Replay::Blocked(Blocked::Unparsable {
            records: outcome.stats.failed_fixup + outcome.stats.bad_records,
        }));
    }

    // Every record the read actually visited, not just the ones the journal
    // named: following an attribute list pulls in extension records and their
    // bases, and a base whose entries are re-emitted must have its old nodes
    // dropped or the file ends up in the index twice.
    let replace: HashSet<u64> = outcome.read.iter().copied().collect();
    let entries = sink.into_entries();

    let stats = ReplayStats {
        events: events.len() as u64,
        records: replace.len() as u64,
        entries: entries.len() as u64,
        reads: outcome.stats.reads,
        bytes_read: outcome.stats.bytes_read,
    };
    tracing::info!(
        events = stats.events,
        deletes,
        records = stats.records,
        entries = stats.entries,
        reads = stats.reads,
        kib_read = stats.bytes_read / 1024,
        "replay: journal applied"
    );

    Ok(Replay::Changed {
        patch: Patch {
            replace,
            entries,
            free_bytes: None,
        },
        journal,
        stats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_block_reason_says_something_a_user_can_act_on() {
        let reasons = [
            Blocked::NoPosition,
            Blocked::Gap,
            Blocked::TooMuch {
                records: 1,
                events: 2,
            },
            Blocked::Opaque { records: 3 },
            Blocked::Unavailable {
                reason: "access denied".to_string(),
            },
        ];
        for reason in reasons {
            let text = reason.explain();
            assert!(!text.is_empty());
            assert!(
                !text.chars().next().is_some_and(char::is_uppercase),
                "reads as a clause inside a sentence, not a sentence: {text}"
            );
        }
    }

    #[test]
    fn the_record_budget_scales_with_the_volume_but_never_below_the_floor() {
        let budget = |nodes: usize| MIN_RECORD_BUDGET.max(nodes as u64 / RECORD_BUDGET_SHARE);
        assert_eq!(
            budget(1_000),
            MIN_RECORD_BUDGET,
            "a tiny volume still gets the floor"
        );
        assert_eq!(
            budget(3_400_000),
            MIN_RECORD_BUDGET,
            "a real C: takes the floor"
        );
        assert_eq!(
            budget(8_000_000),
            200_000,
            "a big volume scales past the floor"
        );
        assert_eq!(
            budget(50_000_000),
            1_250_000,
            "a huge volume scales further"
        );
    }
}
