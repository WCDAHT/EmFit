//! The sweep: read the whole MFT and push what it says at a sink.
//!
//! One pass, front to back, in chunks bounded by what the extent map says is
//! contiguous. Records are parsed **in place** off the read buffer — no
//! per-record allocation, no copying a kilobyte out only to throw it away.
//!
//! Names are the exception, because they arrive as UTF-16 and the index wants
//! UTF-8. They go into one arena per batch which is reused across the whole
//! scan, so the cost is a few hundred allocations rather than a few million.
//!
//! # What this does not do yet
//!
//! - **Hard links.** A file with several `$FILE_NAME` attributes appears once,
//!   under its best name. Emitting one entry per link needs a decision about
//!   which link owns the bytes, or the rollup counts them twice
//!   (`features.md` §1.3). [`ScanStats::multi_linked_records`] counts what is
//!   being left out.
//! - **Extension records.** Attributes that overflowed into another record are
//!   skipped, so a file whose name or size lives there is missed or sized
//!   zero. [`ScanStats::extension_records`] counts them. The fix is the
//!   deferred-replay approach WizTree uses: stash the raw record when its base
//!   lies ahead of the sweep, replay after — no random seeks.

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use crate::error::Result;
use crate::model::entry::{EntryFlags, RawEntry, Times};
use crate::model::sink::{EntrySink, ScanWarning};
use crate::parser::block::{AlignedBuf, BlockSource};
use crate::parser::ntfs::attr::{AttributeType, FileName, Namespace, StandardInfo};
use crate::parser::ntfs::bitmap;
use crate::parser::ntfs::bootstrap::MftLayout;
use crate::parser::ntfs::record::{Record, RecordHeader};
use crate::service::task::{CancellationToken, Progress};

/// NTFS's root directory. Its `$FILE_NAME` names itself as its own parent,
/// which is exactly the convention the index builder recognizes.
pub const ROOT_RECORD: u64 = 5;

/// Records 0–15 are the filesystem's own metadata — `$MFT`, `$LogFile`,
/// `$Bitmap` and friends. They are real files that occupy real space, so they
/// are scanned like anything else; this is only where user files begin.
pub const FIRST_USER_RECORD: u64 = 16;

/// How much to read at once. Large enough that per-read overhead disappears,
/// small enough to stay off the large-page path and out of the way.
pub const DEFAULT_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Tuning for one sweep.
#[derive(Debug, Clone, Copy)]
pub struct ScanOptions {
    /// Bytes per read. Clamped to at least one record.
    pub chunk_bytes: usize,
    /// Emit a progress tick at most this often. Ticking per record would cost
    /// more than the parsing does.
    pub progress_interval: Duration,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            chunk_bytes: DEFAULT_CHUNK_BYTES,
            progress_interval: Duration::from_millis(100),
        }
    }
}

/// What one sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// Record slots read.
    pub records_read: u64,
    /// Records carrying the in-use flag.
    pub records_in_use: u64,
    /// Entries handed to the sink.
    pub entries_emitted: u64,
    pub files: u64,
    pub directories: u64,
    /// In use, but the header would not parse — corrupt or misread.
    pub bad_records: u64,
    /// In use, but the fixup check failed. A torn write.
    pub failed_fixup: u64,
    /// In use with no usable `$FILE_NAME`, so nothing could be emitted.
    pub unnamed_records: u64,
    /// Extension records skipped. Each is a file whose name or size may be
    /// wrong until deferred replay lands.
    pub extension_records: u64,
    /// Records with more than one real name — hard links not yet emitted.
    pub multi_linked_records: u64,
    /// Records whose `$FILE_NAME` was only an 8.3 alias.
    pub dos_only_records: u64,
    pub bytes_read: u64,
    /// Reads issued. Compare against `records_read` to see the batching work.
    pub reads: u64,
    pub elapsed: Duration,
    /// Live records the allocation bitmap reported, when it was readable.
    pub bitmap_in_use: Option<u64>,
    /// True when the sink asked to stop before the end.
    pub cancelled: bool,
}

impl ScanStats {
    /// Records per second, over the whole sweep.
    pub fn records_per_second(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            return 0.0;
        }
        self.records_read as f64 / secs
    }

    /// Read throughput in mebibytes per second.
    pub fn mib_per_second(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            return 0.0;
        }
        self.bytes_read as f64 / secs / (1024.0 * 1024.0)
    }
}

