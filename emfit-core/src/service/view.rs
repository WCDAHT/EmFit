//! The result view: sorted hits and row windows.
//!
//! This is the Rust half of the row-window IPC contract (features.md sec 9):
//! the index never crosses to the webview. The shell keeps the current
//! query's hits here; the frontend asks for `rows(offset, count)` and gets
//! back **pre-formatted display strings plus ids** - never full nodes, never
//! the whole set.
//!
//! Sorting is stable (ties keep their previous order, so re-sorting a
//! filtered view by another column feels incremental, features.md sec 3) and
//! runs off the UI thread - the shell calls it from a worker.

use std::cmp::Ordering;

use rayon::prelude::*;

use crate::model::index::Index;
use crate::service::filetype::{FileKind, extension_of};
use crate::service::fold::CaseFold;
use crate::service::search::Hit;

/// Which column orders the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SortKey {
    Name,
    Path,
    #[default]
    Size,
    Allocated,
    Modified,
    Extension,
    Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub ascending: bool,
}

impl Default for Sort {
    /// Largest first: the question a space analyzer answers.
    fn default() -> Self {
        Self {
            key: SortKey::Size,
            ascending: false,
        }
    }
}

/// Sort hits in place, stably.
///
/// Display ordering uses [`CaseFold::Simple`] regardless of volume - the
/// per-volume `$UpCase` rule governs *matching*, but one list mixing two
/// volumes needs one consistent collation.
pub fn sort_hits(indices: &[&Index], hits: &mut [Hit], sort: Sort) {
    let fold = CaseFold::Simple;
    let node = |hit: &Hit| indices[hit.0 as usize].node(hit.1);
    let name = |hit: &Hit| indices[hit.0 as usize].name(hit.1);

    // Every arm derives its key ONCE per hit (`par_sort_by_cached_key` /
    // decoration), never per comparison: a comparator that folds or
    // classifies inline runs n*log n times - ~70M for a 3M-row view, which
    // profiled as ~9 s of pure sort. Char-wise folded comparison and
    // comparing pre-folded `String`s order identically (char code-point
    // order == UTF-8 byte order), and the transient key allocations are the
    // same accepted tradeoff as the Path decoration below. Rayon spreads
    // both the key construction and the sort across cores - the string
    // sorts profiled ~57% in key allocation, ~37% in the sort itself, both
    // of which parallelize; stability is preserved.
    let folded = |s: &str| -> String { s.chars().map(|c| fold.fold(c)).collect() };
    match sort.key {
        SortKey::Name => hits.par_sort_by_cached_key(|h| folded(name(h))),
        SortKey::Size => hits.par_sort_by_cached_key(|h| effective_size(indices, *h)),
        SortKey::Allocated => hits.par_sort_by_cached_key(|h| effective_allocated(indices, *h)),
        SortKey::Modified => hits.par_sort_by_cached_key(|h| node(h).times().mtime),
        SortKey::Extension => {
            hits.par_sort_by_cached_key(|h| (folded(extension_of(name(h))), folded(name(h))));
        }
        SortKey::Kind => hits.par_sort_by_cached_key(|h| {
            let n = name(h);
            (FileKind::classify(n, node(h).is_directory()), folded(n))
        }),
        SortKey::Path => {
            // Paths are assembled strings; comparing by walking parents for
            // every comparison would be O(depth * n log n). Decorate once
            // instead - the cost is one path string per hit, held only for
            // the duration of the sort.
            let mut decorated: Vec<(String, Hit)> = hits
                .par_iter()
                .map(|&(vol, id)| (indices[vol as usize].path(id), (vol, id)))
                .collect();
            decorated.par_sort_by(|a, b| fold.cmp(&a.0, &b.0));
            for (slot, (_, hit)) in decorated.into_iter().enumerate() {
                hits[slot] = hit;
            }
        }
    }

    if !sort.ascending {
        hits.reverse();
    }
}

