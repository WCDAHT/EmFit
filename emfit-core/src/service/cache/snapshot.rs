//! An [`Index`] as columns on disk, and back again.
//!
//! Columns rather than a memory dump of [`Node`]: `Node` has private fields
//! and no `repr(C)`, so dumping it would freeze a layout the model is free to
//! change - and structure-of-arrays compresses several times better besides
//! (`caching.md` sec 4).
//!
//! Only truth is stored. The CSR child array and the rollup totals are left
//! out and recomputed, because the load path runs the snapshot back through
//! [`IndexBuilder`] anyway: one index-construction path, so a loaded index and
//! a scanned one cannot drift apart. Rebuilding costs a few hundred
//! milliseconds against the thirteen seconds a sweep costs.

use std::collections::HashMap;
use std::path::Path;

use crate::error::{Error, Result};
use crate::model::builder::IndexBuilder;
use crate::model::caps::VolumeCaps;
use crate::model::entry::{EntryFlags, RawEntry, Times};
use crate::model::index::{FS_OBJECT_MASK, Index, NodeId};
use crate::model::sink::{EntrySink, RecordedEntry, ScanWarning};
use crate::service::cache::format::{Appender, Codec, Reader, SectionClass, SectionKind, Writer};
use crate::service::cache::manifest::Manifest;
use crate::service::fold::CaseFold;
use crate::service::scan::FREE_SPACE_FS_ID;
use crate::service::task::CancellationToken;
use crate::service::view::SortKey;

/// On-disk section numbers. Never renumber one; retire it and take a new
/// number instead.
mod kind {
    use super::SectionKind;

    pub const PARENTS: SectionKind = SectionKind(1);
    pub const FLAGS: SectionKind = SectionKind(2);
    pub const NAME_LENS: SectionKind = SectionKind(3);
    pub const SIZES: SectionKind = SectionKind(4);
    pub const ALLOCATED: SectionKind = SectionKind(5);
    pub const MTIMES: SectionKind = SectionKind(6);
    pub const CRTIMES: SectionKind = SectionKind(7);
    pub const FS_IDS: SectionKind = SectionKind(8);
    pub const ARENA: SectionKind = SectionKind(9);
    pub const UPCASE: SectionKind = SectionKind(10);
    /// One per sort column, discriminated by `param`. Derived.
    pub const SORT_ORDER: SectionKind = SectionKind(64);
}

/// How many entries are handed to the builder at once on the way back in.
const REBUILD_BATCH: usize = 4096;

/// Table rows left empty for sections that arrive after the file is written.
///
/// The truth is worth a hundred and fifty megabytes and half a second; the
/// sort orders are worth ten seconds of CPU. Writing them together means an
/// app closed during the warm-up caches nothing at all, so the orders are
/// appended into these rows when they are ready ([`append_orders`]). Room for
/// every sort column plus a few of the side tables the advanced search will
/// want (`caching.md` sec 7).
const RESERVED_SECTIONS: u32 = 12;

/// The sort columns whose order is worth storing.
///
/// Measured on a 3.4M-node volume: `Path` costs 7.2 s to build, `Name`,
/// `Extension` and `Kind` about 0.9 s each, and the three numeric columns
/// under 0.3 s. Storing the cheap ones would trade 40 MB of disk for less time
/// than reading them back takes, so they are rebuilt instead.
pub const CACHED_ORDERS: [SortKey; 4] = [
    SortKey::Name,
    SortKey::Path,
    SortKey::Extension,
    SortKey::Kind,
];

/// Stable on-disk number for a sort column. A renumbering would silently sort
/// by the wrong key, so the mapping is explicit rather than `as u32`.
fn order_param(key: SortKey) -> u32 {
    match key {
        SortKey::Name => 1,
        SortKey::Path => 2,
        SortKey::Size => 3,
        SortKey::Allocated => 4,
        SortKey::Modified => 5,
        SortKey::Extension => 6,
        SortKey::Kind => 7,
    }
}

fn order_key(param: u32) -> Option<SortKey> {
    CACHED_ORDERS
        .iter()
        .chain([SortKey::Size, SortKey::Allocated, SortKey::Modified].iter())
        .copied()
        .find(|&key| order_param(key) == param)
}

// ---------------------------------------------------------------------------
// writing
// ---------------------------------------------------------------------------

