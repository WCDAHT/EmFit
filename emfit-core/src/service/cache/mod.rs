//! The scan cache: a volume's index on disk, plus what it takes to trust it.
//!
//! A scan of C: costs about thirteen seconds and reads three and a half
//! gigabytes. Almost none of that work is new the second time: the MFT the
//! sweep just read is the same MFT, minus whatever changed since. So the
//! finished index is written out, and the next scan of that volume loads it
//! and asks the USN journal what changed rather than reading the table again.
//!
//! The design and its reasoning are in `caching.md`. The short version:
//!
//! - **Truth is stored, derived data is recomputed.** Columns and the name
//!   arena go to disk; the CSR child array and the rollup totals do not,
//!   because the load path runs the snapshot back through [`IndexBuilder`]
//!   anyway. One index-construction path, so nothing can drift.
//! - **The journal is a change list, not a data source.** It says which MFT
//!   records were touched; those records are re-read and believed. Nothing is
//!   inferred from the reason bits, so coalesced or out-of-order events, and
//!   record slots reused by new files, all come out right
//!   ([`crate::service::replay`]).
//! - **Nothing loads by itself.** A cache is consulted when the user asks for
//!   a volume to be scanned, never at startup: what appears in the window is
//!   always something the user asked for.
//!
//! [`IndexBuilder`]: crate::model::builder::IndexBuilder

pub mod format;
pub mod manifest;
pub mod snapshot;

use std::path::{Path, PathBuf};

use directories::ProjectDirs;

use crate::app;
use crate::error::{Error, Result};
use crate::model::index::Index;
use crate::service::fold::CaseFold;
use crate::service::view::SortKey;

pub use manifest::{JournalStamp, Manifest, ScanStamp, VolumeStamp};
pub use snapshot::{CACHED_ORDERS, Columns, Loaded, Patch, RebuildStats};

/// Sub-directory of the OS data dir that holds snapshots.
const CACHE_DIR_NAME: &str = "cache";

/// Extension every snapshot carries.
pub const EXTENSION: &str = "emfit";

/// Where snapshots live: `<data dir>/cache`.
pub fn dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from(app::QUALIFIER, app::COMPANY, app::PRODUCT).ok_or_else(|| {
        Error::Config {
            message: "could not determine the OS data directory (no valid home directory)"
                .to_string(),
        }
    })?;
    Ok(dirs.data_local_dir().join(CACHE_DIR_NAME))
}

/// The file a volume's snapshot is filed under.
pub fn path_for(stamp: &VolumeStamp) -> Result<PathBuf> {
    Ok(dir()?.join(format!("{}.{EXTENSION}", stamp.fingerprint)))
}

/// Describe a finished scan, ready to be written alongside it.
///
/// `journal` is where the volume's change journal stood when the scan
/// **started**; without it the snapshot is unusable, because nothing could
/// ever bring it up to date, and [`save`] refuses it.
pub fn manifest_for(
    volume: &crate::model::volume::VolumeInfo,
    index: &Index,
    journal: JournalStamp,
    duration: std::time::Duration,
    access_mode: &str,
) -> Manifest {
    let arena_bytes: usize = index.ids().map(|id| index.name(id).len()).sum();
    Manifest {
        schema_version: manifest::SCHEMA_VERSION,
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        volume: VolumeStamp::of(volume),
        caps: index.caps().clone(),
        scan: ScanStamp::now(
            duration.as_millis() as u64,
            access_mode,
            index.len() as u64,
            arena_bytes as u64,
        ),
        journal: Some(journal),
    }
}

/// Write `index` out as the snapshot for the volume `manifest` describes.
///
/// Off the UI thread: compressing a few hundred megabytes takes a moment, and
/// nothing waits on it. A failure is worth logging and nothing more - the app
/// simply scans the long way next time.
pub fn save(
    index: &Index,
    fold: &CaseFold,
    manifest: &Manifest,
    orders: &[(SortKey, Vec<u32>)],
) -> Result<PathBuf> {
    let path = path_for(&manifest.volume)?;
    let started = std::time::Instant::now();
    snapshot::save(&path, index, fold, manifest, orders)?;

    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let elapsed = started.elapsed();
    tracing::info!(
        path = %path.display(),
        nodes = index.len(),
        orders = orders.len(),
        mib = bytes / (1 << 20),
        elapsed_ms = elapsed.as_millis() as u64,
        "cache: snapshot written"
    );
    let _ = crate::service::benchlog::append(
        "cache_save",
        elapsed,
        true,
        serde_json::json!({
            "volume": manifest.volume.root_label,
            "nodes": index.len(),
            "orders": orders.len(),
            "bytes": bytes,
        }),
    );
    Ok(path)
}