/// One column's precomputed total ordering across the whole volume set:
/// `ranks[vol][node_id] = position of that node in the column's ascending
/// global order`. Everything's "fast sort" trick: the index is immutable
/// between scans, so each column's order can be computed ONCE - after
/// which sorting any hit subset, filtered or not, is integer-key work
/// ([`sort_by_ranks`]), never string folding or node chasing.
pub type SortRanks = Vec<std::sync::Arc<[u32]>>;

/// Build the rank table for one column. Costs one full sort of every node
/// (the same work one uncached sort of a match-all view does) - intended to
/// run once per scan per column, off the query path.
pub fn build_ranks(indices: &[&Index], key: SortKey) -> SortRanks {
    let mut all: Vec<Hit> = indices
        .iter()
        .enumerate()
        .flat_map(|(vol, index)| index.ids().map(move |id| (vol as u16, id)))
        .collect();
    sort_hits(
        indices,
        &mut all,
        Sort {
            key,
            ascending: true,
        },
    );
    let mut ranks: Vec<Vec<u32>> = indices.iter().map(|index| vec![0; index.len()]).collect();
    for (position, &(vol, id)) in all.iter().enumerate() {
        ranks[vol as usize][id.get() as usize] = position as u32;
    }
    ranks
        .into_iter()
        .map(|r| std::sync::Arc::from(r.into_boxed_slice()))
        .collect()
}

/// One volume's node ids in a column's ascending order.
///
/// The inverse of a rank table, and the form the scan cache stores
/// (`caching.md` sec 7). Ranks can only be used; an order can also be
/// *repaired* - new nodes binary-search into it and deleted ones splice out -
/// which is what a future replay will do instead of spending 7.2 s rebuilding
/// the `Path` column from scratch.
pub fn build_order(index: &Index, key: SortKey) -> Vec<u32> {
    let mut all: Vec<Hit> = index.ids().map(|id| (0u16, id)).collect();
    sort_hits(
        &[index],
        &mut all,
        Sort {
            key,
            ascending: true,
        },
    );
    all.into_iter().map(|(_, id)| id.get()).collect()
}

/// Turn one volume's order into the rank table the sort path wants.
///
/// Only valid for a single-volume view: ranks are positions in one global
/// ordering, and merging two volumes' orders means comparing their keys, which
/// for `Path` costs exactly as much as building the order did. A view over
/// several volumes therefore warms its ranks the ordinary way.
pub fn ranks_from_order(order: &[u32]) -> SortRanks {
    let mut ranks = vec![0u32; order.len()];
    for (position, &id) in order.iter().enumerate() {
        // The caller has already checked that this is a permutation; a bad
        // index here would be a cache that passed validation.
        if let Some(slot) = ranks.get_mut(id as usize) {
            *slot = position as u32;
        }
    }
    vec![std::sync::Arc::from(ranks.into_boxed_slice())]
}

/// Recover one volume's ascending order from its rank column.
///
/// The inverse of [`ranks_from_order`], and the reason the cache can be
/// written after the warm-up instead of before it: whatever the ranks cost to
/// build, turning them back into an order is an integer sort. For a view over
/// several volumes the ranks are positions in one global ordering, and this
/// still yields the right per-volume order - a global order restricted to one
/// volume is that volume's order.
pub fn order_from_ranks(ranks: &[u32]) -> Vec<u32> {
    let mut order: Vec<u32> = (0..ranks.len() as u32).collect();
    order.par_sort_unstable_by_key(|&id| ranks[id as usize]);
    order
}

/// Order hits by a precomputed rank table - the fast path for every sort
/// after the first. Unstable is safe: ranks are a permutation, so keys are
/// unique and there are no equal elements to keep stable.
pub fn sort_by_ranks(hits: &mut [Hit], ranks: &SortRanks, ascending: bool) {
    hits.par_sort_unstable_by_key(|&(vol, id)| ranks[vol as usize][id.get() as usize]);
    if !ascending {
        hits.reverse();
    }
}

// ---------------------------------------------------------------------------
// incremental repair
// ---------------------------------------------------------------------------

/// The place in a [`Remap`] of a node the rebuild did not carry over.
pub const REMOVED: u32 = u32::MAX;

