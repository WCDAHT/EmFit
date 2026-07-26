//! Scan orchestration: from a discovered volume to a finished [`Index`].
//!
//! The layers below this one are deliberately ignorant of each other — a
//! [`BlockSource`] does not know what NTFS is, the sweep does not know where
//! its bytes come from, the builder does not know a disk exists. This module
//! is where they are wired together for one scan:
//!
//! 1. open the volume (physical-drive mode by default, volume-handle mode as
//!    fallback — features.md §1.1),
//! 2. recover the MFT layout (driver retrieval pointers when the volume is
//!    mounted, record 0 otherwise),
//! 3. sweep into an [`IndexBuilder`],
//! 4. add the free-space row so the totals account for the whole volume,
//! 5. finish the index and record a BenchLog event.
//!
//! [`scan_volumes`] runs several volumes in parallel with per-volume failure
//! isolation: one drive refusing access reports an error for that drive and
//! nothing else (features.md §7).

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
use crate::service::fold::CaseFold;
use crate::service::task::{CancellationToken, Progress};

/// The synthetic id the free-space row is pushed under. All ones — no real
/// MFT record can collide (record numbers are 48-bit) and no hard-link alias
/// can either (aliases reuse a real record's low 48 bits, and this value's
/// low 48 bits are no mintable record number).
const FREE_SPACE_FS_ID: u64 = u64::MAX;

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

/// Tuning for one volume scan.
#[derive(Debug, Clone, Default)]
pub struct VolumeScanOptions {
    pub mode: ScanMode,
    pub sweep: ScanOptions,
    /// Skip the synthetic free-space row. On by default because the treemap
    /// and totals should account for the whole volume (features.md §1.3).
    pub skip_free_space: bool,
}

/// Everything one volume scan produced.
#[derive(Debug)]
pub struct VolumeScanOutcome {
    pub index: Index,
    /// Recoverable problems, in the order they surfaced.
    pub warnings: Vec<ScanWarning>,
    pub stats: ScanStats,
    /// Alternate data streams, keyed by owning MFT record (a native id — map
    /// through [`Index::native_id`] when a node is in hand).
    pub ads: Vec<AdsStream>,
    /// How the bytes were actually read.
    pub mode: AccessMode,
    /// The volume's case-folding rule, from `$UpCase` when it was readable —
    /// search compares names with this, not with a global rule
    /// (features.md §2).
    pub fold: CaseFold,
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
    let mut layout = bootstrap::probe(&source)?;

    // The driver's map is authoritative for a mounted volume — one ioctl and
    // it reflects whatever the filesystem believes right now. Taken only when
    // it covers everything record 0 claims exists; otherwise the record 0 map
    // (already validated) stands.
    if let Some(letter) = volume.drive_letter() {
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
    }

    let free_space =
        (!options.skip_free_space && volume.free_bytes > 0).then_some(volume.free_bytes);

    run_scan(
        &source,
        &layout,
        ntfs_caps(volume.root_label()),
        options,
        mode,
        free_space,
        progress,
        cancel,
    )
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
/// Failure is isolated per volume: the result slot for a drive that refused
/// access carries its error, and every other drive completes normally.
/// Progress events arrive tagged with the volume's position in `volumes`.
pub fn scan_volumes(
    volumes: &[VolumeInfo],
    options: &VolumeScanOptions,
    on_progress: &(dyn Fn(usize, Progress) + Sync),
    cancel: &CancellationToken,
) -> Vec<Result<VolumeScanOutcome>> {
    std::thread::scope(|s| {
        let handles: Vec<_> = volumes
            .iter()
            .enumerate()
            .map(|(i, volume)| {
                s.spawn(move || {
                    let mut progress = |p: Progress| on_progress(i, p);
                    scan_volume(volume, options, &mut progress, cancel)
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(result) => result,
                // A panic is a bug in the scanner, not a property of the
                // volume; hiding it as a scan error would bury it.
                Err(payload) => std::panic::resume_unwind(payload),
            })
            .collect()
    })
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

    // Free space as a first-class row (features.md §1.3): without it the
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
    })
}

/// MFT record 10 is `$UpCase`: 128 KiB mapping every UTF-16 code unit to its
/// uppercase form — the table the filesystem itself compares names with.
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

/// Record the scan in the BenchLog (STANDARDS §5.5). Best-effort: a failure
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