/// Read the whole MFT into `sink`.
pub fn sweep(
    source: &dyn BlockSource,
    layout: &MftLayout,
    sink: &mut dyn EntrySink,
    progress: &mut dyn FnMut(Progress),
    cancel: &CancellationToken,
    options: ScanOptions,
) -> Result<ScanStats> {
    let started = Instant::now();
    let mut stats = ScanStats::default();

    let record_size = u64::from(layout.boot.bytes_per_record);
    let total_records = layout.records_to_scan().min(layout.capacity_records());

    // The bitmap turns "slots ever written" into "files actually here", which
    // is what progress should count against and what the builder should size
    // its arrays for. Failing to read it costs accuracy, not correctness.
    if layout.has_bitmap() {
        match bitmap::read(
            source,
            &layout.bitmap_runs,
            layout.boot.bytes_per_cluster,
            layout.bitmap_size,
        ) {
            Ok(bits) => stats.bitmap_in_use = Some(bits.count_in_use()),
            Err(e) => {
                tracing::debug!(error = %e, "could not read $MFT bitmap");
                sink.warn(ScanWarning::BadRecord {
                    fs_id: 0,
                    reason: format!("$MFT bitmap unreadable: {e}"),
                });
            }
        }
    }

    let expected = stats.bitmap_in_use.unwrap_or(total_records);
    progress(Progress::started("Reading the MFT", Some(expected)));

    let records_per_chunk = (options.chunk_bytes as u64 / record_size).max(1);
    let mut buf = AlignedBuf::for_source(source, (records_per_chunk * record_size) as usize);
    let mut batch = Batch::with_capacity(records_per_chunk as usize);

    let mut record = 0u64;
    let mut last_tick = Instant::now();

    while record < total_records {
        if cancel.is_cancelled() {
            stats.cancelled = true;
            break;
        }

        let Some(location) = layout.extents.locate(record) else {
            // Past the mapped extents: nothing left to read.
            break;
        };

        // Never read past the end of the fragment this record sits in. Without
        // this the tail of a chunk is whatever happens to lie next on disk.
        let to_read = location
            .contiguous_records
            .min(records_per_chunk)
            .min(total_records - record);
        if to_read == 0 {
            // The record straddles a fragment boundary, which only happens
            // when records are larger than clusters. Skipping keeps the sweep
            // moving rather than stalling on one pathological record.
            record += 1;
            continue;
        }

        let span = (to_read * record_size) as usize;
        source.read_exact_at(location.byte_offset, &mut buf.as_mut_slice()[..span])?;
        stats.reads += 1;
        stats.bytes_read += span as u64;

        batch.clear();

        let chunk = &mut buf.as_mut_slice()[..span];
        for (index, record_bytes) in chunk.chunks_exact_mut(record_size as usize).enumerate() {
            let number = record + index as u64;
            stats.records_read += 1;
            parse_one(
                number,
                record_bytes,
                layout.boot.bytes_per_sector,
                &mut batch,
                &mut stats,
                sink,
            );
        }

        // Build the borrowed view only now that the arena has stopped moving.
        let entries: Vec<RawEntry<'_>> = batch
            .pending
            .iter()
            .map(|p| RawEntry {
                fs_id: p.fs_id,
                parent_id: p.parent_id,
                name: &batch.names[p.name_start..p.name_end],
                size: p.size,
                allocated: p.allocated,
                times: p.times,
                flags: p.flags,
            })
            .collect();

        stats.entries_emitted += entries.len() as u64;
        if sink.push_batch(&entries) == ControlFlow::Break(()) {
            stats.cancelled = true;
            break;
        }

        record += to_read;

        if last_tick.elapsed() >= options.progress_interval {
            progress(Progress::Tick {
                done: stats.entries_emitted,
            });
            last_tick = Instant::now();
        }
    }

    stats.elapsed = started.elapsed();
    progress(Progress::Finished);

    tracing::info!(
        records = stats.records_read,
        in_use = stats.records_in_use,
        emitted = stats.entries_emitted,
        reads = stats.reads,
        elapsed_ms = stats.elapsed.as_millis(),
        "MFT sweep complete"
    );

    Ok(stats)
}

