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

/// The size a user expects to see beside a row: files show their own bytes,
/// directories their subtree total.
fn effective_size(indices: &[&Index], (vol, id): Hit) -> u64 {
    let node = indices[vol as usize].node(id);
    if node.is_directory() {
        node.total_size()
    } else {
        node.size()
    }
}

fn effective_allocated(indices: &[&Index], (vol, id): Hit) -> u64 {
    let node = indices[vol as usize].node(id);
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
        .map(|&(vol, id)| {
            let index = indices[vol as usize];
            let node = index.node(id);
            let flags = node.flags();
            let name = index.name(id).to_string();
            let kind = FileKind::classify(&name, node.is_directory());
            let size = effective_size(indices, (vol, id));
            let allocated = effective_allocated(indices, (vol, id));

            Row {
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
            }
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
