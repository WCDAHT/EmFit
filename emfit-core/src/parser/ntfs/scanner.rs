//! The sweep: read the whole MFT and push what it says at a sink.
//!
//! One pass, front to back, in chunks bounded by what the extent map says is
//! contiguous. Records are parsed **in place** off the read buffer — no
//! per-record allocation, no copying a kilobyte out only to throw it away.
//!
//! Names are the exception, because they arrive as UTF-16 and the index wants
//! UTF-8. They go into one arena per batch, reused across the whole scan, so
//! the cost is a few hundred allocations rather than a few million.
//!
//! # Extension records
//!
//! A file with more attributes than fit in 1024 bytes — many hard links, a
//! `$DATA` run list fragmented into hundreds of pieces — spills into
//! *extension records*. Those appear in the sweep like any other record and
//! carry a reference to the file they belong to.
//!
//! The naive fix is to seek to them on demand, which is what v1 did: two
//! random 1 KB reads per record, on a volume with tens of thousands of them.
//! Instead, both halves are collected as the sweep passes over them and joined
//! at the end — **no seeking at all**, since everything is read exactly once in
//! sequential order regardless. WizTree does the same, deferring only the
//! records whose base lies ahead of the read head; collecting both sides makes
//! the result independent of which came first.
//!
//! This matters more than the record count suggests. Attribute lists appear
//! precisely when a file's `$DATA` is heavily fragmented, which correlates with
//! it being **large** — so skipping extension records loses a few percent of
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

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use crate::error::Result;
use crate::model::entry::{EntryFlags, RawEntry, Times};
use crate::model::sink::{EntrySink, ScanWarning};
use crate::parser::block::{AlignedBuf, BlockSource};
use crate::parser::ntfs::attr::{self, AttributeType, FileName, Namespace, StandardInfo};
use crate::parser::ntfs::bitmap;
use crate::parser::ntfs::bootstrap::MftLayout;
use crate::parser::ntfs::record::{Record, RecordHeader};
use crate::service::task::{CancellationToken, Progress};

/// NTFS's root directory. Its `$FILE_NAME` names itself as its own parent,
/// which is exactly the convention the index builder recognizes.
pub const ROOT_RECORD: u64 = 5;