/// One entry's metadata, with its name held as a range into the shared arena.
///
/// Indices rather than a `&str` because the arena is still growing while these
/// are collected; borrowing would pin it.
struct Pending {
    fs_id: u64,
    parent_id: u64,
    name_start: usize,
    name_end: usize,
    size: u64,
    allocated: u64,
    times: Times,
    flags: EntryFlags,
}

/// Scratch space reused across every batch of the sweep, so a scan of five
/// million records makes a handful of allocations rather than millions.
struct Batch {
    /// Every name in this batch, concatenated.
    names: String,
    /// One per entry, indexing into `names`.
    pending: Vec<Pending>,
    /// Where a candidate name is decoded before it is judged.
    scratch: String,
    /// The best name seen so far for the record being parsed.
    best_name: String,
}

impl Batch {
    fn with_capacity(records: usize) -> Self {
        Self {
            names: String::with_capacity(64 * 1024),
            pending: Vec::with_capacity(records),
            scratch: String::with_capacity(256),
            best_name: String::with_capacity(256),
        }
    }

    fn clear(&mut self) {
        self.names.clear();
        self.pending.clear();
    }
}

/// Parse one record and append whatever it yields to the batch.
fn parse_one(
    number: u64,
    record_bytes: &mut [u8],
    bytes_per_sector: u32,
    batch: &mut Batch,
    stats: &mut ScanStats,
    sink: &mut dyn EntrySink,
) {
    // Cheapest check first: a free record needs no repair and no walking, and
    // a large share of any used volume's table is free.
    let Ok(header) = RecordHeader::parse(record_bytes) else {
        // Free slots are usually zeroed, so a bad signature is only worth
        // counting once we know the record claims to be in use — which we
        // cannot know without a header. Count it and move on.
        stats.bad_records += 1;
        return;
    };
    if !header.is_in_use() {
        return;
    }
    stats.records_in_use += 1;

    // An extension record's attributes belong to another file. Skipping it
    // leaves that file short a name or a size; counting them says how often.
    if header.base_record().is_some() {
        stats.extension_records += 1;
        return;
    }

    let record = match Record::parse(record_bytes, bytes_per_sector) {
        Ok(record) => record,
        Err(e) => {
            stats.failed_fixup += 1;
            sink.warn(ScanWarning::BadRecord {
                fs_id: number,
                reason: e.to_string(),
            });
            return;
        }
    };

    let is_directory = record.is_directory();
    let mut times = Times::default();
    let mut flags = if is_directory {
        EntryFlags::DIRECTORY
    } else {
        EntryFlags::empty()
    };
    let mut size = 0u64;
    let mut allocated = 0u64;
    let mut best: Option<(Namespace, u64)> = None;
    let mut real_names = 0u32;
    let mut saw_dos_name = false;

    for attribute in record.attributes() {
        match attribute.kind() {
            AttributeType::StandardInformation => {
                if let Some(value) = attribute.resident_value()
                    && let Some(si) = StandardInfo::parse(value)
                {
                    times = Times {
                        mtime: si.modified,
                        crtime: si.created,
                    };
                    flags = flags.union(dos_attributes_to_flags(si.attributes));
                }
            }
            AttributeType::FileName => {
                let Some(value) = attribute.resident_value() else {
                    continue;
                };
                let Some(fname) = FileName::parse(value) else {
                    continue;
                };
                let namespace = fname.namespace();
                // An 8.3 alias is another spelling of a name the file already
                // has, not another place it lives.
                if !namespace.is_real_name() {
                    saw_dos_name = true;
                    continue;
                }
                real_names += 1;

                // Prefer Win32 over POSIX. Most files have exactly one real
                // name, so this rarely changes anything.
                if !is_better(namespace, best.map(|(ns, _)| ns)) {
                    continue;
                }
                fname.decode_name_into(&mut batch.scratch);
                if batch.scratch.is_empty() {
                    continue;
                }
                // Hold the winner aside rather than committing it to the
                // arena: a later attribute may still displace it, and the
                // arena has no way to take a name back.
                std::mem::swap(&mut batch.scratch, &mut batch.best_name);
                best = Some((namespace, fname.parent_record()));
            }
            // The unnamed $DATA is the file's contents; a named one is an
            // alternate stream, whose bytes count toward the file but are not
            // its size.
            AttributeType::Data if attribute.is_unnamed() => {
                if let Some(nr) = attribute.non_resident() {
                    if nr.starting_vcn() == 0 {
                        size = nr.data_size();
                        allocated = nr.allocated_size();
                    }
                } else if let Some(value) = attribute.resident_value() {
                    // Resident: the contents live in the record itself, so the
                    // file occupies no clusters at all.
                    size = value.len() as u64;
                    allocated = 0;
                }
            }
            _ => {}
        }
    }

    if real_names > 1 {
        stats.multi_linked_records += 1;
    }

    let Some((_, parent)) = best else {
        // In use, but nothing usable to call it. Either its $FILE_NAME lives
        // in an extension record, or it only ever had an 8.3 alias.
        stats.unnamed_records += 1;
        if saw_dos_name {
            stats.dos_only_records += 1;
        }
        return;
    };

    // Commit the winning name to the arena, once.
    let start = batch.names.len();
    batch.names.push_str(&batch.best_name);
    batch.pending.push(Pending {
        fs_id: number,
        parent_id: parent,
        name_start: start,
        name_end: batch.names.len(),
        size,
        allocated,
        times,
        flags,
    });

    if is_directory {
        stats.directories += 1;
    } else {
        stats.files += 1;
    }
}

