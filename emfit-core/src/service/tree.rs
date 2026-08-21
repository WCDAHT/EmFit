//! Folder-tree rows: the WizTree-style hierarchical pane (features.md sec 4.1).
//!
//! Children are materialized **lazily from the CSR ranges** - the frontend
//! asks for one directory's children when it expands, and gets rows sorted
//! largest-first with everything the pane renders: sizes, percent-of-parent
//! for the inline proportional bar, and subtree counts. Expanding a 100k-child
//! directory costs one sort of 100k ids, no tree walk.

use crate::model::index::{Index, NodeId};
use crate::service::view::{format_mtime, human_size};

/// One row of the folder tree.
#[derive(Debug, Clone, PartialEq)]
pub struct TreeRow {
    pub vol: u16,
    pub id: u32,
    pub name: String,
    pub is_dir: bool,
    pub synthetic: bool,
    /// Subtree logical / allocated bytes (own bytes, for files).
    pub size: u64,
    pub allocated: u64,
    pub size_display: String,
    pub allocated_display: String,
    /// Share of the parent's allocated bytes, 0..=100 - drives the inline bar.
    pub percent_of_parent: f32,
    pub files: u32,
    pub dirs: u32,
    /// `files + dirs` for a directory (its Items column); 0 for a file.
    pub items: u32,
    /// Raw mtime, nanoseconds since the Unix epoch (0 = unknown) - the sort
    /// key behind `modified_display`.
    pub modified: i64,
    pub modified_display: String,
    /// Compact attribute letters, WizTree-style: `H` hidden, `S` system,
    /// `R` reparse/junction, `C` compressed, `P` sparse, `L` hard-link
    /// alias. Empty when none apply.
    pub attributes: String,
    /// Whether an expand arrow makes sense.
    pub has_children: bool,
}

/// The attribute letters for one node's flags.
fn attributes_of(flags: crate::model::entry::EntryFlags) -> String {
    use crate::model::entry::EntryFlags as F;
    let mut out = String::new();
    for (bit, letter) in [
        (F::HIDDEN, 'H'),
        (F::SYSTEM, 'S'),
        (F::REPARSE, 'R'),
        (F::COMPRESSED, 'C'),
        (F::SPARSE, 'P'),
        (F::ALIAS, 'L'),
    ] {
        if flags.contains(bit) {
            out.push(letter);
        }
    }
    out
}

/// One row per scanned volume: the tree's top level.
pub fn roots(indices: &[&Index]) -> Vec<TreeRow> {
    indices
        .iter()
        .enumerate()
        .map(|(slot, index)| {
            let root = index.root();
            let mut row = row_for(index, slot as u16, root, None);
            row.name = index.caps().root_label.clone();
            row
        })
        .collect()
}

/// One directory's children, largest allocation first (directories and files
/// mixed, as WizTree lists them).
pub fn children(indices: &[&Index], vol: u16, id: u32) -> Vec<TreeRow> {
    let Some(index) = indices.get(vol as usize) else {
        return Vec::new();
    };
    let parent = NodeId::new(id);
    let parent_alloc = effective_alloc(index, parent);

    let mut ids: Vec<NodeId> = index.children(parent).to_vec();
    ids.sort_by_key(|&child| std::cmp::Reverse(effective_alloc(index, child)));

    ids.into_iter()
        .map(|child| row_for(index, vol, child, Some(parent_alloc)))
        .collect()
}

/// Node ids from the root down to `id`, inclusive - what a "reveal in tree"
/// needs to expand.
pub fn lineage(indices: &[&Index], vol: u16, id: u32) -> Vec<u32> {
    let Some(index) = indices.get(vol as usize) else {
        return Vec::new();
    };
    let target = NodeId::new(id);
    let mut chain: Vec<u32> = index.ancestors(target).map(NodeId::get).collect();
    chain.reverse(); // ancestors() walks upward; the tree expands downward
    chain.push(id);
    chain
}

fn effective_alloc(index: &Index, id: NodeId) -> u64 {
    let node = index.node(id);
    if node.is_directory() {
        node.total_allocated()
    } else {
        node.allocated()
    }
}