/// Where each of a snapshot's nodes ended up after a rebuild:
/// `remap[old_id] = new_id`, or [`REMOVED`] for one the patch dropped.
///
/// A rebuild renumbers: it pushes the survivors in their old order skipping
/// the dropped ones, so every id after the first deletion shifts down. That is
/// what made a cached order unusable on a patched snapshot - it addressed rows
/// that had moved - and this is the translation that makes it usable again.
pub type Remap = [u32];

/// The primary key of a sort column, in whatever type that column sorts by.
///
/// Only ever compared against another key of the same column, so the variant
/// ordering that `Ord` derives is never reached.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Primary {
    Text(String),
    Kind(FileKind),
    Bytes(u64),
    Time(i64),
}

/// One node's place under one sort column, as a *total* order.
///
/// Two things this has to get right, both of them ways a repair could quietly
/// diverge from a rebuild:
///
/// - It derives exactly the keys [`sort_hits`] derives. Any drift between the
///   two and a repaired order would sort by something subtly different from
///   the column it claims to be.
/// - Ties break by node id, which is what a stable sort over ascending ids
///   already does. Without that a new node could be seated on either side of
///   an equal one, and the Size column of a volume with ten thousand empty
///   files is nothing but ties.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct OrderKey {
    primary: Primary,
    secondary: String,
    id: u32,
}

/// The sort key of one node, matching [`sort_hits`] arm for arm.
fn order_key(index: &Index, key: SortKey, id: u32) -> OrderKey {
    let fold = CaseFold::Simple;
    let folded = |s: &str| -> String { s.chars().map(|c| fold.fold(c)).collect() };
    let node_id = crate::model::index::NodeId::new(id);
    let name = index.name(node_id);
    let hit = (0u16, node_id);

    let (primary, secondary) = match key {
        SortKey::Name => (Primary::Text(folded(name)), String::new()),
        // Folding the path and comparing the results orders identically to
        // `fold.cmp` on the raw paths, which is what the Path arm of
        // `sort_hits` uses (code-point order == UTF-8 byte order).
        SortKey::Path => (Primary::Text(folded(&index.path(node_id))), String::new()),
        SortKey::Extension => (Primary::Text(folded(extension_of(name))), folded(name)),
        SortKey::Kind => (
            Primary::Kind(FileKind::classify(name, index.node(node_id).is_directory())),
            folded(name),
        ),
        SortKey::Size => (Primary::Bytes(effective_size(&[index], hit)), String::new()),
        SortKey::Allocated => (
            Primary::Bytes(effective_allocated(&[index], hit)),
            String::new(),
        ),
        SortKey::Modified => (
            Primary::Time(index.node(node_id).times().mtime),
            String::new(),
        ),
    };
    OrderKey {
        primary,
        secondary,
        id,
    }
}