/// Records 0–15 are the filesystem's own metadata — `$MFT`, `$LogFile`,
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

    /// Slots whose first four bytes were zero — never written, not corrupt.
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
    /// Extension records whose base never appeared — a deleted base, or one
    /// whose attribute list we could not read. Their attributes are lost.
    pub orphaned_extensions: u64,

    /// Records with more than one real name.
    pub multi_linked_records: u64,
    /// Extra entries emitted for those names. Each is a real path; none of
    /// them carries the file's bytes a second time.
    pub hard_link_aliases: u64,

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
    // is what progress should count against. Failing to read it costs accuracy,
    // not correctness.
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
    let mut held = Held::default();

    let mut record = 0u64;
    let mut last_tick = Instant::now();

    while record < total_records {
        if cancel.is_cancelled() {
            stats.cancelled = true;
            break;
        }

        let Some(location) = layout.extents.locate(record) else {
            break; // past the mapped extents
        };

        // Never read past the end of the fragment this record sits in. Without
        // this the tail of a chunk is whatever happens to lie next on disk.
        let to_read = location
            .contiguous_records
            .min(records_per_chunk)
            .min(total_records - record);
        if to_read == 0 {
            // The record straddles a fragment boundary — only possible when
            // records are larger than clusters. Skip rather than stall.
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
                &mut held,
                &mut stats,
                sink,
            );
        }

        if flush(&mut batch, &mut stats, sink) == ControlFlow::Break(()) {
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

    // Everything the sweep held back, now that both halves are in hand.
    if !stats.cancelled {
        progress(Progress::started(
            "Resolving extension records",
            Some(held.bases.len() as u64),
        ));
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
        reads = stats.reads,
        elapsed_ms = stats.elapsed.as_millis(),
        "MFT sweep complete"
    );

    Ok(stats)
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

/// Scratch space reused across every batch, so a scan of a million records
/// makes a handful of allocations rather than a million.
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

impl Batch {
    fn with_capacity(records: usize) -> Self {
        Self {
            names: String::with_capacity(64 * 1024),
            pending: Vec::with_capacity(records),
            scratch: String::with_capacity(256),
            slots: Vec::with_capacity(8),
        }
    }

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

/// The id an extra hard-link name is emitted under.
///
/// The index builder keys entries by their filesystem id, so several names for
/// one file need distinct ids or they displace each other. Record numbers use
/// the low 48 bits, leaving the top free for a counter — and the builder masks
/// those bits off again when recording the native id, so every link still
/// reports the one record it describes.
fn alias_id(record: u64, index: usize) -> u64 {
    (record & 0x0000_FFFF_FFFF_FFFF) | ((index as u64) << 48)
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
}

/// Attributes harvested from one extension record.
struct HeldExtension {
    names: Vec<NameCandidate>,
    /// `(data_size, physical_size)` from an unnamed `$DATA` at VCN 0.
    size: Option<(u64, u64)>,
}

/// Both halves of every split file, collected as the sweep passes them.
///
/// Keyed by base record number, so it does not matter whether a base or its
/// extensions are read first — which is what makes this work in one sequential
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
                // replacements — a file whose attributes overflowed is exactly
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

/// Parse one record: emit it, or hold it back for [`resolve_held`].
fn parse_one(
    number: u64,
    record_bytes: &mut [u8],
    bytes_per_sector: u32,
    batch: &mut Batch,
    held: &mut Held,
    stats: &mut ScanStats,
    sink: &mut dyn EntrySink,
) {
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
    // True when an $ATTRIBUTE_LIST sends a name or the data elsewhere.
    let mut split = false;

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
                if batch.scratch.is_empty() {
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
            // alternate stream, whose bytes are not the file's size.
            AttributeType::Data if attribute.is_unnamed() => {
                if let Some(nr) = attribute.non_resident() {
                    // Only the fragment at VCN 0 carries the real sizes; later
                    // fragments repeat stale values.
                    if nr.starting_vcn() == 0 {
                        fields.size = nr.data_size();
                        // What it costs, not the virtual range it spans — those
                        // differ for every compressed and sparse file.
                        fields.allocated = nr.physical_size();
                        fields.has_data = true;
                    }
                } else if let Some(value) = attribute.resident_value() {
                    // Resident: the contents live in the record, so the file
                    // occupies no clusters at all.
                    fields.size = value.len() as u64;
                    fields.allocated = 0;
                    fields.has_data = true;
                }
            }
            AttributeType::AttributeList => {
                if let Some(value) = attribute.resident_value() {
                    split = references_elsewhere(value, number);
                }
                // A non-resident attribute list is vanishingly rare and would
                // need its own read; treat the record as split so it is held
                // back rather than emitted with missing parts.
                else {
                    split = true;
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
            if !names.is_empty() || size.is_some() {
                held.extensions
                    .entry(base_number)
                    .or_default()
                    .push(HeldExtension { names, size });
            }
        } else {
            stats.deferred_bases += 1;
            held.bases.insert(number, HeldBase { fields, names });
        }
        return;
    }

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
/// Any single consistent choice would do — what matters is that there is
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
/// An attribute list can be entirely self-referential — listing attributes that
/// never actually moved — in which case the record is complete as read and
/// holding it back would be waste.
fn references_elsewhere(value: &[u8], own_record: u64) -> bool {
    attr::parse_attribute_list(value)
        .iter()
        .any(|entry| matches!(entry.type_code, 0x30 | 0x80) && entry.record_number != own_record)
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
        assert!(!references_elsewhere(&value, 100));
    }

    #[test]
    fn a_list_pointing_at_another_record_forces_deferral() {
        let value: Vec<u8> = [list_entry(0x10, 100), list_entry(0x80, 350)].concat();
        assert!(references_elsewhere(&value, 100));

        let names_elsewhere: Vec<u8> = [list_entry(0x30, 900)].concat();
        assert!(references_elsewhere(&names_elsewhere, 100));
    }

    #[test]
    fn only_names_and_data_matter_for_deferral() {
        // A security descriptor or index allocation living elsewhere changes
        // nothing EmFit reads, so it must not trigger a deferral.
        let value: Vec<u8> = [list_entry(0x50, 400), list_entry(0xA0, 400)].concat();
        assert!(!references_elsewhere(&value, 100));
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
        let ids = [record, alias_id(record, 1), alias_id(record, 2)];

        assert_ne!(ids[0], ids[1]);
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
        // same key — that would reattach one file's children to the other.
        assert_ne!(alias_id(100, 1), alias_id(101, 1));
        assert_ne!(alias_id(100, 1), alias_id(100, 2));
        assert_ne!(alias_id(100, 1), 101);
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