/// Write one volume's index to `path`.
///
/// `orders` are precomputed sort columns ([`crate::service::view::build_order`]);
/// pass none and the next load simply warms them itself.
pub fn save(
    path: &Path,
    index: &Index,
    fold: &CaseFold,
    manifest: &Manifest,
    orders: &[(SortKey, Vec<u32>)],
) -> Result<()> {
    // Without stable ids there is nothing to key a node by, so a snapshot
    // could neither be replayed onto nor even loaded (the id column would be
    // empty while every other column is not). FAT32 will be the first scanner
    // to land here; refusing to write is the honest answer until it does.
    if !index.caps().has_stable_ids || index.fs_ids().len() != index.len() {
        return Err(Error::CacheInvalid {
            path: path.to_path_buf(),
            reason: "the volume has no stable file ids, so its scan cannot be cached".to_string(),
        });
    }

    let nodes = index.nodes();
    let text = serde_json::to_string_pretty(manifest)?;

    let upcase = match fold {
        CaseFold::Table(table) => Some(table),
        CaseFold::Simple => None,
    };
    let sections = 9 + usize::from(upcase.is_some()) + orders.len();
    let mut writer = Writer::create(path, &text, sections as u32, RESERVED_SECTIONS)?;

    let truth = |writer: &mut Writer, kind, bytes: &[u8]| -> Result<()> {
        writer.section(kind, 0, SectionClass::Truth, Codec::Lz4, bytes)
    };

    truth(
        &mut writer,
        kind::PARENTS,
        &pack(nodes.iter().map(|n| n.parent().get())),
    )?;
    truth(
        &mut writer,
        kind::FLAGS,
        &pack(nodes.iter().map(|n| n.flags().bits())),
    )?;
    truth(
        &mut writer,
        kind::NAME_LENS,
        &pack(index.ids().map(|id| index.name(id).len() as u16)),
    )?;
    truth(
        &mut writer,
        kind::SIZES,
        &pack(nodes.iter().map(|n| n.size())),
    )?;
    truth(
        &mut writer,
        kind::ALLOCATED,
        &pack(nodes.iter().map(|n| n.allocated())),
    )?;
    truth(
        &mut writer,
        kind::MTIMES,
        &pack(nodes.iter().map(|n| n.times().mtime)),
    )?;
    truth(
        &mut writer,
        kind::CRTIMES,
        &pack(nodes.iter().map(|n| n.times().crtime)),
    )?;
    truth(
        &mut writer,
        kind::FS_IDS,
        &pack(index.fs_ids().iter().copied()),
    )?;

    // Names in node order, concatenated. That is the arena's own order, so the
    // offsets the loader derives from the length column are the same ones the
    // builder assigned.
    let mut arena = String::with_capacity(index.len() * 16);
    for id in index.ids() {
        arena.push_str(index.name(id));
    }
    truth(&mut writer, kind::ARENA, arena.as_bytes())?;

    if let Some(table) = upcase {
        truth(&mut writer, kind::UPCASE, &pack(table.iter().copied()))?;
    }

    for (key, order) in orders {
        writer.section(
            kind::SORT_ORDER,
            order_param(*key),
            SectionClass::Derived,
            Codec::Lz4,
            &pack(order.iter().copied()),
        )?;
    }

    writer.finish()
}

/// Concatenate a column's little-endian bytes.
fn pack<T: Packable>(values: impl Iterator<Item = T>) -> Vec<u8> {
    let (lower, _) = values.size_hint();
    let mut out = Vec::with_capacity(lower * T::WIDTH);
    for value in values {
        value.write_le(&mut out);
    }
    out
}

/// One fixed-width little-endian column element.
trait Packable {
    const WIDTH: usize;
    fn write_le(self, out: &mut Vec<u8>);
}

macro_rules! packable {
    ($($t:ty),*) => {$(
        impl Packable for $t {
            const WIDTH: usize = std::mem::size_of::<$t>();
            fn write_le(self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }
        }
    )*};
}
packable!(u16, u32, u64, i64);

/// Add sort orders to a snapshot that was written without them.
///
/// Only the columns the file does not already carry are appended, so this is
/// idempotent and safe to run against a snapshot of any vintage. Returns how
/// many were added.
pub fn append_orders(path: &Path, nodes: usize, orders: &[(SortKey, Vec<u32>)]) -> Result<usize> {
    let mut appender = Appender::open(path)?;
    let present = appender.existing()?;
    let missing: Vec<&(SortKey, Vec<u32>)> = orders
        .iter()
        .filter(|(key, order)| {
            // A length mismatch means the file describes a different scan than
            // these orders index into - drop them rather than corrupt it.
            order.len() == nodes
                && !present
                    .iter()
                    .any(|&(kind, param)| kind == kind::SORT_ORDER && param == order_param(*key))
        })
        .collect();

    if missing.is_empty() || appender.room() < missing.len() as u32 {
        return Ok(0);
    }
    for (key, order) in missing {
        appender.section(
            kind::SORT_ORDER,
            order_param(*key),
            SectionClass::Derived,
            Codec::Lz4,
            &pack(order.iter().copied()),
        )?;
    }
    appender.finish()
}