/// Bring a cached order up to date instead of rebuilding it.
///
/// The point of the whole exercise: rebuilding the `Path` column of a
/// three-million-node volume costs about 7.2 s of string work, and a replay
/// that touched two hundred files has not changed the answer for the other
/// 2,999,800. So the survivors are carried across unchanged and only the new
/// nodes are placed.
///
/// With `n` nodes, `f` to place and `C` the cost of deriving one key
/// (O(depth) for `Path`, O(1) for `Size`):
///
/// - carrying the survivors over is one pass, O(n), and derives **no keys**;
/// - placing them is `f log f + f log n` key comparisons - about 22 per node
///   at three million, against the `n log n` a rebuild pays;
/// - splicing them in is one pass, O(n + f), with no comparisons in it at all,
///   because a binary search already said where each one goes.
///
/// `displaced` names snapshot nodes that survived but whose key changed anyway,
/// so they have to be lifted out of the order and placed again. Only the Path
/// column ever has any: a node's own name, size and times are safe by
/// construction, since a patch replaces every node it touches, but a path is
/// assembled from ancestors and moving a directory moves every descendant
/// without the journal ever mentioning them. `snapshot::path_damage` is what
/// finds them; pass an empty slice for every other column.
///
/// `Some` only when `remap` describes the snapshot `old_order` came from and
/// the result addresses `index` exactly; `None` says warm the column the
/// ordinary way rather than trust a mismatch.
pub fn repair_order(
    index: &Index,
    key: SortKey,
    old_order: &[u32],
    remap: &Remap,
    displaced: &[u32],
) -> Option<Vec<u32>> {
    if old_order.len() != remap.len() {
        return None; // this order did not come from the snapshot that was rebuilt
    }
    let n = index.len();

    let mut lifted = vec![false; remap.len()];
    for &old in displaced {
        *lifted.get_mut(old as usize)? = true;
    }

    // Pass 1 - carry the survivors over, in the order they were already in.
    // Renumbering is a compaction, so it preserves relative order, and a
    // survivor's key did not change; the result is still sorted.
    let mut kept: Vec<u32> = Vec::with_capacity(n);
    let mut covered = vec![false; n];
    for &old in old_order {
        let new = *remap.get(old as usize)?;
        if new == REMOVED || lifted[old as usize] {
            continue;
        }
        let slot = covered.get_mut(new as usize)?;
        if *slot {
            return None; // two old nodes claiming one new one
        }
        *slot = true;
        kept.push(new);
    }

    // Pass 2 - whatever the old order no longer accounts for: the patch's own
    // entries, anything `IndexBuilder::finish` synthesized (a replacement
    // root, the folder that collects orphans), and the nodes lifted out above.
    // Placing them is all the same job, so they are all one list.
    let mut fresh: Vec<u32> = (0..n as u32).filter(|&id| !covered[id as usize]).collect();
    if fresh.is_empty() {
        return Some(kept);
    }
    fresh.par_sort_by_cached_key(|&id| order_key(index, key, id));

    // Pass 3 - one binary search each. This is the only place keys are derived
    // against the existing order, and it touches log n of its entries rather
    // than all of them, which is the whole difference from a merge.
    let at: Vec<usize> = fresh
        .par_iter()
        .map(|&id| {
            let probe = order_key(index, key, id);
            kept.partition_point(|&other| order_key(index, key, other) < probe)
        })
        .collect();

    // Pass 4 - splice. `fresh` is sorted and `kept` is sorted, so the
    // insertion points come out non-decreasing and this is a merge by
    // position: no keys, no comparisons, one memmove's worth of work.
    let mut out = Vec::with_capacity(kept.len() + fresh.len());
    let mut next = 0;
    for (position, &id) in kept.iter().enumerate() {
        while next < fresh.len() && at[next] <= position {
            out.push(fresh[next]);
            next += 1;
        }
        out.push(id);
    }
    out.extend_from_slice(&fresh[next..]);

    (out.len() == n).then_some(out)
}

/// The size a user expects to see beside a row: files show their own bytes,
/// directories their subtree total.
/// The index a hit belongs to, if that volume is still loaded and still has
/// that node.
///
/// A hit is only meaningful against the volume set that produced it, and
/// scanning replaces that set. Every path that resolves one therefore asks
/// rather than indexing: the release profile builds with `panic = "abort"`, so
/// an out-of-bounds here is not a caught error but the process vanishing
/// mid-click with nothing written to the log.
fn resolve<'a>(indices: &'a [&Index], (vol, id): Hit) -> Option<&'a Index> {
    let index = *indices.get(vol as usize)?;
    (id.index() < index.len()).then_some(index)
}

fn effective_size(indices: &[&Index], hit: Hit) -> u64 {
    let Some(index) = resolve(indices, hit) else {
        return 0;
    };
    let node = index.node(hit.1);
    if node.is_directory() {
        node.total_size()
    } else {
        node.size()
    }
}

fn effective_allocated(indices: &[&Index], hit: Hit) -> u64 {
    let Some(index) = resolve(indices, hit) else {
        return 0;
    };
    let node = index.node(hit.1);
    if node.is_directory() {
        node.total_allocated()
    } else {
        node.allocated()
    }
}

