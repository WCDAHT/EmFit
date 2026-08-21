//! File-type breakdown: what kind of thing is eating the disk
//! (features.md sec 4.3, WizTree's "File Types" tab).
//!
//! One pass over the flat node array, aggregating by extension. Hard-link
//! aliases contribute their *count* (they are real paths) but zero allocated
//! bytes, so the byte totals stay honest; synthetic rows (free space) are
//! excluded entirely.

use std::collections::HashMap;

use crate::model::index::Index;
use crate::service::filetype::{FileKind, extension_of};
use crate::service::view::human_size;

/// One extension's share of the volume(s).
#[derive(Debug, Clone, PartialEq)]
pub struct TypeRow {
    /// Lower-cased extension without the dot; `""` renders as "(none)".
    pub extension: String,
    pub kind_label: String,
    /// `--category-N` slot, 0 = neutral.
    pub category: u8,
    pub count: u64,
    pub size: u64,
    pub allocated: u64,
    pub size_display: String,
    pub allocated_display: String,
    /// Share of all files' allocated bytes, 0..=100.
    pub percent: f32,
}

/// Aggregate every file by extension, largest allocation first. At most
/// `limit` rows come back; the tail is folded into one `(other)` row so the
/// table never lies by omission.
pub fn by_extension(indices: &[&Index], limit: usize) -> Vec<TypeRow> {
    let mut buckets: HashMap<String, (u64, u64, u64)> = HashMap::new();

    for index in indices {
        for id in index.ids() {
            let node = index.node(id);
            if node.is_directory() || node.is_synthetic() {
                continue;
            }
            let ext = extension_of(index.name(id)).to_ascii_lowercase();
            let bucket = buckets.entry(ext).or_default();
            bucket.0 += 1;
            bucket.1 += node.size();
            bucket.2 += node.allocated();
        }
    }

    let total_allocated: u64 = buckets.values().map(|&(_, _, alloc)| alloc).sum();
    let mut rows: Vec<(String, (u64, u64, u64))> = buckets.into_iter().collect();
    rows.sort_by(|a, b| (b.1).2.cmp(&(a.1).2).then_with(|| a.0.cmp(&b.0)));

    let mut out: Vec<TypeRow> = Vec::with_capacity(limit.min(rows.len()) + 1);
    let mut other = (0u64, 0u64, 0u64);
    for (i, (ext, (count, size, allocated))) in rows.into_iter().enumerate() {
        if i < limit {
            out.push(row(ext, count, size, allocated, total_allocated));
        } else {
            other.0 += count;
            other.1 += size;
            other.2 += allocated;
        }
    }
    if other.0 > 0 {
        let mut folded = row(String::new(), other.0, other.1, other.2, total_allocated);
        folded.extension = "(other)".to_string();
        folded.kind_label = "Other".to_string();
        folded.category = 0;
        out.push(folded);
    }
    out
}

fn row(extension: String, count: u64, size: u64, allocated: u64, total: u64) -> TypeRow {
    // Classify from a representative name; the kind table keys off the
    // extension alone.
    let kind = FileKind::classify(&format!("x.{extension}"), false);
    TypeRow {
        kind_label: kind.label().to_string(),
        category: kind.category_slot(),
        count,
        size,
        allocated,
        size_display: human_size(size),
        allocated_display: human_size(allocated),
        percent: if total > 0 {
            (allocated as f64 / total as f64 * 100.0) as f32
        } else {
            0.0
        },
        extension,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::builder::IndexBuilder;
    use crate::model::entry::{EntryFlags, RawEntry, Times};
    use crate::model::sink::EntrySink;
    use crate::service::task::CancellationToken;

    fn entry(fs_id: u64, name: &str, allocated: u64, flags: EntryFlags) -> RawEntry<'_> {
        RawEntry {
            fs_id,
            parent_id: 5,
            name,
            size: allocated,
            allocated: if flags.is_alias() { 0 } else { allocated },
            times: Times::default(),
            flags,
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
            entry(10, "a.mp4", 6000, EntryFlags::empty()),
            entry(11, "b.MP4", 2000, EntryFlags::empty()),
            entry(12, "c.pdf", 1500, EntryFlags::empty()),
            entry(13, "README", 500, EntryFlags::empty()),
            entry(14 | (1 << 48), "a-link.mp4", 6000, EntryFlags::ALIAS),
            entry(15, "Free space", 4000, EntryFlags::SYNTHETIC),
        ]);
        b.finish().0
    }

    #[test]
    fn extensions_aggregate_case_insensitively_and_sort_by_bytes() {
        let index = index();
        let rows = by_extension(&[&index], 50);

        assert_eq!(rows[0].extension, "mp4");
        assert_eq!(rows[0].count, 3, "the hard link is a real path");
        assert_eq!(rows[0].allocated, 8000, "but its bytes count once");
        assert_eq!(rows[0].kind_label, "Video");

        assert_eq!(rows[1].extension, "pdf");
        let none = rows.iter().find(|r| r.extension.is_empty()).unwrap();
        assert_eq!(none.count, 1, "README has no extension");

        assert!(
            !rows.iter().any(|r| r.allocated == 4000 && r.count == 1),
            "the synthetic free-space row is not a file type"
        );
    }

    #[test]
    fn percentages_are_of_all_files() {
        let index = index();
        let rows = by_extension(&[&index], 50);
        let total: f32 = rows.iter().map(|r| r.percent).sum();
        assert!((total - 100.0).abs() < 0.5, "shares sum to {total}");
        assert!((rows[0].percent - 80.0).abs() < 0.5, "mp4 is 8000 of 10000");
    }

    #[test]
    fn the_tail_folds_into_other() {
        let index = index();
        let rows = by_extension(&[&index], 1);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].extension, "mp4");
        assert_eq!(rows[1].extension, "(other)");
        assert_eq!(rows[1].count, 2);
        assert_eq!(rows[1].allocated, 2000);
    }
}
