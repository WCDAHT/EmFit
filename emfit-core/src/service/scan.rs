//! Scan orchestration: from a discovered volume to a finished [`Index`].
//!
//! The layers below this one are deliberately ignorant of each other - a
//! [`BlockSource`] does not know what NTFS is, the sweep does not know where
//! its bytes come from, the builder does not know a disk exists. This module
//! is where they are wired together for one scan:
//!
//! 1. open the volume (physical-drive mode by default, volume-handle mode as
//!    fallback - features.md sec 1.1),
//! 2. recover the MFT layout (driver retrieval pointers when the volume is
//!    mounted, record 0 otherwise),
//! 3. sweep into an [`IndexBuilder`],
//! 4. add the free-space row so the totals account for the whole volume,
//! 5. finish the index and record a BenchLog event.
//!
//! [`scan_volumes`] runs several volumes in parallel with per-volume failure
//! isolation: one drive refusing access reports an error for that drive and
//! nothing else (features.md sec 7).

use std::time::Instant;

use crate::error::{Error, Result};
use crate::model::builder::IndexBuilder;
use crate::model::caps::VolumeCaps;
use crate::model::entry::{EntryFlags, RawEntry, Times};
use crate::model::index::Index;
use crate::model::sink::{EntrySink, ScanWarning};
use crate::model::volume::VolumeInfo;
use crate::parser::block::{AlignedBuf, BlockSource, FileBlockSource};
use crate::parser::ntfs::attr::AttributeType;
use crate::parser::ntfs::bootstrap::{self, MftLayout};
use crate::parser::ntfs::record::Record;
use crate::parser::ntfs::scanner::{AdsStream, ROOT_RECORD, ScanOptions, ScanStats};
use crate::parser::ntfs::{bitmap, retrieval, scanner};
use crate::service::benchlog;
use crate::service::cache::{self, JournalStamp};
use crate::service::fold::CaseFold;
use crate::service::replay;
use crate::service::task::{CancellationToken, Progress};
use crate::service::usn;
use crate::service::view;

/// The synthetic id the free-space row is pushed under. All ones - no real
/// MFT record can collide (record numbers are 48-bit) and no hard-link alias
/// can either (aliases reuse a real record's low 48 bits, and this value's
/// low 48 bits are no mintable record number).
pub const FREE_SPACE_FS_ID: u64 = u64::MAX;

/// How the caller wants the volume opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScanMode {
    /// Physical-drive mode when the partition is locatable, falling back to a
    /// volume handle. The default: physical mode bypasses the filesystem
    /// driver entirely and survives volumes that refuse ioctls.
    #[default]
    Auto,
    /// `\\.\PhysicalDriveN` plus the partition offset, or fail.
    Physical,
    /// `\\.\C:`, or fail.
    VolumeHandle,
}

/// How the volume actually ended up being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    Physical,
    VolumeHandle,
    Image,
}

impl AccessMode {
    /// Short name for logs and BenchLog events.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Physical => "physical",
            Self::VolumeHandle => "volume",
            Self::Image => "image",
        }
    }
}

/// Whether a scan may answer from the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CachePolicy {
    /// Load this volume's snapshot and replay the change journal onto it when
    /// both are available, sweeping the MFT only when they are not. The
    /// default, because the result is the same index either way.
    #[default]
    Use,
    /// Read the table, whatever is cached. The escape hatch for a user who
    /// suspects the cache, and what the CLI uses when measuring a real sweep.
    Ignore,
}

/// Tuning for one volume scan.
#[derive(Debug, Clone, Default)]
pub struct VolumeScanOptions {
    pub mode: ScanMode,
    pub sweep: ScanOptions,
    /// Skip the synthetic free-space row. On by default because the treemap
    /// and totals should account for the whole volume (features.md sec 1.3).
    pub skip_free_space: bool,
    pub cache: CachePolicy,
}