// ---------------------------------------------------------------------------
// reading
// ---------------------------------------------------------------------------

/// One volume's truth, as it came off disk.
pub struct Columns {
    pub parents: Vec<u32>,
    pub flags: Vec<u16>,
    pub name_lens: Vec<u16>,
    pub sizes: Vec<u64>,
    pub allocated: Vec<u64>,
    pub mtimes: Vec<i64>,
    pub crtimes: Vec<i64>,
    pub fs_ids: Vec<u64>,
    pub arena: String,
    /// Cumulative name offsets, one per node plus a terminator. Derived from
    /// `name_lens` during validation, where the bounds are checked once so the
    /// rebuild can slice without checking again.
    offsets: Vec<u32>,
}

impl Columns {
    pub fn len(&self) -> usize {
        self.parents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }

    pub fn name(&self, at: usize) -> &str {
        &self.arena[self.offsets[at] as usize..self.offsets[at + 1] as usize]
    }
}

/// A snapshot, loaded but not yet an index.
pub struct Loaded {
    pub manifest: Manifest,
    pub columns: Columns,
    pub fold: CaseFold,
    /// Whichever sort columns the file happened to carry and pass validation.
    pub orders: HashMap<SortKey, Vec<u32>>,
}

/// Read a snapshot and check that it describes an index at all.
///
/// Every structural claim is verified here - name slices inside the arena and
/// on character boundaries, parents in range, exactly one self-parented root -
/// because [`Index`] slices raw and only `debug_assert`s its invariants. A
/// corrupt or tampered snapshot has to be caught in this function or it
/// panics on a hot path later (`caching.md` sec 5).
pub fn load(path: &Path) -> Result<Loaded> {
    let mut reader = Reader::open(path)?;
    let manifest: Manifest = serde_json::from_str(reader.manifest())?;

    let truth = |reader: &mut Reader, kind: SectionKind, name: &str| -> Result<Vec<u8>> {
        let entry = reader
            .find(kind, 0)
            .ok_or_else(|| invalid(path, &format!("no {name} section")))?;
        reader.read(&entry)
    };

    let parents = unpack_u32(&truth(&mut reader, kind::PARENTS, "parents")?);
    let n = parents.len();
    let flags = unpack_u16(&truth(&mut reader, kind::FLAGS, "flags")?);
    let name_lens = unpack_u16(&truth(&mut reader, kind::NAME_LENS, "name lengths")?);
    let sizes = unpack_u64(&truth(&mut reader, kind::SIZES, "sizes")?);
    let allocated = unpack_u64(&truth(&mut reader, kind::ALLOCATED, "allocated sizes")?);
    let mtimes = unpack_i64(&truth(&mut reader, kind::MTIMES, "modification times")?);
    let crtimes = unpack_i64(&truth(&mut reader, kind::CRTIMES, "creation times")?);
    let fs_ids = unpack_u64(&truth(&mut reader, kind::FS_IDS, "ids")?);
    let arena = String::from_utf8(truth(&mut reader, kind::ARENA, "arena")?)
        .map_err(|_| invalid(path, "the name arena is not UTF-8"))?;

    let fold = match reader.find(kind::UPCASE, 0) {
        Some(entry) => match reader.read(&entry) {
            Ok(bytes) => CaseFold::from_upcase(unpack_u16(&bytes)),
            // Folding is truth in the sense that a scan produced it, but
            // losing it costs accuracy on exotic characters, not correctness.
            Err(e) => {
                tracing::warn!(error = %e, "cache: $UpCase section unreadable; folding simply");
                CaseFold::Simple
            }
        },
        None => CaseFold::Simple,
    };

    let mut columns = Columns {
        parents,
        flags,
        name_lens,
        sizes,
        allocated,
        mtimes,
        crtimes,
        fs_ids,
        arena,
        offsets: Vec::new(),
    };
    validate(&mut columns, path)?;

    // Derived sections are best-effort by definition: anything missing or
    // unreadable is simply rebuilt later.
    let mut orders = HashMap::new();
    let order_entries: Vec<_> = reader
        .sections()
        .iter()
        .filter(|e| e.kind == kind::SORT_ORDER)
        .copied()
        .collect();
    for entry in order_entries {
        let Some(key) = order_key(entry.param) else {
            continue; // a column this build does not have
        };
        match reader.read(&entry) {
            Ok(bytes) => {
                let order = unpack_u32(&bytes);
                if is_permutation(&order, n) {
                    orders.insert(key, order);
                } else {
                    tracing::warn!(
                        ?key,
                        "cache: sort order is not a permutation; rebuilding it"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(?key, error = %e, "cache: sort order unreadable; rebuilding it")
            }
        }
    }

    Ok(Loaded {
        manifest,
        columns,
        fold,
        orders,
    })
}

fn invalid(path: &Path, reason: &str) -> Error {
    Error::CacheInvalid {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    }
}

/// Check every structural claim, and derive the name offsets while doing it.
fn validate(columns: &mut Columns, path: &Path) -> Result<()> {
    let n = columns.parents.len();
    if n == 0 {
        return Err(invalid(path, "the snapshot holds no nodes"));
    }
    if n > u32::MAX as usize {
        return Err(invalid(path, "the snapshot holds more nodes than exist"));
    }
    let same = [
        columns.flags.len(),
        columns.name_lens.len(),
        columns.sizes.len(),
        columns.allocated.len(),
        columns.mtimes.len(),
        columns.crtimes.len(),
        columns.fs_ids.len(),
    ];
    if same.iter().any(|&len| len != n) {
        return Err(invalid(path, "columns disagree about the node count"));
    }

    let mut roots = 0usize;
    for (i, &parent) in columns.parents.iter().enumerate() {
        if parent as usize >= n {
            return Err(invalid(path, "a node's parent is out of range"));
        }
        if parent as usize == i {
            roots += 1;
        }
    }
    // The builder guarantees exactly one - every other node was either
    // resolved or collected under the orphan folder.
    if roots != 1 {
        return Err(invalid(
            path,
            &format!("expected exactly one root, found {roots}"),
        ));
    }

    let mut offsets = Vec::with_capacity(n + 1);
    let mut at = 0usize;
    for &len in &columns.name_lens {
        offsets.push(at as u32);
        at += len as usize;
        if at > columns.arena.len() {
            return Err(invalid(path, "a name runs past the end of the arena"));
        }
        if !columns.arena.is_char_boundary(at) {
            return Err(invalid(path, "a name ends inside a character"));
        }
    }
    if at != columns.arena.len() {
        return Err(invalid(path, "the arena is longer than its names"));
    }
    offsets.push(at as u32);
    columns.offsets = offsets;
    Ok(())
}

/// Whether `order` lists every node exactly once.
fn is_permutation(order: &[u32], n: usize) -> bool {
    if order.len() != n {
        return false;
    }
    let mut seen = vec![false; n];
    for &id in order {
        match seen.get_mut(id as usize) {
            Some(slot) if !*slot => *slot = true,
            _ => return false,
        }
    }
    true
}

fn unpack_u16(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes(b.try_into().expect("2-byte chunk")))
        .collect()
}

fn unpack_u32(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes(b.try_into().expect("4-byte chunk")))
        .collect()
}