/// Add sort orders to a snapshot already on disk.
///
/// The truth is written the moment a scan finishes; the sort columns take
/// about ten seconds to build, and holding the file back for them means an app
/// closed in between caches nothing at all. So they arrive separately, into
/// table rows the write reserved. Idempotent: a column the file already has is
/// skipped.
pub fn append_orders(
    stamp: &VolumeStamp,
    nodes: usize,
    orders: &[(SortKey, Vec<u32>)],
) -> Result<usize> {
    let path = path_for(stamp)?;
    if !path.exists() {
        return Ok(0);
    }
    let started = std::time::Instant::now();
    let added = snapshot::append_orders(&path, nodes, orders)?;
    if added > 0 {
        tracing::info!(
            path = %path.display(),
            added,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "cache: sort orders appended"
        );
    }
    Ok(added)
}

/// Read the snapshot for `stamp`, if there is a usable one.
///
/// `Ok(None)` covers every ordinary miss - no file, a file for a volume that
/// has since been reformatted, a file this build cannot read - because none of
/// those is an error the user needs to see; they just mean "scan the long way".
pub fn load(stamp: &VolumeStamp) -> Result<Option<Loaded>> {
    let path = path_for(stamp)?;
    if !path.exists() {
        return Ok(None);
    }
    let started = std::time::Instant::now();
    match snapshot::load(&path) {
        Ok(loaded) => {
            if !loaded.manifest.volume.matches(stamp) {
                // Two volumes can only collide here by hashing alike, but a
                // stale file left by a reformat is a real possibility.
                tracing::info!(
                    path = %path.display(),
                    "cache: snapshot describes a different volume; ignoring it"
                );
                return Ok(None);
            }
            tracing::info!(
                path = %path.display(),
                nodes = loaded.columns.len(),
                orders = loaded.orders.len(),
                age_s = loaded.manifest.scan.age().as_secs(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "cache: snapshot read"
            );
            Ok(Some(loaded))
        }
        Err(e) => {
            // Corrupt, truncated, or written by a version that laid its
            // sections out differently. Nothing to do but scan.
            tracing::warn!(path = %path.display(), error = %e, "cache: snapshot unusable");
            Ok(None)
        }
    }
}

/// Delete the snapshot for one volume, if it exists.
pub fn forget(stamp: &VolumeStamp) -> Result<()> {
    let path = path_for(stamp)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io { path, source: e }),
    }
}

/// One snapshot on disk, as `cache list` sees it.
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub bytes: u64,
    pub manifest: Manifest,
}

/// Every readable snapshot in the cache directory, newest scan first.
///
/// Only headers are read, so this stays cheap no matter how big the files are.
pub fn list() -> Result<Vec<Entry>> {
    let dir = dir()?;
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new()); // no cache directory yet
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(EXTENSION) {
            continue;
        }
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let Ok(reader) = format::Reader::open(&path) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<Manifest>(reader.manifest()) else {
            continue;
        };
        out.push(Entry {
            path,
            bytes,
            manifest,
        });
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.manifest.scan.taken_at_unix));
    Ok(out)
}

/// Delete the oldest snapshots until the directory fits in `budget` bytes.
///
/// A disk-space analyzer that quietly eats half a gigabyte of disk would be a
/// bad joke, so the cache has a ceiling and enforces it after every write.
/// Returns how many files were removed.
pub fn evict(budget: u64) -> Result<usize> {
    let mut entries = list()?;
    let total: u64 = entries.iter().map(|e| e.bytes).sum();
    if total <= budget {
        return Ok(0);
    }

    // Oldest first, so the newest snapshots are the ones that survive.
    entries.sort_by_key(|e| e.manifest.scan.taken_at_unix);
    let mut freed = 0u64;
    let mut removed = 0usize;
    for entry in entries {
        if total - freed <= budget {
            break;
        }
        if std::fs::remove_file(&entry.path).is_ok() {
            tracing::info!(
                path = %entry.path.display(),
                mib = entry.bytes / (1 << 20),
                "cache: evicted an old snapshot"
            );
            freed += entry.bytes;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Remove every snapshot. Returns how many went.
pub fn clear() -> Result<usize> {
    let mut removed = 0;
    for entry in list()? {
        if std::fs::remove_file(&entry.path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// True when `path` looks like a snapshot rather than a disk image.
pub fn is_snapshot(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 8];
    use std::io::Read;
    file.read_exact(&mut magic).is_ok() && magic == format::MAGIC
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_directory_sits_under_the_product_data_dir() {
        let dir = dir().expect("resolves in a test environment");
        assert!(dir.ends_with(CACHE_DIR_NAME), "got {}", dir.display());
        assert!(dir.to_string_lossy().contains(app::PRODUCT));
    }

    #[test]
    fn a_file_that_is_not_a_snapshot_is_not_mistaken_for_one() {
        let path = std::env::temp_dir().join("emfit-not-a-snapshot.bin");
        std::fs::write(&path, b"MZ\x90\x00\x03\x00\x00\x00").expect("write");
        assert!(!is_snapshot(&path));
        assert!(!is_snapshot(Path::new("no-such-file-anywhere.emfit")));
    }
}
