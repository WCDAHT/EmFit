//! The sweep: read the whole MFT and push what it says at a sink.
//!
//! One pass, front to back, in chunks bounded by what the extent map says is
//! contiguous. Records are parsed **in place** off the read buffer - no
//! per-record allocation, no copying a kilobyte out only to throw it away.
//!
//! Names are the exception, because they arrive as UTF-16 and the index wants
//! UTF-8. They go into one arena per parse batch, reused across the whole
//! scan, so the cost is a few hundred allocations rather than a few million.
//!
//! # Overlapped, double-buffered I/O
//!
//! A dedicated reader thread issues the next read while the current chunk is
//! being parsed, cycling two sector-aligned buffers through a pair of
//! channels. v1 was a strictly serial read->parse->read loop with queue depth 1;
//! the disk idled during every parse and the CPU idled during every read.
//!
//! # Parallel parse
//!
//! Each chunk is fanned out across rayon threads, every worker accumulating
//! into its own scratch ([`WorkerOut`]) so the hot path takes no locks. The
//! outputs are merged and flushed to the sink on the coordinating thread - the
//! sink stays single-threaded and dumb, per `architecture.md` sec 8.
//!
//! # Extension records
//!
//! A file with more attributes than fit in 1024 bytes - many hard links, a
//! `$DATA` run list fragmented into hundreds of pieces - spills into
//! *extension records*. Those appear in the sweep like any other record and
//! carry a reference to the file they belong to.
//!
//! The naive fix is to seek to them on demand, which is what v1 did: two
//! random 1 KB reads per record, on a volume with tens of thousands of them.
//! Instead, both halves are collected as the sweep passes over them and joined
//! at the end - **no seeking at all**, since everything is read exactly once in
//! sequential order regardless. WizTree does the same, deferring only the
//! records whose base lies ahead of the read head; collecting both sides makes
//! the result independent of which came first.
//!
//! This matters more than the record count suggests. Attribute lists appear
//! precisely when a file's `$DATA` is heavily fragmented, which correlates with
//! it being **large** - so skipping extension records loses a few percent of
//! the files and a large share of the bytes.
//!
//! # Hard links
//!
//! A file with several `$FILE_NAME` attributes is several real paths sharing
//! one blob of data. Every name is emitted as its own entry carrying the full
//! logical size, and exactly one of them carries the allocated bytes; the rest
//! are flagged [`EntryFlags::ALIAS`] with zero allocated.
//!
//! That keeps both questions answerable: `total_size` says how much file is
//! listed somewhere, `total_allocated` says how much disk it costs. Counting
//! the bytes once per link would report a WinSxS-heavy volume as several times
//! its real size.
//!
//! # Alternate data streams
//!
//! A named `$DATA` attribute is an ADS. Its **allocated** bytes are folded
//! into the owning file's allocated size - they genuinely occupy disk, and
//! WizTree accounts them the same way - while the logical size is *not* added
//! to the file's, because streams like `$BadClus:$Bad` report a logical size
//! equal to the whole volume while occupying nothing. Each stream is also
//! recorded individually in [`SweepOutcome::ads`], keyed by the owning MFT
//! record, so a detail pane can list them later.

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::error::{Error, Result};
use crate::model::entry::{EntryFlags, RawEntry, Times};
use crate::model::sink::{EntrySink, ScanWarning};
use crate::parser::block::{AlignedBuf, BlockSource};
use crate::parser::ntfs::attr::{
    self, AttributeType, FileName, Namespace, NonResident, StandardInfo,
};
use crate::parser::ntfs::bitmap;
use crate::parser::ntfs::bootstrap::MftLayout;
use crate::parser::ntfs::live::LiveRecords;
use crate::parser::ntfs::record::{Record, RecordForm, RecordHeader};
use crate::service::task::{CancellationToken, Progress};

/// NTFS's root directory. Its `$FILE_NAME` names itself as its own parent,
/// which is exactly the convention the index builder recognizes.
pub const ROOT_RECORD: u64 = 5;

/// Records 0-15 are the filesystem's own metadata - `$MFT`, `$LogFile`,
/// `$Bitmap` and friends. They are real files occupying real space, so they are
/// scanned like anything else; this is only where user files begin.
pub const FIRST_USER_RECORD: u64 = 16;

/// How much to read at once. Large enough that per-read overhead disappears,
/// small enough to stay out of the way.
pub const DEFAULT_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// How many held-back entries to emit per batch once the sweep is done.
const RESOLVE_BATCH: usize = 4096;

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

/// One alternate data stream found during the sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdsStream {
    /// MFT record number of the owning file.
    pub owner: u64,
    /// The stream's name - the `secret` in `file.txt:secret`.
    pub name: String,
    /// Logical bytes. Can be enormous for sparse system streams
    /// (`$BadClus:$Bad` spans the volume), which is why it is reported here
    /// but never added to the owner's logical size.
    pub size: u64,
    /// Bytes actually occupied on disk. Folded into the owner's allocated
    /// size during the sweep.
    pub allocated: u64,
}

/// Everything one sweep produced besides what went into the sink.
#[derive(Debug, Default)]
pub struct SweepOutcome {
    pub stats: ScanStats,
    /// Every alternate data stream seen, keyed by owning record. A side table
    /// per `architecture.md` sec 7 rule 2 - never widens `Node`.
    pub ads: Vec<AdsStream>,
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

    /// Slots whose first four bytes were zero - never written, not corrupt.
    pub never_used_slots: u64,
    /// Slots with a non-zero, non-`FILE` signature. Genuinely unexpected.
    pub bad_records: u64,
    /// In use, but the fixup check failed. A torn write.
    pub failed_fixup: u64,
    /// In use with no usable `$FILE_NAME` anywhere, so nothing was emitted.
    pub unnamed_records: u64,
    /// Records whose only name was an 8.3 alias.
    pub dos_only_records: u64,

    /// Extension records found and harvested.
    pub extension_records: u64,
    /// Base records held back until their extensions were known.
    pub deferred_bases: u64,
    /// Held-back records that actually gained a name or a size from an
    /// extension. The rest had attribute lists that pointed nowhere useful.
    pub resolved_from_extensions: u64,
    /// Extension records whose base never appeared - a deleted base, or one
    /// whose attribute list we could not read. Their attributes are lost.
    pub orphaned_extensions: u64,