fn unpack_u64(bytes: &[u8]) -> Vec<u64> {
    bytes
        .chunks_exact(8)
        .map(|b| u64::from_le_bytes(b.try_into().expect("8-byte chunk")))
        .collect()
}

fn unpack_i64(bytes: &[u8]) -> Vec<i64> {
    bytes
        .chunks_exact(8)
        .map(|b| i64::from_le_bytes(b.try_into().expect("8-byte chunk")))
        .collect()
}

// ---------------------------------------------------------------------------
// rebuilding
// ---------------------------------------------------------------------------

/// What to change about a snapshot on its way back into an index.
///
/// Empty means "exactly what was scanned". A journal replay fills it in: the
/// records it re-read, the entries it found there, and the free space as it
/// stands now.
#[derive(Debug, Default)]
pub struct Patch {
    /// Filesystem object numbers whose nodes are replaced wholesale. A record
    /// with no matching entry in `entries` was deleted.
    ///
    /// Replacing every node of a record at once is what makes hard links come
    /// out right: a file with three names has three nodes, and dropping one
    /// link must remove exactly one of them.
    pub replace: std::collections::HashSet<u64>,
    pub entries: Vec<RecordedEntry>,
    /// Free space now. The free-space row is refreshed rather than replayed.
    pub free_bytes: Option<u64>,
}

impl Patch {
    pub fn is_empty(&self) -> bool {
        self.replace.is_empty() && self.entries.is_empty()
    }
}

/// What a rebuild did, beyond producing the index.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RebuildStats {
    /// Nodes carried over from the snapshot.
    pub kept: u64,
    /// Nodes the patch superseded or deleted.
    pub dropped: u64,
    /// Nodes the patch contributed.
    pub added: u64,
}