fn row_for(index: &Index, vol: u16, id: NodeId, parent_alloc: Option<u64>) -> TreeRow {
    let node = index.node(id);
    let (size, allocated) = if node.is_directory() {
        (node.total_size(), node.total_allocated())
    } else {
        (node.size(), node.allocated())
    };

    let percent_of_parent = match parent_alloc {
        Some(parent) if parent > 0 => (allocated as f64 / parent as f64 * 100.0) as f32,
        _ => 100.0,
    };

    let files = node.file_count();
    let dirs = node
        .dir_count()
        .saturating_sub(u32::from(node.is_directory()));

    TreeRow {
        vol,
        id: id.get(),
        name: index.name(id).to_string(),
        is_dir: node.is_directory(),
        synthetic: node.is_synthetic(),
        size,
        allocated,
        size_display: human_size(size),
        allocated_display: human_size(allocated),
        percent_of_parent,
        files,
        dirs,
        items: if node.is_directory() {
            files.saturating_add(dirs)
        } else {
            0
        },
        modified: node.times().mtime,
        modified_display: format_mtime(node.times().mtime),
        attributes: attributes_of(node.flags()),
        has_children: node.child_count() > 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::builder::IndexBuilder;
    use crate::model::entry::{EntryFlags, RawEntry, Times};
    use crate::model::sink::EntrySink;
    use crate::service::task::CancellationToken;

    fn file(fs_id: u64, parent: u64, name: &str, bytes: u64) -> RawEntry<'_> {
        RawEntry {
            fs_id,
            parent_id: parent,
            name,
            size: bytes,
            allocated: bytes,
            times: Times {
                mtime: 86_400_000_000_000,
                crtime: 0,
            },
            flags: EntryFlags::empty(),
        }
    }

    fn dir(fs_id: u64, parent: u64, name: &str) -> RawEntry<'_> {
        RawEntry {
            fs_id,
            parent_id: parent,
            name,
            size: 0,
            allocated: 0,
            times: Times::default(),
            flags: EntryFlags::DIRECTORY,
        }
    }

    /// C:\ -> docs{a:600, sub{b:200}}, big.iso:1200
    fn index() -> Index {
        let caps = crate::service::scan::ntfs_caps("C:".to_string());
        let mut b = IndexBuilder::new(caps, CancellationToken::new());
        let _ = b.push_batch(&[
            dir(5, 5, ""),
            dir(16, 5, "docs"),
            file(17, 16, "a.pdf", 600),
            dir(18, 16, "sub"),
            file(19, 18, "b.pdf", 200),
            file(20, 5, "big.iso", 1200),
        ]);
        b.finish().0
    }

    #[test]
    fn roots_carry_the_volume_label_and_totals() {
        let index = index();
        let rows = roots(&[&index]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "C:");
        assert_eq!(rows[0].allocated, 2000);
        assert_eq!(rows[0].files, 3);
        assert_eq!(rows[0].percent_of_parent, 100.0);
        assert!(rows[0].has_children);
    }

    #[test]
    fn children_come_largest_first_with_parent_percentages() {
        let index = index();
        let root_id = index.root().get();
        let rows = children(&[&index], 0, root_id);

        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["big.iso", "docs"]);

        assert_eq!(rows[0].percent_of_parent, 60.0, "1200 of 2000");
        assert_eq!(rows[1].percent_of_parent, 40.0, "800 of 2000");
        assert!(rows[1].is_dir && rows[1].has_children);
        assert!(!rows[0].is_dir && !rows[0].has_children);

        // Subtree counts on the directory row.
        assert_eq!(rows[1].files, 2);
        assert_eq!(rows[1].dirs, 1, "sub, not counting docs itself");
        assert_eq!(rows[1].items, 3, "items = files + folders");
        assert_eq!(rows[0].items, 0, "files carry no Items count");

        // Raw mtime rides along as the sort key; plain entries have no
        // attribute letters.
        assert_eq!(rows[0].modified, 86_400_000_000_000);
        assert_eq!(rows[0].attributes, "");
    }

    #[test]
    fn attribute_letters_reflect_the_flags() {
        let caps = crate::service::scan::ntfs_caps("C:".to_string());
        let mut b = IndexBuilder::new(caps, CancellationToken::new());
        let flagged = RawEntry {
            fs_id: 20,
            parent_id: 5,
            name: "pagefile.sys",
            size: 100,
            allocated: 100,
            times: Times::default(),
            flags: EntryFlags::HIDDEN
                .union(EntryFlags::SYSTEM)
                .union(EntryFlags::COMPRESSED),
        };
        let _ = b.push_batch(&[dir(5, 5, ""), flagged]);
        let index = b.finish().0;

        let rows = children(&[&index], 0, index.root().get());
        assert_eq!(rows[0].attributes, "HSC");
    }

    #[test]
    fn expanding_a_subdirectory_scopes_percentages_to_it() {
        let index = index();
        let docs = index
            .ids()
            .find(|&i| index.name(i) == "docs")
            .unwrap()
            .get();
        let rows = children(&[&index], 0, docs);

        assert_eq!(rows[0].name, "a.pdf");
        assert_eq!(rows[0].percent_of_parent, 75.0, "600 of docs' 800");
        assert_eq!(rows[1].name, "sub");
        assert_eq!(rows[1].percent_of_parent, 25.0);
    }

    #[test]
    fn lineage_runs_root_to_target() {
        let index = index();
        let b = index.ids().find(|&i| index.name(i) == "b.pdf").unwrap();
        let chain = lineage(&[&index], 0, b.get());

        let names: Vec<String> = chain
            .iter()
            .map(|&id| index.name(NodeId::new(id)).to_string())
            .collect();
        assert_eq!(names, vec!["", "docs", "sub", "b.pdf"]);
    }

    #[test]
    fn bad_slots_and_missing_volumes_return_empty() {
        let index = index();
        assert!(children(&[&index], 7, 0).is_empty());
        assert!(lineage(&[&index], 7, 0).is_empty());
    }
}