    /// Records with more than one real name.
    pub multi_linked_records: u64,
    /// Extra entries emitted for those names. Each is a real path; none of
    /// them carries the file's bytes a second time.
    pub hard_link_aliases: u64,

    /// Alternate data streams seen.
    pub ads_streams: u64,
    /// Disk bytes those streams occupy, already folded into their owners'
    /// allocated sizes.
    pub ads_bytes: u64,

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

    /// Fold a parse worker's counters in. I/O-side fields (`reads`,
    /// `bytes_read`, `records_read`, `elapsed`, `bitmap_in_use`, `cancelled`,
    /// `entries_emitted`) belong to the coordinating thread and are not
    /// merged.
    fn absorb(&mut self, other: &ScanStats) {
        self.records_in_use += other.records_in_use;
        self.files += other.files;
        self.directories += other.directories;
        self.never_used_slots += other.never_used_slots;
        self.bad_records += other.bad_records;
        self.failed_fixup += other.failed_fixup;
        self.unnamed_records += other.unnamed_records;
        self.dos_only_records += other.dos_only_records;
        self.extension_records += other.extension_records;
        self.deferred_bases += other.deferred_bases;
        self.multi_linked_records += other.multi_linked_records;
        self.hard_link_aliases += other.hard_link_aliases;
        self.ads_streams += other.ads_streams;
        self.ads_bytes += other.ads_bytes;
    }
}

/// One filled buffer on its way from the reader thread to the parser.
struct Chunk {
    buf: AlignedBuf,
    first_record: u64,
    records: u64,
    span: usize,
}