/// Push a snapshot back through the builder, applying `patch` on the way.
///
/// The same [`IndexBuilder`] a scan uses, running the same resolve, collect,
/// link and rollup passes. Nothing about index construction is duplicated
/// here, which is why a loaded index cannot disagree with a scanned one about
/// orphan collection or subtree totals.
pub fn rebuild(
    columns: &Columns,
    caps: VolumeCaps,
    patch: &Patch,
    cancel: &CancellationToken,
) -> Result<(Index, Vec<ScanWarning>, RebuildStats)> {
    let n = columns.len();
    let mut builder = IndexBuilder::new(caps, cancel.clone());
    builder.reserve(n + patch.entries.len());

    let mut stats = RebuildStats::default();
    let mut batch: Vec<RawEntry<'_>> = Vec::with_capacity(REBUILD_BATCH);

    for i in 0..n {
        let fs_id = columns.fs_ids[i];
        if patch.replace.contains(&(fs_id & FS_OBJECT_MASK)) {
            stats.dropped += 1;
            continue;
        }
        let parent = columns.parents[i] as usize;
        // The root is the node that is its own parent, which is exactly what
        // the builder recognizes.
        let parent_id = columns.fs_ids[parent];
        let size = if fs_id == FREE_SPACE_FS_ID {
            patch.free_bytes.unwrap_or(columns.sizes[i])
        } else {
            columns.sizes[i]
        };
        let allocated = if fs_id == FREE_SPACE_FS_ID {
            patch.free_bytes.unwrap_or(columns.allocated[i])
        } else {
            columns.allocated[i]
        };

        batch.push(RawEntry {
            fs_id,
            parent_id,
            name: columns.name(i),
            size,
            allocated,
            times: Times {
                mtime: columns.mtimes[i],
                crtime: columns.crtimes[i],
            },
            flags: EntryFlags::from_bits(columns.flags[i]),
        });
        stats.kept += 1;

        if batch.len() == REBUILD_BATCH {
            if builder.push_batch(&batch).is_break() {
                return Err(Error::Cancelled);
            }
            batch.clear();
        }
    }
    if !batch.is_empty() && builder.push_batch(&batch).is_break() {
        return Err(Error::Cancelled);
    }

    for chunk in patch.entries.chunks(REBUILD_BATCH) {
        let entries: Vec<RawEntry<'_>> = chunk.iter().map(RecordedEntry::as_raw).collect();
        if builder.push_batch(&entries).is_break() {
            return Err(Error::Cancelled);
        }
        stats.added += entries.len() as u64;
    }

    let (index, warnings) = builder.finish();
    Ok((index, warnings, stats))
}

/// Node ids whose object number is in `records`, for a caller that has to
/// replace them. One pass over the id column.
pub fn nodes_of(index: &Index, records: &std::collections::HashSet<u64>) -> Vec<NodeId> {
    index
        .fs_ids()
        .iter()
        .enumerate()
        .filter(|&(_, &fs_id)| records.contains(&(fs_id & FS_OBJECT_MASK)))
        .map(|(i, _)| NodeId::new(i as u32))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_permutation_is_recognized_and_a_forgery_is_not() {
        assert!(is_permutation(&[2, 0, 1], 3));
        assert!(is_permutation(&[], 0));
        assert!(!is_permutation(&[0, 0, 1], 3), "a repeat is not one");
        assert!(!is_permutation(&[0, 1, 9], 3), "out of range");
        assert!(!is_permutation(&[0, 1], 3), "too short");
    }

    #[test]
    fn columns_pack_and_unpack_unchanged() {
        let values = [1u64, 0, u64::MAX, 1 << 48];
        assert_eq!(unpack_u64(&pack(values.iter().copied())), values);
        let signed = [-1i64, 0, i64::MIN, i64::MAX];
        assert_eq!(unpack_i64(&pack(signed.iter().copied())), signed);
        let small = [0u16, 7, u16::MAX];
        assert_eq!(unpack_u16(&pack(small.iter().copied())), small);
    }

    #[test]
    fn every_cached_order_has_a_distinct_stable_number() {
        let mut seen = Vec::new();
        for key in [
            SortKey::Name,
            SortKey::Path,
            SortKey::Size,
            SortKey::Allocated,
            SortKey::Modified,
            SortKey::Extension,
            SortKey::Kind,
        ] {
            let param = order_param(key);
            assert!(!seen.contains(&param), "{key:?} reuses number {param}");
            assert_eq!(order_key(param), Some(key), "round trip for {key:?}");
            seen.push(param);
        }
    }
}