/// One display row: everything the list needs, nothing more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Volume slot + node id - the handle for selection and future actions.
    pub vol: u16,
    pub id: u32,
    pub name: String,
    /// The containing directory's full path (the Path column).
    pub dir_path: String,
    pub size: u64,
    pub allocated: u64,
    pub size_display: String,
    pub allocated_display: String,
    pub modified_display: String,
    pub extension: String,
    pub kind_label: String,
    /// `--category-N` slot for type coloring; 0 = neutral.
    pub category: u8,
    pub is_directory: bool,
    pub is_hidden: bool,
    pub is_system: bool,
    pub is_alias: bool,
    pub is_synthetic: bool,
    pub is_reparse: bool,
}

/// Build the display rows for one window of the hit list.
pub fn build_rows(indices: &[&Index], hits: &[Hit], offset: usize, count: usize) -> Vec<Row> {
    let end = offset.saturating_add(count).min(hits.len());
    let window = &hits[offset.min(hits.len())..end];

    window
        .iter()
        .filter_map(|&(vol, id)| {
            // A hit left over from a volume set that has been replaced. It
            // will be gone as soon as the new query finishes; until then it is
            // one row missing from a window, not a dead process.
            let index = resolve(indices, (vol, id))?;
            let node = index.node(id);
            let flags = node.flags();
            let name = index.name(id).to_string();
            let kind = FileKind::classify(&name, node.is_directory());
            let size = effective_size(indices, (vol, id));
            let allocated = effective_allocated(indices, (vol, id));

            Some(Row {
                vol,
                id: id.get(),
                dir_path: index.path(node.parent()),
                size,
                allocated,
                size_display: human_size(size),
                allocated_display: human_size(allocated),
                modified_display: format_mtime(node.times().mtime),
                extension: if node.is_directory() {
                    String::new()
                } else {
                    extension_of(&name).to_string()
                },
                kind_label: kind.label().to_string(),
                category: kind.category_slot(),
                is_directory: node.is_directory(),
                is_hidden: flags.contains(crate::model::entry::EntryFlags::HIDDEN),
                is_system: flags.contains(crate::model::entry::EntryFlags::SYSTEM),
                is_alias: node.is_alias(),
                is_synthetic: node.is_synthetic(),
                is_reparse: flags.contains(crate::model::entry::EntryFlags::REPARSE),
                name,
            })
        })
        .collect()
}

/// Sum of the selected rows, for the status bar's "how much would this free".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SelectionSummary {
    pub count: u64,
    pub bytes: u64,
    pub allocated: u64,
}

/// Summarize hits by position. `picks` indexes into `hits`; out-of-range
/// picks (a stale selection racing a new query) are skipped.
pub fn summarize(indices: &[&Index], hits: &[Hit], picks: &[u32]) -> SelectionSummary {
    let mut summary = SelectionSummary::default();
    for &pick in picks {
        let Some(&hit) = hits.get(pick as usize) else {
            continue;
        };
        if resolve(indices, hit).is_none() {
            continue;
        }
        summary.count += 1;
        summary.bytes += effective_size(indices, hit);
        summary.allocated += effective_allocated(indices, hit);
    }
    summary
}

/// Summarize the whole hit list (select-all without shipping 5M indexes).
pub fn summarize_all(indices: &[&Index], hits: &[Hit]) -> SelectionSummary {
    let mut summary = SelectionSummary {
        count: hits.len() as u64,
        ..Default::default()
    };
    for &hit in hits {
        summary.bytes += effective_size(indices, hit);
        summary.allocated += effective_allocated(indices, hit);
    }
    summary
}