/// Read the whole MFT into `sink`.
pub fn sweep(
    source: &dyn BlockSource,
    layout: &MftLayout,
    sink: &mut dyn EntrySink,
    progress: &mut dyn FnMut(Progress),
    cancel: &CancellationToken,
    options: ScanOptions,
) -> Result<SweepOutcome> {
    let started = Instant::now();
    let mut stats = ScanStats::default();
    let mut ads: Vec<AdsStream> = Vec::new();

    let record_size = u64::from(layout.boot.bytes_per_record);
    let bytes_per_sector = layout.boot.bytes_per_sector;
    let bytes_per_cluster = layout.boot.bytes_per_cluster;
    let total_records = layout.records_to_scan().min(layout.capacity_records());

    // The bitmap turns "slots ever written" into "files actually here" - a
    // stats cross-check, not the progress denominator. Failing to read it
    // costs nothing but that check.
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

    // Progress tracks the read position through the WHOLE table, free slots
    // included. Counting files found instead freezes the bar early on any
    // volume that once held more files than it does now: the MFT never
    // shrinks, and the free tail (60% of the table on a churned drive) still
    // has to be read.
    progress(Progress::started("Reading the MFT", Some(total_records)));

    let records_per_chunk = (options.chunk_bytes as u64 / record_size).max(1);
    let chunk_bytes = (records_per_chunk * record_size) as usize;

    // Two buffers, two channels: the reader fills one while the parser drains
    // the other. `sync_channel(2)` means a send can never block, so the reader
    // only ever waits for a free buffer - dropping `free_tx` is what stops it.
    let (full_tx, full_rx) = mpsc::sync_channel::<Result<Chunk>>(2);
    let (free_tx, free_rx) = mpsc::channel::<AlignedBuf>();
    for _ in 0..2 {
        let _ = free_tx.send(AlignedBuf::for_source(source, chunk_bytes));
    }

    let mut held = Held::default();
    let mut read_error: Option<Error> = None;
    let mut last_tick = Instant::now();

    std::thread::scope(|s| {
        let extents = &layout.extents;
        s.spawn(move || {
            let mut record = 0u64;
            while record < total_records {
                if cancel.is_cancelled() {
                    break;
                }
                let Some(location) = extents.locate(record) else {
                    break; // past the mapped extents
                };

                // Never read past the end of the fragment this record sits in.
                // Without this the tail of a chunk is whatever happens to lie
                // next on disk.
                let to_read = location
                    .contiguous_records
                    .min(records_per_chunk)
                    .min(total_records - record);
                if to_read == 0 {
                    // The record straddles a fragment boundary - only possible
                    // when records are larger than clusters. Skip rather than
                    // stall.
                    record += 1;
                    continue;
                }

                // Blocks until the parser hands a buffer back; ends when the
                // parser is done with us and drops its end.
                let Ok(mut buf) = free_rx.recv() else {
                    break;
                };

                let span = (to_read * record_size) as usize;
                match source.read_exact_at(location.byte_offset, &mut buf.as_mut_slice()[..span]) {
                    Ok(()) => {
                        let chunk = Chunk {
                            buf,
                            first_record: record,
                            records: to_read,
                            span,
                        };
                        if full_tx.send(Ok(chunk)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = full_tx.send(Err(e));
                        break;
                    }
                }
                record += to_read;
            }
        });

        while let Ok(message) = full_rx.recv() {
            let mut chunk = match message {
                Ok(chunk) => chunk,
                Err(e) => {
                    read_error = Some(e);
                    break;
                }
            };

            stats.reads += 1;
            stats.bytes_read += chunk.span as u64;
            stats.records_read += chunk.records;

            let outputs = parse_chunk(
                &mut chunk.buf.as_mut_slice()[..chunk.span],
                chunk.first_record,
                record_size as usize,
                bytes_per_sector,
                bytes_per_cluster,
            );

            let mut broke = false;
            for mut out in outputs {
                stats.absorb(&out.stats);
                for warning in out.warnings.drain(..) {
                    sink.warn(warning);
                }
                for (id, base) in out.bases.drain(..) {
                    held.bases.insert(id, base);
                }
                for (id, extension) in out.extensions.drain(..) {
                    held.extensions.entry(id).or_default().push(extension);
                }
                ads.append(&mut out.ads);
                if flush(&mut out.batch, &mut stats, sink) == ControlFlow::Break(()) {
                    broke = true;
                    break;
                }
            }
            if broke {
                stats.cancelled = true;
                break;
            }

            // Hand the buffer back for the next read.
            let _ = free_tx.send(chunk.buf);

            if last_tick.elapsed() >= options.progress_interval {
                progress(Progress::Tick {
                    done: stats.records_read,
                });
                last_tick = Instant::now();
            }
        }

        // Unblock the reader (it may be waiting on a free buffer) and let the
        // scope join it.
        drop(free_tx);
        drop(full_rx);
    });

    if let Some(e) = read_error {
        return Err(e);
    }
    if cancel.is_cancelled() {
        stats.cancelled = true;
    }

    // Everything the sweep held back, now that both halves are in hand.
    if !stats.cancelled {
        progress(Progress::started(
            "Resolving extension records",
            Some(held.bases.len() as u64),
        ));
        let mut batch = Batch::default();
        resolve_held(&mut held, &mut batch, &mut stats, sink);
    }

    stats.elapsed = started.elapsed();
    progress(Progress::Finished);

    tracing::info!(
        records = stats.records_read,
        in_use = stats.records_in_use,
        emitted = stats.entries_emitted,
        extensions = stats.extension_records,
        resolved = stats.resolved_from_extensions,
        ads = stats.ads_streams,
        reads = stats.reads,
        elapsed_ms = stats.elapsed.as_millis(),
        "MFT sweep complete"
    );

    Ok(SweepOutcome { stats, ads })
}

// ---------------------------------------------------------------------------
// targeted reads
// ---------------------------------------------------------------------------

/// Most bytes one targeted read covers. Records are 1 KiB, so a run of 64
/// neighbours costs the same single seek as one of them - and dirty sets
/// cluster, because a build output directory occupies consecutive records.
const MAX_TARGET_SPAN: usize = 64 * 1024;

/// How many rounds of "read what the last round pointed at" to run before
/// giving up. Two is enough for every real record (a base names its extension
/// records, an extension names its base); the rest is slack.
const MAX_TARGET_ROUNDS: usize = 4;

/// What one targeted read produced besides what went into the sink.
#[derive(Debug, Default)]
pub struct TargetedOutcome {
    pub stats: ScanStats,
    pub ads: Vec<AdsStream>,
    /// Records whose `$ATTRIBUTE_LIST` is non-resident, so where their names
    /// and data live cannot be learned without reading it. A sweep finds those
    /// parts anyway because it reads everything; a targeted read cannot, so the
    /// caller must fall back rather than emit a record with missing parts.
    pub opaque: Vec<u64>,
    /// Every record the read visited, sorted - the ones asked for plus the
    /// ones their attribute lists led to. A caller replacing nodes has to
    /// replace all of these, not just what it asked for, or a record whose
    /// entries were re-emitted ends up in the index twice.
    pub read: Vec<u64>,
}

/// Where [`read_records`] gets its bytes.
///
/// The choice is not about speed, it is about *when*. A sweep reads the volume
/// because that is one sequential pass over a table it wants all of. Replay
/// asks about records that changed seconds ago, and only the filesystem's own
/// cached copy reflects that yet - see [`LiveRecords`].
pub enum RecordSource<'a> {
    /// Straight off the volume, the way a sweep reads. What a disk image and
    /// an unmounted volume have.
    Blocks(&'a dyn BlockSource),
    /// Through the filesystem driver, which serves the copy it is about to
    /// write down rather than the one already written.
    Driver(&'a mut LiveRecords),
}

/// Parse a chosen set of MFT records instead of sweeping the whole table.
///
/// This is what makes journal replay cheap: the journal says which records
/// changed, and only those - plus whatever their attribute lists point at - are
/// read. Everything downstream is identical to a sweep: the same [`parse_one`],
/// the same held-base resolution, the same entries at the sink. A replayed
/// index and a rescanned one therefore agree by construction rather than by two
/// implementations happening to match. See `caching.md` sec 6.4.
///
/// `records` need not be sorted or unique. Records that are free, unnamed, or
/// past the end of the MFT simply produce no entries; the caller learns which
/// by diffing what it asked for against what arrived.
pub fn read_records(
    source: &mut RecordSource<'_>,
    layout: &MftLayout,
    records: &[u64],
    sink: &mut dyn EntrySink,
    cancel: &CancellationToken,
) -> Result<TargetedOutcome> {
    let started = Instant::now();
    let record_size = layout.boot.bytes_per_record as usize;
    let capacity = layout.capacity_records();

    let mut queue: Vec<u64> = records.iter().copied().filter(|&r| r < capacity).collect();
    queue.sort_unstable();
    queue.dedup();

    let mut seen: HashSet<u64> = queue.iter().copied().collect();
    let mut outcome = TargetedOutcome::default();
    let mut held = Held::default();

    for _ in 0..MAX_TARGET_ROUNDS {
        if queue.is_empty() || cancel.is_cancelled() {
            break;
        }
        let mut next: Vec<u64> = Vec::new();
        let outputs = match source {
            RecordSource::Blocks(blocks) => {
                let mut outputs = Vec::new();
                for run in coalesce(&queue, layout, record_size) {
                    if cancel.is_cancelled() {
                        break;
                    }
                    let mut out = WorkerOut::default();
                    read_run(*blocks, layout, &run, record_size, &mut out, &mut outcome)?;
                    outputs.push(out);
                }
                outputs
            }
            RecordSource::Driver(live) => {
                // One call per record: there is no seek to amortize, because
                // the driver is answering out of memory.
                let mut out = WorkerOut::default();
                for &record in &queue {
                    if cancel.is_cancelled() {
                        break;
                    }
                    outcome.stats.records_read += 1;
                    outcome.stats.reads += 1;
                    // `None` is a free record, which is how a deletion looks.
                    let form = live.form();
                    if let Some(bytes) = live.read(record)? {
                        outcome.stats.bytes_read += bytes.len() as u64;
                        parse_one(
                            record,
                            bytes,
                            layout.boot.bytes_per_sector,
                            layout.boot.bytes_per_cluster,
                            form,
                            &mut out,
                        );
                    }
                }
                vec![out]
            }
        };

        for mut out in outputs {
            outcome.stats.absorb(&out.stats);
            for warning in out.warnings.drain(..) {
                sink.warn(warning);
            }
            for (id, base) in out.bases.drain(..) {
                if base.opaque_list {
                    outcome.opaque.push(id);
                }
                for &record in &base.extension_records {
                    if record < capacity && seen.insert(record) {
                        next.push(record);
                    }
                }
                held.bases.insert(id, base);
            }
            for (id, extension) in out.extensions.drain(..) {
                // An extension record was touched but its base was not. The
                // base holds the name and the size, so it has to be read too.
                if id < capacity && seen.insert(id) {
                    next.push(id);
                }
                held.extensions.entry(id).or_default().push(extension);
            }
            outcome.ads.append(&mut out.ads);
            if flush(&mut out.batch, &mut outcome.stats, sink) == ControlFlow::Break(()) {
                outcome.stats.cancelled = true;
                return Ok(outcome);
            }
        }

        next.sort_unstable();
        queue = next;
    }

    outcome.read = seen.into_iter().collect();
    outcome.read.sort_unstable();

    if !queue.is_empty() {
        tracing::warn!(
            pending = queue.len(),
            "targeted read did not settle; some extension records were left unread"
        );
    }

    if cancel.is_cancelled() {
        outcome.stats.cancelled = true;
    } else {
        let mut batch = Batch::default();
        resolve_held(&mut held, &mut batch, &mut outcome.stats, sink);
    }

    outcome.stats.elapsed = started.elapsed();
    tracing::debug!(
        asked = records.len(),
        read = outcome.stats.records_read,
        reads = outcome.stats.reads,
        emitted = outcome.stats.entries_emitted,
        opaque = outcome.opaque.len(),
        elapsed_ms = outcome.stats.elapsed.as_millis(),
        "targeted MFT read complete"
    );
    Ok(outcome)
}

/// One read's worth of records: consecutive on disk, though not necessarily
/// consecutive in the request.
struct Run {
    first: u64,
    /// How many record slots the read spans, wanted or not.
    slots: u64,
    /// Slots left in the fragment `first` sits in - the run cannot outgrow it.
    available: u64,
    byte_offset: u64,
    /// Which of those slots were actually asked for, as offsets from `first`.
    wanted: Vec<u64>,
}

/// Group sorted record numbers into runs one read can cover.
///
/// A run never crosses an extent boundary (the locator says how many records
/// remain in the fragment) and never exceeds [`MAX_TARGET_SPAN`], so reading
/// over a gap between two wanted records is bounded waste that buys one seek.
fn coalesce(records: &[u64], layout: &MftLayout, record_size: usize) -> Vec<Run> {
    let max_slots = (MAX_TARGET_SPAN / record_size).max(1) as u64;
    let mut runs: Vec<Run> = Vec::new();

    for &record in records {
        if let Some(run) = runs.last_mut() {
            let offset = record - run.first;
            if offset < max_slots && offset < run.available {
                run.slots = offset + 1;
                run.wanted.push(offset);
                continue;
            }
        }
        let Some(location) = layout.extents.locate(record) else {
            continue; // past the mapped extents
        };
        runs.push(Run {
            first: record,
            slots: 1,
            available: location.contiguous_records,
            byte_offset: location.byte_offset,
            wanted: vec![0],
        });
    }
    runs
}

/// Read one run and parse the records that were asked for.
fn read_run(
    source: &dyn BlockSource,
    layout: &MftLayout,
    run: &Run,
    record_size: usize,
    out: &mut WorkerOut,
    outcome: &mut TargetedOutcome,
) -> Result<()> {
    // A record offset is record-aligned, not necessarily sector-aligned (1 KiB
    // records on a 4Kn disk), so read the surrounding aligned window. The
    // fragment starts on a cluster and is a whole number of clusters long, so
    // rounding outwards stays inside it.
    let sector = u64::from(source.sector_size()).max(1);
    let start = run.byte_offset / sector * sector;
    let lead = (run.byte_offset - start) as usize;
    let body = run.slots as usize * record_size;
    let span = (lead + body).div_ceil(sector as usize) * sector as usize;

    let mut buf = AlignedBuf::for_source(source, span);
    source.read_exact_at(start, buf.as_mut_slice())?;

    outcome.stats.reads += 1;
    outcome.stats.bytes_read += span as u64;
    outcome.stats.records_read += run.wanted.len() as u64;

    let bytes = buf.as_mut_slice();
    for &offset in &run.wanted {
        let at = lead + offset as usize * record_size;
        parse_one(
            run.first + offset,
            &mut bytes[at..at + record_size],
            layout.boot.bytes_per_sector,
            layout.boot.bytes_per_cluster,
            RecordForm::OnDisk,
            out,
        );
    }
    Ok(())
}

/// Fan one chunk's records out across rayon workers.
///
/// Every worker folds into its own [`WorkerOut`], so the parse takes no locks;
/// `collect` preserves split order, which keeps the emission order - and
/// therefore the whole scan - deterministic.
fn parse_chunk(
    chunk: &mut [u8],
    first_record: u64,
    record_size: usize,
    bytes_per_sector: u32,
    bytes_per_cluster: u32,
) -> Vec<WorkerOut> {
    chunk
        .par_chunks_exact_mut(record_size)
        .enumerate()
        .fold(WorkerOut::default, |mut out, (index, record_bytes)| {
            parse_one(
                first_record + index as u64,
                record_bytes,
                bytes_per_sector,
                bytes_per_cluster,
                RecordForm::OnDisk,
                &mut out,
            );
            out
        })
        .collect()
}

// ---------------------------------------------------------------------------
// batching
// ---------------------------------------------------------------------------

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

/// One `$FILE_NAME` found in the record being parsed, as a slice of the arena.
struct NameSlot {
    start: usize,
    end: usize,
    namespace: Namespace,
    parent: u64,
}

/// Scratch space reused across every record a worker parses, so a scan of a
/// million records makes a handful of allocations rather than a million.
struct Batch {
    /// Every name in this batch, concatenated.
    names: String,
    /// One per entry, indexing into `names`.
    pending: Vec<Pending>,
    /// Where a name is decoded before it is kept.
    scratch: String,
    /// Names belonging to the record currently being parsed. Cleared per
    /// record; almost always holds exactly one.
    slots: Vec<NameSlot>,
}

impl Default for Batch {
    fn default() -> Self {
        Self {
            names: String::with_capacity(16 * 1024),
            pending: Vec::with_capacity(1024),
            scratch: String::with_capacity(256),
            slots: Vec::with_capacity(8),
        }
    }
}

impl Batch {
    fn clear(&mut self) {
        self.names.clear();
        self.pending.clear();
        self.slots.clear();
    }

    fn push(&mut self, fields: &Fields, fs_id: u64, name: &str, parent: u64, flags: EntryFlags) {
        let start = self.names.len();
        self.names.push_str(name);
        self.pending.push(Pending {
            fs_id,
            parent_id: parent,
            name_start: start,
            name_end: self.names.len(),
            size: fields.size,
            // Only the owning entry carries the bytes; see EntryFlags::ALIAS.
            allocated: if flags.is_alias() {
                0
            } else {
                fields.allocated
            },
            times: fields.times,
            flags,
        });
    }
}

/// Everything one parse worker accumulates. Merged on the coordinating thread
/// after the chunk is done, so workers share nothing.
#[derive(Default)]
struct WorkerOut {
    batch: Batch,
    bases: Vec<(u64, HeldBase)>,
    extensions: Vec<(u64, HeldExtension)>,
    ads: Vec<AdsStream>,
    stats: ScanStats,
    warnings: Vec<ScanWarning>,
}

/// The id an extra hard-link name is emitted under.
///
/// The index builder keys entries by their filesystem id, so several names for
/// one file need distinct ids or they displace each other. Record numbers use
/// the low 48 bits, leaving the top free for a counter - and the builder masks
/// those bits off again when reporting the native id, so every link still
/// reports the one record it describes.
///
/// The counter is `index + 1`, never `index`: the owning name is emitted under
/// the bare record number, and [`owning_slot`] can pick any slot, so a zero
/// counter would mint the owner's id a second time and the two would fight
/// over one slot in the builder's id map.
fn alias_id(record: u64, index: usize) -> u64 {
    // 16 bits of counter. No real record carries anywhere near that many
    // names; a corrupt one that claims to is clamped rather than wrapped into
    // another record's id space.
    let counter = (index as u64 + 1).min(0xFFFF);
    (record & 0x0000_FFFF_FFFF_FFFF) | (counter << 48)
}

/// Hand the batch to the sink. The borrowed view is built only now that the
/// arena has stopped moving.
fn flush(batch: &mut Batch, stats: &mut ScanStats, sink: &mut dyn EntrySink) -> ControlFlow<()> {
    if batch.pending.is_empty() {
        return ControlFlow::Continue(());
    }

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
    sink.push_batch(&entries)
}

// ---------------------------------------------------------------------------
// deferral
// ---------------------------------------------------------------------------

/// A name and where it lives, owned because it has to outlive the read buffer.
#[derive(Debug, Clone)]
struct NameCandidate {
    namespace: Namespace,
    name: String,
    parent: u64,
}

/// The parts of a record its attributes yielded.
#[derive(Debug, Clone, Default)]
struct Fields {
    size: u64,
    allocated: u64,
    /// Whether an unnamed `$DATA` at VCN 0 was found here. Without it the size
    /// is not yet known and an extension record may supply it.
    has_data: bool,
    times: Times,
    flags: EntryFlags,
    is_directory: bool,
}

/// A base record whose emission waits on its extension records.
struct HeldBase {
    fields: Fields,
    names: Vec<NameCandidate>,
    /// Records its `$ATTRIBUTE_LIST` sends names or data to. A sweep does not
    /// need these - it reads every record anyway and matches on
    /// `base_record()` - but a targeted read ([`read_records`]) has no other
    /// way to find them.
    extension_records: Vec<u64>,
    /// The attribute list is non-resident, so where the parts live cannot be
    /// known without reading it. Only a full sweep can complete this record.
    opaque_list: bool,
}

/// Attributes harvested from one extension record.
struct HeldExtension {
    names: Vec<NameCandidate>,
    /// `(data_size, physical_size)` from an unnamed `$DATA` at VCN 0.
    size: Option<(u64, u64)>,
    /// Disk bytes of alternate data streams held in this extension record,
    /// owed to the base record's allocated size.
    ads_allocated: u64,
}

/// Both halves of every split file, collected as the sweep passes them.
///
/// Keyed by base record number, so it does not matter whether a base or its
/// extensions are read first - which is what makes this work in one sequential
/// pass with no seeking.
#[derive(Default)]
struct Held {
    bases: HashMap<u64, HeldBase>,
    extensions: HashMap<u64, Vec<HeldExtension>>,
}

/// Join the two halves and emit what was held back.
fn resolve_held(
    held: &mut Held,
    batch: &mut Batch,
    stats: &mut ScanStats,
    sink: &mut dyn EntrySink,
) {
    batch.clear();

    let bases = std::mem::take(&mut held.bases);
    for (fs_id, mut base) in bases {
        let mut gained = false;

        if let Some(extensions) = held.extensions.remove(&fs_id) {
            for extension in extensions {
                // Names in extension records are additional links, not
                // replacements - a file whose attributes overflowed is exactly
                // the kind that has many of them.
                if !extension.names.is_empty() {
                    base.names.extend(extension.names);
                    gained = true;
                }
                // The size is taken only if the base did not carry it: the base
                // holds the authoritative $DATA when it holds any at all.
                if !base.fields.has_data
                    && let Some((size, allocated)) = extension.size
                {
                    base.fields.size = size;
                    base.fields.allocated = allocated;
                    base.fields.has_data = true;
                    gained = true;
                }
                // ADS bytes are real disk cost wherever the stream's attribute
                // happened to land.
                base.fields.allocated += extension.ads_allocated;
            }
        }

        if gained {
            stats.resolved_from_extensions += 1;
        }

        if base.names.is_empty() {
            stats.unnamed_records += 1;
        } else {
            let owner = owning_name(&base.names);
            for (i, candidate) in base.names.iter().enumerate() {
                let (id, flags) = if i == owner {
                    (fs_id, base.fields.flags)
                } else {
                    stats.hard_link_aliases += 1;
                    (
                        alias_id(fs_id, i),
                        base.fields.flags.union(EntryFlags::ALIAS),
                    )
                };
                batch.push(&base.fields, id, &candidate.name, candidate.parent, flags);
            }
            if base.fields.is_directory {
                stats.directories += 1;
            } else {
                stats.files += 1;
            }
        }

        if batch.pending.len() >= RESOLVE_BATCH {
            if flush(batch, stats, sink) == ControlFlow::Break(()) {
                stats.cancelled = true;
                return;
            }
            batch.clear();
        }
    }

    if flush(batch, stats, sink) == ControlFlow::Break(()) {
        stats.cancelled = true;
        return;
    }
    batch.clear();

    // Extension records whose base never turned up. Their attributes are lost;
    // the file is either deleted or was already emitted without them.
    stats.orphaned_extensions += held
        .extensions
        .values()
        .map(|v| v.len() as u64)
        .sum::<u64>();
    held.extensions.clear();
}

// ---------------------------------------------------------------------------
// per-record parsing
// ---------------------------------------------------------------------------

/// What a non-resident stream really costs on disk.
///
/// Compressed streams carry the true figure in their header. For everything
/// else the run list is checked for holes: a holed stream reports the full
/// *reserved* span as its allocated size, and `$BadClus:$Bad` - the whole
/// volume as one hole - does so **without** setting the sparse flag, so the
/// flag cannot be trusted. When the runs show no holes the header's figure
/// is used, because a multi-fragment stream's VCN-0 header covers the whole
/// stream while its runs cover only this fragment.
fn stream_physical(nr: &NonResident<'_>, bytes_per_cluster: u32) -> u64 {
    if let Some(compressed) = nr.compressed_size() {
        return compressed;
    }
    let summary = nr.run_summary();
    if summary.has_holes() {
        summary
            .allocated_clusters
            .saturating_mul(u64::from(bytes_per_cluster))
    } else {
        nr.allocated_size()
    }
}

/// Parse one record into a worker's scratch: emit it, or hold it back for
/// [`resolve_held`].
fn parse_one(
    number: u64,
    record_bytes: &mut [u8],
    bytes_per_sector: u32,
    bytes_per_cluster: u32,
    form: RecordForm,
    out: &mut WorkerOut,
) {
    let stats = &mut out.stats;

    // Cheapest check first: a free record needs no repair and no walking, and a
    // large share of any used volume's table is free.
    let Ok(header) = RecordHeader::parse(record_bytes) else {
        if record_bytes[..4] == [0, 0, 0, 0] {
            stats.never_used_slots += 1;
        } else {
            stats.bad_records += 1;
        }
        return;
    };
    if !header.is_in_use() {
        return;
    }
    stats.records_in_use += 1;

    let base = header.base_record();

    let record = match Record::parse_as(record_bytes, bytes_per_sector, form) {
        Ok(record) => record,
        Err(e) => {
            stats.failed_fixup += 1;
            out.warnings.push(ScanWarning::BadRecord {
                fs_id: number,
                reason: e.to_string(),
            });
            return;
        }
    };

    let mut fields = Fields {
        is_directory: record.is_directory(),
        flags: if record.is_directory() {
            EntryFlags::DIRECTORY
        } else {
            EntryFlags::empty()
        },
        ..Fields::default()
    };
    let mut saw_dos_name = false;
    // Records an $ATTRIBUTE_LIST sends a name or the data to. Non-empty means
    // this record is split and must be held back.
    let mut elsewhere: Vec<u64> = Vec::new();
    // Set when the list itself is non-resident: split, but where to is unknown.
    let mut opaque_list = false;
    // Disk bytes of this record's alternate data streams.
    let mut ads_allocated = 0u64;

    let batch = &mut out.batch;

    // Names go straight into the arena as they are found. If the record turns
    // out to need holding back, the arena is rewound to here and the names are
    // copied into owned strings instead.
    let arena_mark = batch.names.len();
    batch.slots.clear();

    for attribute in record.attributes() {
        match attribute.kind() {
            AttributeType::StandardInformation => {
                if let Some(value) = attribute.resident_value()
                    && let Some(si) = StandardInfo::parse(value)
                {
                    fields.times = Times {
                        mtime: si.modified,
                        crtime: si.created,
                    };
                    fields.flags = fields.flags.union(dos_attributes_to_flags(si.attributes));
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

                fname.decode_name_into(&mut batch.scratch);
                // The root names itself "." - NTFS-internal spelling, not a
                // display name. It becomes the empty name the sink contract
                // reserves for the root.
                let is_root_self_name = fname.parent_record() == number && batch.scratch == ".";
                if is_root_self_name {
                    batch.scratch.clear();
                } else if batch.scratch.is_empty() {
                    continue;
                }
                // Every real name is somewhere the file genuinely appears, so
                // all of them are kept. Which one owns the bytes is decided
                // once they have all been seen.
                let start = batch.names.len();
                batch.names.push_str(&batch.scratch);
                batch.slots.push(NameSlot {
                    start,
                    end: batch.names.len(),
                    namespace,
                    parent: fname.parent_record(),
                });
            }
            // The unnamed $DATA is the file's contents; a named one is an
            // alternate stream, whose bytes occupy disk but are not the file's
            // logical size.
            AttributeType::Data => {
                if attribute.is_unnamed() {
                    if let Some(nr) = attribute.non_resident() {
                        // Only the fragment at VCN 0 carries the real sizes;
                        // later fragments repeat stale values.
                        if nr.starting_vcn() == 0 {
                            fields.size = nr.data_size();
                            // What it costs, not the virtual range it spans -
                            // those differ for every compressed and sparse
                            // file.
                            fields.allocated = stream_physical(&nr, bytes_per_cluster);
                            fields.has_data = true;
                        }
                    } else if let Some(value) = attribute.resident_value() {
                        // Resident: the contents live in the record, so the
                        // file occupies no clusters at all.
                        fields.size = value.len() as u64;
                        fields.allocated = 0;
                        fields.has_data = true;
                    }
                } else {
                    // An alternate data stream.
                    let (size, allocated) = match attribute.non_resident() {
                        Some(nr) if nr.starting_vcn() == 0 => {
                            (nr.data_size(), stream_physical(&nr, bytes_per_cluster))
                        }
                        Some(_) => continue, // later fragment of a stream already seen
                        None => match attribute.resident_value() {
                            Some(value) => (value.len() as u64, 0),
                            None => continue,
                        },
                    };
                    ads_allocated += allocated;
                    stats.ads_streams += 1;
                    stats.ads_bytes += allocated;
                    out.ads.push(AdsStream {
                        owner: base.unwrap_or(number),
                        name: decode_utf16(attribute.name_bytes()),
                        size,
                        allocated,
                    });
                }
            }
            AttributeType::AttributeList => {
                if let Some(value) = attribute.resident_value() {
                    elsewhere = elsewhere_records(value, number);
                }
                // A non-resident attribute list is vanishingly rare and would
                // need its own read; treat the record as split so it is held
                // back rather than emitted with missing parts.
                else {
                    opaque_list = true;
                }
            }
            _ => {}
        }
    }

    if batch.slots.len() > 1 {
        stats.multi_linked_records += 1;
    }

    // An extension record's attributes belong to another file, and a record
    // held back needs its names to outlive the read buffer. Both take owned
    // copies and rewind the arena.
    let split = opaque_list || !elsewhere.is_empty();
    if base.is_some() || split {
        let names: Vec<NameCandidate> = batch
            .slots
            .iter()
            .map(|slot| NameCandidate {
                namespace: slot.namespace,
                name: batch.names[slot.start..slot.end].to_string(),
                parent: slot.parent,
            })
            .collect();
        batch.names.truncate(arena_mark);

        if let Some(base_number) = base {
            stats.extension_records += 1;
            let size = fields.has_data.then_some((fields.size, fields.allocated));
            if !names.is_empty() || size.is_some() || ads_allocated > 0 {
                out.extensions.push((
                    base_number,
                    HeldExtension {
                        names,
                        size,
                        ads_allocated,
                    },
                ));
            }
        } else {
            stats.deferred_bases += 1;
            fields.allocated += ads_allocated;
            out.bases.push((
                number,
                HeldBase {
                    fields,
                    names,
                    extension_records: elsewhere,
                    opaque_list,
                },
            ));
        }
        return;
    }

    fields.allocated += ads_allocated;

    if batch.slots.is_empty() {
        // In use, complete, and yet nothing usable to call it.
        stats.unnamed_records += 1;
        if saw_dos_name {
            stats.dos_only_records += 1;
        }
        return;
    }

    // One entry per name. Exactly one carries the bytes.
    let owner = owning_slot(&batch.slots);
    for i in 0..batch.slots.len() {
        let slot = &batch.slots[i];
        let (start, end, parent) = (slot.start, slot.end, slot.parent);
        let (id, flags) = if i == owner {
            (number, fields.flags)
        } else {
            stats.hard_link_aliases += 1;
            (alias_id(number, i), fields.flags.union(EntryFlags::ALIAS))
        };
        batch.pending.push(Pending {
            fs_id: id,
            parent_id: parent,
            name_start: start,
            name_end: end,
            size: fields.size,
            allocated: if flags.is_alias() {
                0
            } else {
                fields.allocated
            },
            times: fields.times,
            flags,
        });
    }

    if fields.is_directory {
        stats.directories += 1;
    } else {
        stats.files += 1;
    }
}

/// Which of a record's names should carry its bytes.
///
/// The best namespace wins, first-seen breaking ties, so the choice is
/// deterministic and the owner is the name a user is most likely to recognize.
/// Any single consistent choice would do - what matters is that there is
/// exactly one.
fn owning_slot(slots: &[NameSlot]) -> usize {
    let mut best = 0;
    for (i, slot) in slots.iter().enumerate().skip(1) {
        if is_better(slot.namespace, Some(slots[best].namespace)) {
            best = i;
        }
    }
    best
}

/// As [`owning_slot`], for names already lifted out of the arena.
fn owning_name(names: &[NameCandidate]) -> usize {
    let mut best = 0;
    for (i, candidate) in names.iter().enumerate().skip(1) {
        if is_better(candidate.namespace, Some(names[best].namespace)) {
            best = i;
        }
    }
    best
}

/// Whether an `$ATTRIBUTE_LIST` sends a name or the file's data to another
/// record.
///
/// An attribute list can be entirely self-referential - listing attributes that
/// never actually moved - in which case the record is complete as read and
/// holding it back would be waste.
fn elsewhere_records(value: &[u8], own_record: u64) -> Vec<u64> {
    let mut out: Vec<u64> = attr::parse_attribute_list(value)
        .iter()
        .filter(|entry| matches!(entry.type_code, 0x30 | 0x80))
        .map(|entry| entry.record_number)
        .filter(|&record| record != own_record)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
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

/// Decode UTF-16LE bytes, replacing lone surrogates. Used for the rare named
/// stream; file names go through the allocation-free
/// [`FileName::decode_name_into`].
fn decode_utf16(bytes: &[u8]) -> String {
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
    char::decode_utf16(units)
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Map the DOS attribute bits onto the index's flags.
fn dos_attributes_to_flags(attributes: u32) -> EntryFlags {
    const HIDDEN: u32 = 0x0000_0002;
    const SYSTEM: u32 = 0x0000_0004;
    const SPARSE: u32 = 0x0000_0200;
    const REPARSE: u32 = 0x0000_0400;
    const COMPRESSED: u32 = 0x0000_0800;

    let mut flags = EntryFlags::empty();
    if attributes & HIDDEN != 0 {
        flags = flags.union(EntryFlags::HIDDEN);
    }
    if attributes & SYSTEM != 0 {
        flags = flags.union(EntryFlags::SYSTEM);
    }
    if attributes & SPARSE != 0 {
        flags = flags.union(EntryFlags::SPARSE);
    }
    if attributes & REPARSE != 0 {
        flags = flags.union(EntryFlags::REPARSE);
    }
    if attributes & COMPRESSED != 0 {
        flags = flags.union(EntryFlags::COMPRESSED);
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

        assert!(dos_attributes_to_flags(0x0400).contains(EntryFlags::REPARSE));
        assert!(dos_attributes_to_flags(0x0200).contains(EntryFlags::SPARSE));
        assert!(dos_attributes_to_flags(0x0800).contains(EntryFlags::COMPRESSED));
        assert_eq!(dos_attributes_to_flags(0), EntryFlags::empty());
    }

    /// One `$ATTRIBUTE_LIST` entry.
    fn list_entry(type_code: u32, record: u64) -> Vec<u8> {
        let mut entry = vec![0u8; 32];
        entry[0x00..0x04].copy_from_slice(&type_code.to_le_bytes());
        entry[0x04..0x06].copy_from_slice(&32u16.to_le_bytes());
        // With a sequence number in the top bits, as on disk.
        entry[0x10..0x18].copy_from_slice(&(record | (2u64 << 48)).to_le_bytes());
        entry
    }

    #[test]
    fn a_self_contained_attribute_list_needs_no_deferral() {
        // Every attribute is listed as living in the record itself, so nothing
        // is missing and holding it back would be pure waste.
        let value: Vec<u8> = [
            list_entry(0x10, 100),
            list_entry(0x30, 100),
            list_entry(0x80, 100),
        ]
        .concat();
        assert!(elsewhere_records(&value, 100).is_empty());
    }

    #[test]
    fn a_list_pointing_at_another_record_forces_deferral() {
        let value: Vec<u8> = [list_entry(0x10, 100), list_entry(0x80, 350)].concat();
        assert_eq!(elsewhere_records(&value, 100), vec![350]);

        let names_elsewhere: Vec<u8> = [list_entry(0x30, 900)].concat();
        assert_eq!(elsewhere_records(&names_elsewhere, 100), vec![900]);
    }

    #[test]
    fn elsewhere_records_are_sorted_and_deduplicated() {
        // A targeted read turns this list into its next round of reads, so
        // one record named by three attributes must not be read three times.
        let value: Vec<u8> = [
            list_entry(0x80, 350),
            list_entry(0x80, 200),
            list_entry(0x30, 350),
        ]
        .concat();
        assert_eq!(elsewhere_records(&value, 100), vec![200, 350]);
    }

    #[test]
    fn only_names_and_data_matter_for_deferral() {
        // A security descriptor or index allocation living elsewhere changes
        // nothing EmFit reads, so it must not trigger a deferral.
        let value: Vec<u8> = [list_entry(0x50, 400), list_entry(0xA0, 400)].concat();
        assert!(elsewhere_records(&value, 100).is_empty());
    }

    fn slot(namespace: Namespace) -> NameSlot {
        NameSlot {
            start: 0,
            end: 0,
            namespace,
            parent: 5,
        }
    }

    #[test]
    fn the_best_namespace_owns_the_bytes() {
        // A file named both POSIX-style and Win32-style: the Win32 name is the
        // one a user recognizes, so it carries the size.
        let slots = [slot(Namespace::Posix), slot(Namespace::Win32)];
        assert_eq!(owning_slot(&slots), 1);

        let slots = [slot(Namespace::Win32), slot(Namespace::Posix)];
        assert_eq!(owning_slot(&slots), 0);
    }

    #[test]
    fn ties_go_to_the_first_name_seen() {
        // Ordinary hard links all share a namespace, so this is the common
        // case: any consistent choice will do, as long as it is exactly one.
        let slots = [
            slot(Namespace::Win32),
            slot(Namespace::Win32),
            slot(Namespace::Win32),
        ];
        assert_eq!(owning_slot(&slots), 0);
    }

    #[test]
    fn a_single_name_owns_its_own_bytes() {
        assert_eq!(owning_slot(&[slot(Namespace::Win32)]), 0);
    }

    #[test]
    fn alias_ids_are_distinct_but_keep_the_record_number() {
        // Extra names need their own key in the builder's map, yet must still
        // point back at the one record they all describe.
        let record = 123_456u64;
        let ids = [record, alias_id(record, 0), alias_id(record, 1)];

        assert_ne!(
            ids[0], ids[1],
            "slot 0 is an alias whenever another slot owns the bytes"
        );
        assert_ne!(ids[1], ids[2]);
        for id in ids {
            assert_eq!(
                id & 0x0000_FFFF_FFFF_FFFF,
                record,
                "the builder masks this back to the real record"
            );
        }
    }

    #[test]
    fn alias_ids_do_not_collide_across_records() {
        // Two different files, each with extra names, must not land on the
        // same key - that would reattach one file's children to the other.
        assert_ne!(alias_id(100, 1), alias_id(101, 1));
        assert_ne!(alias_id(100, 1), alias_id(100, 2));
        assert_ne!(alias_id(100, 1), 101);

        // The owning entry keeps the bare record number, so no alias of that
        // record may ever equal it.
        for index in 0..8 {
            assert_ne!(alias_id(100, index), 100);
        }
    }

    #[test]
    fn utf16_decoding_survives_lone_surrogates() {
        // "A" then an unpaired high surrogate.
        let bytes = [0x41, 0x00, 0x00, 0xD8];
        assert_eq!(decode_utf16(&bytes), "A\u{FFFD}");
        assert_eq!(decode_utf16(&[]), "");
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

    #[test]
    fn worker_stats_fold_into_the_totals() {
        let mut main = ScanStats {
            records_read: 100, // I/O-side: must survive the merge untouched
            reads: 2,
            ..Default::default()
        };
        let worker = ScanStats {
            records_in_use: 10,
            files: 7,
            directories: 3,
            ads_streams: 1,
            ads_bytes: 4096,
            records_read: 999, // parse workers do not own this counter
            ..Default::default()
        };
        main.absorb(&worker);

        assert_eq!(main.records_in_use, 10);
        assert_eq!(main.files, 7);
        assert_eq!(main.directories, 3);
        assert_eq!(main.ads_streams, 1);
        assert_eq!(main.ads_bytes, 4096);
        assert_eq!(main.records_read, 100, "I/O counters are not merged");
        assert_eq!(main.reads, 2);
    }
}