/// Whether `candidate` should displace the namespace chosen so far.
fn is_better(candidate: Namespace, current: Option<Namespace>) -> bool {
    fn rank(ns: Namespace) -> u8 {
        match ns {
            Namespace::Win32 => 3,
            Namespace::Win32AndDos => 2,
            Namespace::Posix => 1,
            Namespace::Dos | Namespace::Unknown(_) => 0,
        }
    }
    match current {
        None => true,
        Some(current) => rank(candidate) > rank(current),
    }
}

/// Map the DOS attribute bits onto the index's flags.
fn dos_attributes_to_flags(attributes: u32) -> EntryFlags {
    const HIDDEN: u32 = 0x0000_0002;
    const SYSTEM: u32 = 0x0000_0004;
    const REPARSE: u32 = 0x0000_0400;
    const COMPRESSED: u32 = 0x0000_0800;
    const SPARSE: u32 = 0x0000_0200;

    let mut flags = EntryFlags::empty();
    if attributes & HIDDEN != 0 {
        flags = flags.union(EntryFlags::HIDDEN);
    }
    if attributes & SYSTEM != 0 {
        flags = flags.union(EntryFlags::SYSTEM);
    }
    if attributes & REPARSE != 0 {
        flags = flags.union(EntryFlags::REPARSE);
    }
    if attributes & COMPRESSED != 0 {
        flags = flags.union(EntryFlags::COMPRESSED);
    }
    if attributes & SPARSE != 0 {
        flags = flags.union(EntryFlags::SPARSE);
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win32_names_win_over_posix_and_dos() {
        assert!(is_better(Namespace::Win32, None));
        assert!(is_better(Namespace::Win32, Some(Namespace::Posix)));
        assert!(is_better(Namespace::Win32, Some(Namespace::Win32AndDos)));
        assert!(!is_better(Namespace::Posix, Some(Namespace::Win32)));
        assert!(!is_better(Namespace::Win32, Some(Namespace::Win32)));
        assert!(!is_better(Namespace::Dos, Some(Namespace::Posix)));
    }

    #[test]
    fn dos_attribute_bits_map_across() {
        let flags = dos_attributes_to_flags(0x0000_0006);
        assert!(flags.contains(EntryFlags::HIDDEN));
        assert!(flags.contains(EntryFlags::SYSTEM));
        assert!(!flags.contains(EntryFlags::REPARSE));

        let reparse = dos_attributes_to_flags(0x0000_0400);
        assert!(reparse.contains(EntryFlags::REPARSE));

        assert_eq!(dos_attributes_to_flags(0), EntryFlags::empty());
    }

    #[test]
    fn rates_are_zero_rather_than_infinite_for_an_instant_scan() {
        let stats = ScanStats::default();
        assert_eq!(stats.records_per_second(), 0.0);
        assert_eq!(stats.mib_per_second(), 0.0);
    }

    #[test]
    fn rates_divide_by_elapsed() {
        let stats = ScanStats {
            records_read: 1_000_000,
            bytes_read: 1024 * 1024 * 512,
            elapsed: Duration::from_secs(2),
            ..Default::default()
        };
        assert_eq!(stats.records_per_second(), 500_000.0);
        assert_eq!(stats.mib_per_second(), 256.0);
    }
}