/// `1.5 GiB`-style display, one decimal, binary units.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `2024-06-15 14:02` in UTC, or empty when the filesystem didn't say.
pub fn format_mtime(nanos: i64) -> String {
    if nanos == 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp_nanos(nanos)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// Compare helper exposed for tests: the collation used by name sorting.
pub fn display_cmp(a: &str, b: &str) -> Ordering {
    CaseFold::Simple.cmp(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::builder::IndexBuilder;
    use crate::model::entry::{EntryFlags, RawEntry, Times};
    use crate::model::sink::EntrySink;
    use crate::service::task::CancellationToken;

    fn entry(fs_id: u64, parent: u64, name: &str, size: u64, mtime: i64) -> RawEntry<'_> {
        RawEntry {
            fs_id,
            parent_id: parent,
            name,
            size,
            allocated: size / 2,
            times: Times { mtime, crtime: 0 },
            flags: EntryFlags::empty(),
        }
    }

    fn index() -> Index {
        let caps = crate::service::scan::ntfs_caps("C:".to_string());
        let mut b = IndexBuilder::new(caps, CancellationToken::new());
        let _ = b.push_batch(&[
            RawEntry {
                fs_id: 5,
                parent_id: 5,
                name: "",
                size: 0,
                allocated: 0,
                times: Times::default(),
                flags: EntryFlags::DIRECTORY,
            },
            entry(10, 5, "bravo.txt", 100, 3),
            entry(11, 5, "Alpha.pdf", 300, 1),
            entry(12, 5, "charlie.txt", 200, 2),
        ]);
        b.finish().0
    }

    /// A volume with fewer nodes than [`index`], for the "scanned a smaller
    /// drive" half of the stale-hit regression.
    fn small_index() -> Index {
        let caps = crate::service::scan::ntfs_caps("D:".to_string());
        let mut b = IndexBuilder::new(caps, CancellationToken::new());
        let _ = b.push_batch(&[RawEntry {
            fs_id: 5,
            parent_id: 5,
            name: "",
            size: 0,
            allocated: 0,
            times: Times::default(),
            flags: EntryFlags::DIRECTORY,
        }]);
        b.finish().0
    }

    fn hits(index: &Index) -> Vec<Hit> {
        index
            .ids()
            .filter(|&id| !index.name(id).is_empty())
            .map(|id| (0u16, id))
            .collect()
    }

    #[test]
    fn ranked_sort_orders_exactly_like_the_direct_sort() {
        let index = index();
        let indices = [&index];
        for key in [
            SortKey::Name,
            SortKey::Size,
            SortKey::Allocated,
            SortKey::Modified,
            SortKey::Extension,
            SortKey::Kind,
            SortKey::Path,
        ] {
            for ascending in [true, false] {
                let sort = Sort { key, ascending };
                let mut direct = hits(&index);
                sort_hits(&indices, &mut direct, sort);

                let ranks = build_ranks(&indices, key);
                let mut ranked = hits(&index);
                sort_by_ranks(&mut ranked, &ranks, ascending);

                assert_eq!(ranked, direct, "key {key:?} ascending {ascending}");
            }
        }
    }

    fn names(index: &Index, hits: &[Hit]) -> Vec<String> {
        hits.iter()
            .map(|&(_, id)| index.name(id).to_string())
            .collect()
    }

    #[test]
    fn a_cached_order_ranks_exactly_like_a_freshly_built_table() {
        // The cache stores orders and derives ranks from them, so the two
        // routes to a rank table have to agree node for node.
        let index = index();
        for key in [
            SortKey::Name,
            SortKey::Path,
            SortKey::Size,
            SortKey::Allocated,
            SortKey::Modified,
            SortKey::Extension,
            SortKey::Kind,
        ] {
            let order = build_order(&index, key);
            assert_eq!(order.len(), index.len(), "{key:?} covers every node");
            assert_eq!(
                ranks_from_order(&order),
                build_ranks(&[&index], key),
                "{key:?}"
            );
        }
    }

    #[test]
    fn ranks_and_orders_are_inverses_of_each_other() {
        let index = index();
        for key in [SortKey::Name, SortKey::Path, SortKey::Size] {
            let order = build_order(&index, key);
            let ranks = build_ranks(&[&index], key);
            assert_eq!(order_from_ranks(&ranks[0]), order, "{key:?}");
        }
    }

    #[test]
    fn name_sort_ignores_case_both_directions() {
        let index = index();
        let mut h = hits(&index);

        sort_hits(
            &[&index],
            &mut h,
            Sort {
                key: SortKey::Name,
                ascending: true,
            },
        );
        assert_eq!(
            names(&index, &h),
            vec!["Alpha.pdf", "bravo.txt", "charlie.txt"]
        );

        sort_hits(
            &[&index],
            &mut h,
            Sort {
                key: SortKey::Name,
                ascending: false,
            },
        );
        assert_eq!(
            names(&index, &h),
            vec!["charlie.txt", "bravo.txt", "Alpha.pdf"]
        );
    }

    #[test]
    fn default_sort_is_largest_first() {
        let index = index();
        let mut h = hits(&index);
        sort_hits(&[&index], &mut h, Sort::default());
        assert_eq!(
            names(&index, &h),
            vec!["Alpha.pdf", "charlie.txt", "bravo.txt"]
        );
    }

    #[test]
    fn extension_sort_groups_then_orders_by_name() {
        let index = index();
        let mut h = hits(&index);
        sort_hits(
            &[&index],
            &mut h,
            Sort {
                key: SortKey::Extension,
                ascending: true,
            },
        );
        assert_eq!(
            names(&index, &h),
            vec!["Alpha.pdf", "bravo.txt", "charlie.txt"]
        );
    }

    #[test]
    fn hits_that_outlived_their_volumes_are_dropped_rather_than_fatal() {
        // Regression: pressing Scan clears the loaded volumes, but the last
        // query's hits stayed behind. A row window or a selection total asked
        // for in that gap resolved a node id against a volume that was gone -
        // an out-of-bounds index, which in a `panic = "abort"` release build
        // takes the process down with nothing in the log.
        let index = index();
        let h = hits(&index);
        assert!(!h.is_empty());

        // No volumes at all: mid-scan, before the new index is installed.
        assert!(build_rows(&[], &h, 0, 10).is_empty());
        assert_eq!(summarize(&[], &h, &[0, 1]).count, 0);
        assert_eq!(summarize_all(&[], &h).bytes, 0);

        // A volume set that exists but is smaller than the ids refer to -
        // scanning a different, smaller drive.
        let small = small_index();
        let last = crate::model::index::NodeId::new(index.len() as u32 - 1);
        let stale = vec![(0u16, last)];
        assert!(small.len() < index.len());
        assert!(build_rows(&[&small], &stale, 0, 10).is_empty());
        assert_eq!(summarize(&[&small], &stale, &[0]).count, 0);
    }

    #[test]
    fn rows_carry_preformatted_display_strings() {
        let index = index();
        let mut h = hits(&index);
        sort_hits(
            &[&index],
            &mut h,
            Sort {
                key: SortKey::Name,
                ascending: true,
            },
        );

        let rows = build_rows(&[&index], &h, 0, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "Alpha.pdf");
        assert_eq!(rows[0].dir_path, "C:");
        assert_eq!(rows[0].size_display, "300 B");
        assert_eq!(rows[0].extension, "pdf");
        assert_eq!(rows[0].kind_label, "Document");
        assert!(!rows[0].is_directory);

        // Windows clamp instead of panicking.
        assert_eq!(build_rows(&[&index], &h, 2, 10).len(), 1);
        assert!(build_rows(&[&index], &h, 99, 10).is_empty());
    }

    #[test]
    fn selection_summary_sums_what_is_picked() {
        let index = index();
        let mut h = hits(&index);
        sort_hits(
            &[&index],
            &mut h,
            Sort {
                key: SortKey::Name,
                ascending: true,
            },
        );

        let s = summarize(&[&index], &h, &[0, 2, 999]);
        assert_eq!(s.count, 2, "the stale pick is skipped");
        assert_eq!(s.bytes, 300 + 200);

        let all = summarize_all(&[&index], &h);
        assert_eq!(all.count, 3);
        assert_eq!(all.bytes, 600);
        assert_eq!(all.allocated, 300);
    }

    #[test]
    fn sizes_and_dates_format_for_display() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(5 << 30), "5.0 GiB");

        assert_eq!(format_mtime(0), "", "unknown stays blank, not 1970");
        assert_eq!(format_mtime(86_400_000_000_000), "1970-01-02 00:00");
    }
}