/// Everything one volume scan produced.
#[derive(Debug)]
pub struct VolumeScanOutcome {
    pub index: Index,
    /// Recoverable problems, in the order they surfaced.
    pub warnings: Vec<ScanWarning>,
    pub stats: ScanStats,
    /// Alternate data streams, keyed by owning MFT record (a native id - map
    /// through [`Index::native_id`] when a node is in hand).
    pub ads: Vec<AdsStream>,
    /// How the bytes were actually read.
    pub mode: AccessMode,
    /// The volume's case-folding rule, from `$UpCase` when it was readable -
    /// search compares names with this, not with a global rule
    /// (features.md sec 2).
    pub fold: CaseFold,
    /// Where the change journal stood when this scan started, when the volume
    /// has one. Without it the result cannot be cached: a snapshot with no
    /// journal position can never be brought up to date (`caching.md` sec 6.1).
    pub journal: Option<JournalStamp>,
    /// Whether the index was read off the volume or off a snapshot.
    pub source: OutcomeSource,
}

/// How an index came to exist.
#[derive(Debug, Default)]
pub enum OutcomeSource {
    /// Swept off the MFT.
    #[default]
    Scanned,
    /// Rebuilt from a snapshot with the journal replayed onto it.
    Cached {
        /// How old the snapshot was.
        age: std::time::Duration,
        replay: replay::ReplayStats,
        /// Sort columns the snapshot carried, ready to install instead of
        /// being warmed (`caching.md` sec 7).
        orders: std::collections::HashMap<crate::service::view::SortKey, Vec<u32>>,
    },
}

impl OutcomeSource {
    /// True when no MFT sweep was needed.
    pub fn is_cached(&self) -> bool {
        matches!(self, Self::Cached { .. })
    }
}

/// The capabilities an NTFS volume has. One place, so the UI, search, and the
/// CLI all agree on what NTFS can do.
pub fn ntfs_caps(root_label: String) -> VolumeCaps {
    VolumeCaps {
        case_sensitive: false,
        has_stable_ids: true,
        has_hard_links: true,
        has_allocated_size: true,
        // The USN journal exists; the watcher that uses it lands in M5.
        live_updates: true,
        sizes_are_exact: true,
        path_separator: '\\',
        root_label,
    }
}

/// Scan one NTFS volume into an index.
pub fn scan_volume(
    volume: &VolumeInfo,
    options: &VolumeScanOptions,
    progress: &mut dyn FnMut(Progress),
    cancel: &CancellationToken,
) -> Result<VolumeScanOutcome> {
    let started = Instant::now();
    let label = volume.display_name();

    let result = scan_volume_inner(volume, options, progress, cancel);

    log_bench(&label, started, &result);
    result
}

fn scan_volume_inner(
    volume: &VolumeInfo,
    options: &VolumeScanOptions,
    progress: &mut dyn FnMut(Progress),
    cancel: &CancellationToken,
) -> Result<VolumeScanOutcome> {
    let (source, mode) = open_volume_source(volume, options.mode)?;
    let layout = probe_layout(&source, volume)?;

    // The cheap way first. Only a volume that cannot be brought up to date
    // from its journal gets its table swept (`caching.md` sec 6).
    if options.cache == CachePolicy::Use {
        match from_cache(volume, options, &layout, mode, progress, cancel) {
            Ok(Some(outcome)) => return Ok(outcome),
            Ok(None) => {}
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            // A cache is an optimization; nothing about it is worth failing a
            // scan over.
            Err(e) => tracing::warn!(error = %e, "cache: reload failed; sweeping instead"),
        }
    }

    let free_space =
        (!options.skip_free_space && volume.free_bytes > 0).then_some(volume.free_bytes);

    // Where the journal stands *now*, before a single record is read. Anything
    // that changes during the sweep is then replayed by the next run rather
    // than missed - safe because replay re-reads records (`caching.md` sec 6.1).
    let journal = volume.drive_letter().and_then(|letter| match usn::query(letter) {
        Ok(state) => Some(JournalStamp {
            id: state.id,
            next_usn: state.next_usn,
        }),
        Err(e) => {
            tracing::info!(error = %e, "usn: no journal position; this scan will not be cacheable");
            None
        }
    });

    let mut outcome = run_scan(
        &source,
        &layout,
        ntfs_caps(volume.root_label()),
        options,
        mode,
        free_space,
        progress,
        cancel,
    )?;
    outcome.journal = journal;
    Ok(outcome)
}

/// Recover the MFT layout, preferring the filesystem driver's own extent map.
///
/// The driver's map is authoritative for a mounted volume - one ioctl and it
/// reflects whatever the filesystem believes right now. Taken only when it
/// covers everything record 0 claims exists; otherwise the record 0 map
/// (already validated) stands.
fn probe_layout(source: &FileBlockSource, volume: &VolumeInfo) -> Result<MftLayout> {
    let mut layout = bootstrap::probe(source)?;
    let Some(letter) = volume.drive_letter() else {
        return Ok(layout);
    };
    match retrieval::mft_extents(
        letter,
        layout.boot.bytes_per_cluster,
        layout.boot.bytes_per_record,
    ) {
        Ok(extents) if extents.capacity_records() >= layout.records_to_scan() => {
            tracing::debug!(
                fragments = extents.fragment_count(),
                "using the driver's $MFT extent map"
            );
            layout.extents = extents;
        }
        Ok(short) => tracing::warn!(
            driver_records = short.capacity_records(),
            needed = layout.records_to_scan(),
            "driver extent map is shorter than $MFT's data size; keeping the record 0 map"
        ),
        Err(e) => {
            tracing::debug!(error = %e, "retrieval pointers unavailable; using the record 0 map");
        }
    }
    Ok(layout)
}

/// Rebuild this volume's index from its snapshot, brought up to date.
///
/// `Ok(None)` is the ordinary answer for "there is nothing cached, or what is
/// cached cannot be trusted to be brought up to date" - every one of those
/// cases is a reason to sweep, not an error.
#[allow(
    clippy::too_many_arguments,
    reason = "internal seam; every argument is used"
)]
fn from_cache(
    volume: &VolumeInfo,
    options: &VolumeScanOptions,
    layout: &MftLayout,
    mode: AccessMode,
    progress: &mut dyn FnMut(Progress),
    cancel: &CancellationToken,
) -> Result<Option<VolumeScanOutcome>> {
    let started = Instant::now();
    let stamp = cache::VolumeStamp::of(volume);
    let Some(loaded) = cache::load(&stamp)? else {
        return Ok(None);
    };

    let Some(letter) = volume.drive_letter() else {
        return Ok(None); // no journal without a mount point
    };
    let Some(from) = loaded.manifest.journal else {
        tracing::info!("cache: {}", replay::Blocked::NoPosition.explain());
        return Ok(None);
    };

    progress(Progress::started("Reading the change journal", None));
    let nodes = loaded.columns.len();
    let (mut patch, journal, replayed) = match replay::replay(letter, layout, from, nodes, cancel)?
    {
        replay::Replay::Blocked(reason) => {
            tracing::info!(
                "cache: cannot replay - {}; sweeping instead",
                reason.explain()
            );
            return Ok(None);
        }
        replay::Replay::Unchanged { journal } => (
            cache::Patch::default(),
            journal,
            replay::ReplayStats::default(),
        ),
        replay::Replay::Changed {
            patch,
            journal,
            stats,
        } => (patch, journal, stats),
    };

    // Free space is a property of the volume right now, not of the scan that
    // produced the snapshot, so it is refreshed rather than replayed.
    patch.free_bytes =
        (!options.skip_free_space && volume.free_bytes > 0).then_some(volume.free_bytes);

    progress(Progress::started(
        "Rebuilding the index",
        Some(nodes as u64),
    ));
    let caps = ntfs_caps(volume.root_label());
    let (index, warnings, rebuilt, remap) =
        cache::snapshot::rebuild(&loaded.columns, caps, &patch, cancel)?;
    progress(Progress::Finished);

    // The rebuild renumbered the nodes, so the orders the snapshot carried
    // address the wrong rows until they are translated and the new nodes are
    // placed. Doing it here rather than letting the app warm from scratch is
    // the difference between a cached load opening sorted and a cached load
    // spending seven seconds on the Path column first.
    let orders = repair_orders(&loaded.columns, &patch, &index, loaded.orders, &remap);

    let root = index.node(index.root());
    let stats = ScanStats {
        files: u64::from(root.file_count()),
        directories: u64::from(root.dir_count()),
        entries_emitted: index.len() as u64,
        records_read: replayed.records,
        reads: replayed.reads,
        bytes_read: replayed.bytes_read,
        elapsed: started.elapsed(),
        ..ScanStats::default()
    };
    tracing::info!(
        volume = %volume.display_name(),
        nodes = index.len(),
        age_s = loaded.manifest.scan.age().as_secs(),
        events = replayed.events,
        records = replayed.records,
        kept = rebuilt.kept,
        dropped = rebuilt.dropped,
        added = rebuilt.added,
        elapsed_ms = stats.elapsed.as_millis() as u64,
        "cache: index rebuilt from the snapshot"
    );

    Ok(Some(VolumeScanOutcome {
        index,
        warnings,
        stats,
        // Alternate data streams are not stored in the snapshot: nothing reads
        // them yet, and they would cost a section as large as the arena.
        ads: Vec::new(),
        mode,
        fold: loaded.fold,
        journal: Some(journal),
        source: OutcomeSource::Cached {
            age: loaded.manifest.scan.age(),
            replay: replayed,
            orders,
        },
    }))
}

/// Bring the sort orders a snapshot carried onto the index that was rebuilt
/// from it.
///
/// Best-effort per column: one that cannot be repaired is simply left out, and
/// the app warms it the ordinary way. Costs two linear passes plus a binary
/// search per placed node ([`view::repair_order`]), so it is milliseconds
/// where rebuilding the Path column is seconds.
fn repair_orders(
    columns: &cache::snapshot::Columns,
    patch: &cache::Patch,
    index: &Index,
    orders: std::collections::HashMap<view::SortKey, Vec<u32>>,
    remap: &[u32],
) -> std::collections::HashMap<view::SortKey, Vec<u32>> {
    if orders.is_empty() {
        return orders;
    }
    let started = Instant::now();

    // Only the Path column cares. Every other key is a property of the node
    // itself, and a patch replaces every node it touches.
    let damage = cache::snapshot::path_damage(columns, patch);
    let paths_repairable = damage.repairable(columns.len());
    if damage.moved > 0 {
        tracing::info!(
            directories = damage.moved,
            stranded = damage.stranded.len(),
            repairable = paths_repairable,
            sample = ?damage.sample,
            "cache: directories moved, so nodes under them have to be placed again"
        );
    }

    let mut repaired = std::collections::HashMap::with_capacity(orders.len());
    for (key, order) in orders {
        let displaced: &[u32] = match key {
            view::SortKey::Path if !paths_repairable => {
                tracing::info!(
                    stranded = damage.stranded.len(),
                    "cache: too much of the tree moved; the path order has to be warmed"
                );
                continue;
            }
            view::SortKey::Path => &damage.stranded,
            _ => &[],
        };
        match view::repair_order(index, key, &order, remap, displaced) {
            Some(fixed) => {
                repaired.insert(key, fixed);
            }
            None => tracing::warn!(?key, "cache: the stored order does not fit this snapshot"),
        }
    }

    tracing::info!(
        columns = repaired.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "cache: sort orders repaired onto the rebuilt index"
    );
    repaired
}

/// Scan an NTFS filesystem inside a disk image file.
///
/// The cross-platform path: no drivers, no elevation, works on the fixture
/// images the tests use and on evidence images in the field. The image must
/// start at the filesystem's boot sector; MBR/GPT partition parsing arrives
/// with ext4 in M7.
pub fn scan_image(
    path: &std::path::Path,
    options: &VolumeScanOptions,
    progress: &mut dyn FnMut(Progress),
    cancel: &CancellationToken,
) -> Result<VolumeScanOutcome> {
    let started = Instant::now();
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    let result = (|| {
        let source = FileBlockSource::open_image(path)?;
        let layout = bootstrap::probe(&source)?;
        run_scan(
            &source,
            &layout,
            ntfs_caps(label.clone()),
            options,
            AccessMode::Image,
            None, // free bytes are a property of the mounted volume, unknown here
            progress,
            cancel,
        )
    })();

    log_bench(&label, started, &result);
    result
}

/// Scan several volumes at once, one thread each.
///
/// Parallelism follows the **physical disks**, not the volume list: volumes
/// on different disks scan concurrently, volumes sharing a disk scan one
/// after another. Two partitions of one SSD scanned at once halve each
/// other's sequential bandwidth and both finish late - "C: and D: at once
/// is nearly free" holds only when they are separate spindles/SSDs
/// (features.md sec 1.2).
///
/// Failure is isolated per volume: the result slot for a drive that refused
/// access carries its error, and every other drive completes normally.
/// Progress events arrive tagged with the volume's position in `volumes`.
pub fn scan_volumes(
    volumes: &[VolumeInfo],
    options: &VolumeScanOptions,
    on_progress: &(dyn Fn(usize, Progress) + Sync),
    cancel: &CancellationToken,
) -> Vec<Result<VolumeScanOutcome>> {
    let groups = group_by_disk(volumes);
    if groups.iter().any(|group| group.len() > 1) {
        tracing::info!(
            volumes = volumes.len(),
            lanes = groups.len(),
            "volumes share a physical disk; scanning same-disk volumes sequentially"
        );
    }

    let mut results: Vec<Option<Result<VolumeScanOutcome>>> =
        (0..volumes.len()).map(|_| None).collect();

    std::thread::scope(|s| {
        let handles: Vec<_> = groups
            .into_iter()
            .map(|group| {
                s.spawn(move || {
                    group
                        .into_iter()
                        .map(|i| {
                            let mut progress = |p: Progress| on_progress(i, p);
                            (i, scan_volume(&volumes[i], options, &mut progress, cancel))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();

        for handle in handles {
            match handle.join() {
                Ok(lane) => {
                    for (i, result) in lane {
                        results[i] = Some(result);
                    }
                }
                // A panic is a bug in the scanner, not a property of the
                // volume; hiding it as a scan error would bury it.
                Err(payload) => std::panic::resume_unwind(payload),
            }
        }
    });

    results
        .into_iter()
        .map(|slot| slot.expect("every volume lands in exactly one lane"))
        .collect()
}

/// Partition volume indices into scan lanes, one per physical disk.
///
/// A volume whose disk is unknown (spanned, or no partition location) gets
/// its own lane - the safe default, since nothing proves it shares media.
fn group_by_disk(volumes: &[VolumeInfo]) -> Vec<Vec<usize>> {
    let mut lanes: Vec<(Option<u32>, Vec<usize>)> = Vec::new();
    for (i, volume) in volumes.iter().enumerate() {
        match volume.location.map(|l| l.disk_number) {
            Some(disk) => {
                if let Some((_, lane)) = lanes.iter_mut().find(|(key, _)| *key == Some(disk)) {
                    lane.push(i);
                } else {
                    lanes.push((Some(disk), vec![i]));
                }
            }
            None => lanes.push((None, vec![i])),
        }
    }
    lanes.into_iter().map(|(_, lane)| lane).collect()
}

/// Open the volume for raw reading, honouring the requested mode.
///
/// Public because the CLI's diagnostics (`read-mft`) need the same access
/// path the scan uses.
pub fn open_volume_source(
    volume: &VolumeInfo,
    mode: ScanMode,
) -> Result<(FileBlockSource, AccessMode)> {
    #[cfg(windows)]
    {
        let physical = |volume: &VolumeInfo| -> Result<FileBlockSource> {
            let location = volume.location.ok_or_else(|| {
                Error::UnsupportedFormat(format!(
                    "{} spans several disk extents; physical mode needs a single partition",
                    volume.display_name()
                ))
            })?;
            let source = FileBlockSource::open_device(&location.physical_drive_path())?
                .partition(location.starting_offset, Some(location.length))
                .with_label(format!(
                    "{} on {}",
                    volume.display_name(),
                    location.physical_drive_path()
                ));
            Ok(apply_sector_size(source, volume))
        };
        let handle = |volume: &VolumeInfo| -> Result<FileBlockSource> {
            let source = FileBlockSource::open_device(&volume.device_path())?;
            Ok(apply_sector_size(source, volume))
        };

        match mode {
            ScanMode::Physical => Ok((physical(volume)?, AccessMode::Physical)),
            ScanMode::VolumeHandle => Ok((handle(volume)?, AccessMode::VolumeHandle)),
            ScanMode::Auto => match physical(volume) {
                Ok(source) => Ok((source, AccessMode::Physical)),
                Err(e) => {
                    tracing::debug!(error = %e, "physical mode unavailable; trying the volume handle");
                    Ok((handle(volume)?, AccessMode::VolumeHandle))
                }
            },
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (volume, mode);
        Err(Error::UnsupportedPlatform {
            operation: "raw volume access".to_string(),
        })
    }
}

#[cfg(windows)]
fn apply_sector_size(source: FileBlockSource, volume: &VolumeInfo) -> FileBlockSource {
    if volume.bytes_per_sector > 0 {
        source.with_sector_size(volume.bytes_per_sector)
    } else {
        source
    }
}

/// The shared back half: sweep, free-space row, finish.
#[allow(
    clippy::too_many_arguments,
    reason = "internal seam; every argument is used"
)]
fn run_scan(
    source: &dyn BlockSource,
    layout: &MftLayout,
    caps: VolumeCaps,
    options: &VolumeScanOptions,
    mode: AccessMode,
    free_bytes: Option<u64>,
    progress: &mut dyn FnMut(Progress),
    cancel: &CancellationToken,
) -> Result<VolumeScanOutcome> {
    let mut builder = IndexBuilder::new(caps, cancel.clone());
    builder.reserve(layout.records_to_scan().min(64 << 20) as usize);

    let outcome = scanner::sweep(
        source,
        layout,
        &mut builder,
        progress,
        cancel,
        options.sweep,
    )?;
    if outcome.stats.cancelled || cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }

    // The volume's own case table, so search compares names the way the
    // filesystem does. Missing or unreadable degrades to the simple rule.
    let fold = match read_upcase(source, layout) {
        Some(table) => CaseFold::from_upcase(table),
        None => {
            tracing::debug!("$UpCase not read; folding with the simple rule");
            CaseFold::Simple
        }
    };

    // Free space as a first-class row (features.md sec 1.3): without it the
    // treemap under-accounts the volume and "used plus free" never adds up.
    // Synthetic, so exports and the UI can disclose it is not a real file.
    if let Some(free) = free_bytes {
        let entry = RawEntry {
            fs_id: FREE_SPACE_FS_ID,
            parent_id: ROOT_RECORD,
            name: "Free space",
            size: free,
            allocated: free,
            times: Times::default(),
            flags: EntryFlags::SYNTHETIC,
        };
        let _ = builder.push_batch(&[entry]);
    }

    progress(Progress::started("Building the index", None));
    let (index, warnings) = builder.finish();
    progress(Progress::Finished);

    Ok(VolumeScanOutcome {
        index,
        warnings,
        stats: outcome.stats,
        ads: outcome.ads,
        mode,
        fold,
        journal: None,
        source: OutcomeSource::Scanned,
    })
}

/// MFT record 10 is `$UpCase`: 128 KiB mapping every UTF-16 code unit to its
/// uppercase form - the table the filesystem itself compares names with.
const UPCASE_RECORD: u64 = 10;

/// Read `$UpCase` off the volume. `None` on any problem: folding then falls
/// back to the simple rule, which costs accuracy on exotic characters, not
/// correctness.
fn read_upcase(source: &dyn BlockSource, layout: &MftLayout) -> Option<Vec<u16>> {
    let location = layout.extents.locate(UPCASE_RECORD)?;
    let record_size = layout.boot.bytes_per_record as usize;

    // The record's offset is record-aligned, not necessarily sector-aligned
    // (1 KiB records on a 4Kn disk); read the surrounding aligned window.
    let sector = u64::from(source.sector_size());
    let start = location.byte_offset / sector * sector;
    let lead = (location.byte_offset - start) as usize;
    let span = (lead + record_size).div_ceil(sector as usize) * sector as usize;

    let mut buf = AlignedBuf::for_source(source, span);
    source.read_exact_at(start, buf.as_mut_slice()).ok()?;
    let record_bytes = &mut buf.as_mut_slice()[lead..lead + record_size];

    let record = Record::parse(record_bytes, layout.boot.bytes_per_sector).ok()?;
    if !record.is_in_use() {
        return None;
    }

    for attribute in record.attributes() {
        if attribute.kind() != AttributeType::Data || !attribute.is_unnamed() {
            continue;
        }
        let bytes = if let Some(nr) = attribute.non_resident() {
            bitmap::read_non_resident(
                source,
                &nr.data_runs(),
                layout.boot.bytes_per_cluster,
                nr.data_size(),
            )
            .ok()?
        } else {
            attribute.resident_value()?.to_vec()
        };
        let table: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        return Some(table);
    }
    None
}

/// Record the scan in the BenchLog (STANDARDS sec 5.5). Best-effort: a failure
/// to write telemetry must never fail a scan.
fn log_bench(label: &str, started: Instant, result: &Result<VolumeScanOutcome>) {
    let elapsed = started.elapsed();
    let event = match result {
        Ok(outcome) => serde_json::json!({
            "volume": label,
            "mode": outcome.mode.as_str(),
            "records_read": outcome.stats.records_read,
            "entries": outcome.stats.entries_emitted,
            "files": outcome.stats.files,
            "directories": outcome.stats.directories,
            "bytes_read": outcome.stats.bytes_read,
            "reads": outcome.stats.reads,
            "mib_per_s": outcome.stats.mib_per_second().round(),
            "nodes": outcome.index.len(),
        }),
        Err(e) => serde_json::json!({
            "volume": label,
            "error": e.to_string(),
        }),
    };
    if let Err(e) = benchlog::append("scan_ntfs", elapsed, result.is_ok(), event) {
        tracing::debug!(error = %e, "could not write the BenchLog entry");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::volume::{FilesystemKind, PartitionLocation};

    fn volume_on(disk: Option<u32>) -> VolumeInfo {
        VolumeInfo {
            guid_path: String::new(),
            mount_points: Vec::new(),
            label: None,
            filesystem: FilesystemKind::Ntfs,
            serial: 0,
            bytes_per_sector: 512,
            bytes_per_cluster: 4096,
            total_bytes: 0,
            free_bytes: 0,
            location: disk.map(|disk_number| PartitionLocation {
                disk_number,
                starting_offset: 0,
                length: 0,
            }),
        }
    }

    #[test]
    fn volumes_sharing_a_disk_scan_in_one_lane() {
        // C: and D: on disk 0 (one SSD, two partitions), E: on disk 1,
        // one spanned volume with no single disk.
        let volumes = vec![
            volume_on(Some(0)), // C:
            volume_on(Some(1)), // E:
            volume_on(Some(0)), // D:
            volume_on(None),    // spanned
        ];

        let lanes = group_by_disk(&volumes);
        assert_eq!(lanes.len(), 3, "disk 0, disk 1, and the unknown");
        assert_eq!(lanes[0], vec![0, 2], "same disk -> sequential lane");
        assert_eq!(lanes[1], vec![1]);
        assert_eq!(lanes[2], vec![3]);
    }

    #[test]
    fn ntfs_caps_declare_what_ntfs_actually_has() {
        let caps = ntfs_caps("C:".to_string());
        assert!(!caps.case_sensitive, "NTFS compares case-insensitively");
        assert!(caps.has_stable_ids);
        assert!(caps.has_hard_links);
        assert!(caps.has_allocated_size);
        assert!(caps.sizes_are_exact);
        assert_eq!(caps.path_separator, '\\');
        assert_eq!(caps.root_label, "C:");
    }

    // FREE_SPACE_FS_ID can collide with nothing: record numbers are 48-bit,
    // alias ids reuse a real record's low 48 bits, and this value's low 48
    // bits are above any mintable record number. Checked at compile time.
    const _: () = assert!(FREE_SPACE_FS_ID & 0x0000_FFFF_FFFF_FFFF == 0x0000_FFFF_FFFF_FFFF);
}
