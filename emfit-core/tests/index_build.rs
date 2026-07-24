//! Index construction, through the public API only.
//!
//! Covers M0's exit criteria: parents resolved regardless of arrival order,
//! CSR children correct, rollup verified against hand-computed totals on a
//! deep chain, a wide directory, and unreachable entries.

use std::ops::ControlFlow;

use emfit_core::model::builder::{EntrySink, IndexBuilder, ORPHAN_FOLDER_NAME, ScanWarning};
use emfit_core::model::caps::VolumeCaps;
use emfit_core::model::entry::{EntryFlags, RawEntry, Times};
use emfit_core::model::index::{Index, NodeId};
use emfit_core::service::task::CancellationToken;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

const ROOT_FS: u64 = 5; // NTFS's own root record number, for familiarity

fn caps() -> VolumeCaps {
    VolumeCaps {
        root_label: "C:".to_string(),
        path_separator: '\\',
        ..VolumeCaps::default()
    }
}

fn dir(fs_id: u64, parent_id: u64, name: &str) -> RawEntry<'_> {
    RawEntry {
        fs_id,
        parent_id,
        name,
        size: 0,
        allocated: 0,
        times: Times::default(),
        flags: EntryFlags::DIRECTORY,
    }
}

fn file(fs_id: u64, parent_id: u64, name: &str, size: u64) -> RawEntry<'_> {
    RawEntry {
        fs_id,
        parent_id,
        name,
        size,
        allocated: size.div_ceil(4096) * 4096,
        times: Times::default(),
        flags: EntryFlags::empty(),
    }
}

fn root_entry() -> RawEntry<'static> {
    dir(ROOT_FS, ROOT_FS, "")
}

/// Build from one batch, as a scanner that read everything at once would.
fn build(entries: &[RawEntry<'_>]) -> (Index, Vec<ScanWarning>) {
    let mut builder = IndexBuilder::new(caps(), CancellationToken::new());
    assert_eq!(builder.push_batch(entries), ControlFlow::Continue(()));
    builder.finish()
}

/// Find a node by path. Panics if absent, so tests read as assertions.
fn at(index: &Index, path: &str) -> NodeId {
    index
        .ids()
        .find(|&id| index.path(id) == path)
        .unwrap_or_else(|| panic!("no node at {path}"))
}

// ---------------------------------------------------------------------------
// arrival order
// ---------------------------------------------------------------------------

#[test]
fn resolves_parents_when_children_arrive_before_their_parent() {
    // MFT order is effectively random: a file can be pushed long before the
    // directory containing it.
    let (index, warnings) = build(&[
        file(300, 200, "deep.txt", 10),
        dir(200, 100, "inner"),
        dir(100, ROOT_FS, "outer"),
        root_entry(),
    ]);

    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    assert_eq!(
        index.path(at(&index, r"C:\outer\inner\deep.txt")),
        r"C:\outer\inner\deep.txt"
    );
    assert_eq!(index.node(index.root()).total_size(), 10);
}

#[test]
fn arrival_order_does_not_change_the_result() {
    let forward = [
        root_entry(),
        dir(100, ROOT_FS, "a"),
        dir(200, 100, "b"),
        file(300, 200, "f.txt", 42),
    ];
    let reversed: Vec<_> = forward.iter().copied().rev().collect();

    let (a, _) = build(&forward);
    let (b, _) = build(&reversed);

    assert_eq!(a.len(), b.len());
    for path in [r"C:\a", r"C:\a\b", r"C:\a\b\f.txt"] {
        let na = a.node(at(&a, path));
        let nb = b.node(at(&b, path));
        assert_eq!(na.total_size(), nb.total_size(), "{path}");
        assert_eq!(na.file_count(), nb.file_count(), "{path}");
        assert_eq!(na.child_count(), nb.child_count(), "{path}");
    }
}

#[test]
fn entries_split_across_batches_resolve_across_the_boundary() {
    let mut builder = IndexBuilder::new(caps(), CancellationToken::new());
    // The child's parent arrives in a later batch entirely.
    assert_eq!(
        builder.push_batch(&[file(300, 100, "late.txt", 7)]),
        ControlFlow::Continue(())
    );
    assert_eq!(
        builder.push_batch(&[root_entry(), dir(100, ROOT_FS, "docs")]),
        ControlFlow::Continue(())
    );
    let (index, warnings) = builder.finish();

    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    assert_eq!(
        index.path(at(&index, r"C:\docs\late.txt")),
        r"C:\docs\late.txt"
    );
}

// ---------------------------------------------------------------------------
// children (CSR)
// ---------------------------------------------------------------------------

#[test]
fn children_are_grouped_under_the_right_parent() {
    let (index, _) = build(&[
        root_entry(),
        dir(100, ROOT_FS, "a"),
        dir(200, ROOT_FS, "b"),
        file(300, 100, "in_a.txt", 1),
        file(400, 200, "in_b.txt", 2),
        file(500, 200, "also_b.txt", 3),
    ]);

    let a = at(&index, r"C:\a");
    let b = at(&index, r"C:\b");

    assert_eq!(index.children(a).len(), 1);
    assert_eq!(index.children(b).len(), 2);
    assert_eq!(index.children(index.root()).len(), 2);

    let mut b_names: Vec<&str> = index.children(b).iter().map(|&c| index.name(c)).collect();
    b_names.sort_unstable();
    assert_eq!(b_names, ["also_b.txt", "in_b.txt"]);

    // Every child points back at the parent whose run it sits in.
    for &child in index.children(b) {
        assert_eq!(index.node(child).parent(), b);
    }
}

#[test]
fn root_is_not_its_own_child() {
    let (index, _) = build(&[root_entry(), file(100, ROOT_FS, "f.txt", 1)]);
    let root = index.root();
    assert_eq!(index.node(root).parent(), root, "root parents itself");
    assert!(!index.children(root).contains(&root));
    assert_eq!(index.children(root).len(), 1);
}

#[test]
fn a_wide_directory_holds_every_child() {
    const WIDE: u64 = 10_000;

    let names: Vec<String> = (0..WIDE).map(|i| format!("f{i:05}.bin")).collect();
    let mut entries = vec![root_entry(), dir(100, ROOT_FS, "wide")];
    entries.extend(
        names
            .iter()
            .enumerate()
            .map(|(i, name)| file(1000 + i as u64, 100, name, 1024)),
    );

    let (index, warnings) = build(&entries);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    let wide = at(&index, r"C:\wide");
    assert_eq!(index.children(wide).len(), WIDE as usize);
    assert_eq!(index.node(wide).file_count(), WIDE as u32);
    assert_eq!(index.node(wide).total_size(), WIDE * 1024);
}

// ---------------------------------------------------------------------------
// rollup
// ---------------------------------------------------------------------------

#[test]
fn rollup_matches_hand_computed_totals() {
    //  C:\                        3 dirs (incl. root), 3 files, 1110 bytes
    //  ├── docs\                  1000 + 100 = 1100
    //  │   ├── big.bin   1000
    //  │   └── small.txt  100
    //  └── tmp\                   10
    //      └── log.txt     10
    let (index, warnings) = build(&[
        root_entry(),
        dir(100, ROOT_FS, "docs"),
        dir(200, ROOT_FS, "tmp"),
        file(300, 100, "big.bin", 1000),
        file(400, 100, "small.txt", 100),
        file(500, 200, "log.txt", 10),
    ]);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    let docs = index.node(at(&index, r"C:\docs"));
    assert_eq!(docs.total_size(), 1100);
    assert_eq!(docs.file_count(), 2);
    assert_eq!(docs.dir_count(), 1); // itself

    let tmp = index.node(at(&index, r"C:\tmp"));
    assert_eq!(tmp.total_size(), 10);
    assert_eq!(tmp.file_count(), 1);

    let root = index.node(index.root());
    assert_eq!(root.total_size(), 1110);
    assert_eq!(root.file_count(), 3);
    assert_eq!(root.dir_count(), 3); // root + docs + tmp

    // A file's own total is just itself.
    let big = index.node(at(&index, r"C:\docs\big.bin"));
    assert_eq!(big.total_size(), 1000);
    assert_eq!(big.file_count(), 1);
    assert_eq!(big.dir_count(), 0);
}

#[test]
fn rollup_carries_through_a_deep_chain() {
    // 500 nested directories with one file at the bottom. Also proves the
    // traversal is iterative — a recursive rollup would risk the stack here.
    const DEPTH: u64 = 500;

    let names: Vec<String> = (0..DEPTH).map(|i| format!("d{i}")).collect();
    let mut entries = vec![root_entry()];
    for (i, name) in names.iter().enumerate() {
        let fs_id = 100 + i as u64;
        let parent = if i == 0 { ROOT_FS } else { fs_id - 1 };
        entries.push(dir(fs_id, parent, name));
    }
    entries.push(file(9999, 100 + DEPTH - 1, "bottom.txt", 777));

    let (index, warnings) = build(&entries);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    // Every level carries the file's bytes.
    let mut expected_path = String::from("C:");
    for name in &names {
        expected_path.push('\\');
        expected_path.push_str(name);
        let node = index.node(at(&index, &expected_path));
        assert_eq!(node.total_size(), 777, "at {expected_path}");
        assert_eq!(node.file_count(), 1, "at {expected_path}");
    }

    let root = index.node(index.root());
    assert_eq!(root.total_size(), 777);
    assert_eq!(root.dir_count(), DEPTH as u32 + 1);
}

#[test]
fn allocated_size_rolls_up_separately_from_logical_size() {
    // 1-byte files each occupy a full 4 KiB cluster.
    let (index, _) = build(&[
        root_entry(),
        dir(100, ROOT_FS, "many"),
        file(200, 100, "a", 1),
        file(300, 100, "b", 1),
    ]);

    let many = index.node(at(&index, r"C:\many"));
    assert_eq!(many.total_size(), 2);
    assert_eq!(many.total_allocated(), 8192);
}

// ---------------------------------------------------------------------------
// unreachable entries
// ---------------------------------------------------------------------------

#[test]
fn entries_with_a_missing_parent_go_to_the_synthetic_folder() {
    // 999 was never pushed — the directory containing orphan.txt is unknown.
    let (index, warnings) = build(&[
        root_entry(),
        dir(100, ROOT_FS, "real"),
        file(300, 999, "orphan.txt", 64),
    ]);

    let orphan = at(&index, &format!(r"C:\{ORPHAN_FOLDER_NAME}\orphan.txt"));
    let folder = index.node(orphan).parent();

    assert!(
        index.node(folder).is_synthetic(),
        "folder must be flagged synthetic"
    );
    assert!(index.node(folder).is_directory());
    assert_eq!(index.name(folder), ORPHAN_FOLDER_NAME);
    assert_eq!(index.node(folder).parent(), index.root());
    assert_eq!(
        index.native_id(folder),
        None,
        "synthetic nodes have no native id"
    );

    // The real tree is untouched, and the bytes are still counted once.
    assert_eq!(index.children(at(&index, r"C:\real")).len(), 0);
    assert_eq!(index.node(index.root()).total_size(), 64);

    assert!(
        warnings.contains(&ScanWarning::OrphansCollected { count: 1 }),
        "expected OrphansCollected, got {warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| matches!(w, ScanWarning::MissingParent { parent_id: 999, .. })),
        "expected MissingParent, got {warnings:?}"
    );
}

#[test]
fn a_parent_cycle_is_collected_rather_than_looping() {
    // 100 -> 200 -> 100. Neither reaches the root.
    let (index, warnings) = build(&[
        root_entry(),
        dir(100, 200, "loop_a"),
        dir(200, 100, "loop_b"),
        file(300, 100, "trapped.txt", 5),
    ]);

    assert!(
        warnings
            .iter()
            .any(|w| matches!(w, ScanWarning::OrphansCollected { count: 3 })),
        "cycle members and their descendants are unreachable: {warnings:?}"
    );
    assert!(
        !warnings
            .iter()
            .any(|w| matches!(w, ScanWarning::UnreachableNodes { .. })),
        "collection must leave everything reachable: {warnings:?}"
    );

    // Everything is reachable from the root once collected.
    let folder = at(&index, &format!(r"C:\{ORPHAN_FOLDER_NAME}"));
    assert_eq!(index.children(folder).len(), 3);
    assert_eq!(index.node(index.root()).total_size(), 5);
}

#[test]
fn no_synthetic_folder_when_every_entry_is_reachable() {
    let (index, warnings) = build(&[root_entry(), file(100, ROOT_FS, "f.txt", 1)]);

    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    assert_eq!(index.len(), 2, "no extra node invented");
    assert!(index.ids().all(|id| !index.node(id).is_synthetic()));
}

#[test]
fn a_scan_with_no_root_gets_one_synthesized() {
    // A subtree scan that never pushed the volume root.
    let (index, warnings) = build(&[dir(100, 999, "stray"), file(200, 100, "f.txt", 3)]);

    assert!(
        warnings.contains(&ScanWarning::SynthesizedRoot),
        "expected SynthesizedRoot, got {warnings:?}"
    );
    assert!(index.node(index.root()).is_synthetic());
    assert_eq!(index.node(index.root()).total_size(), 3);
    assert_eq!(index.path(index.root()), "C:");
}

// ---------------------------------------------------------------------------
// identity, paths, cancellation
// ---------------------------------------------------------------------------

#[test]
fn native_ids_are_preserved_and_node_ids_are_independent_of_them() {
    let (index, _) = build(&[
        root_entry(),
        file(4_000_000, ROOT_FS, "sparse_id.txt", 1),
        file(7, ROOT_FS, "low_id.txt", 1),
    ]);

    let big = at(&index, r"C:\sparse_id.txt");
    let small = at(&index, r"C:\low_id.txt");

    assert_eq!(index.native_id(big), Some(4_000_000));
    assert_eq!(index.native_id(small), Some(7));
    // NodeIds are dense arrival order, unrelated to the filesystem's numbering.
    assert!(big.index() < index.len());
    assert!(small.index() < index.len());
}

#[test]
fn native_ids_are_absent_when_the_volume_has_none() {
    let caps = VolumeCaps {
        has_stable_ids: false,
        root_label: "C:".to_string(),
        ..VolumeCaps::default()
    };
    let mut builder = IndexBuilder::new(caps, CancellationToken::new());
    assert_eq!(
        builder.push_batch(&[root_entry(), file(100, ROOT_FS, "f.txt", 1)]),
        ControlFlow::Continue(())
    );
    let (index, _) = builder.finish();

    assert_eq!(index.native_id(at(&index, r"C:\f.txt")), None);
}

#[test]
fn paths_follow_the_volume_not_a_hardcoded_separator() {
    let caps = VolumeCaps {
        path_separator: '/',
        root_label: "/".to_string(),
        case_sensitive: true,
        ..VolumeCaps::default()
    };
    let mut builder = IndexBuilder::new(caps, CancellationToken::new());
    assert_eq!(
        builder.push_batch(&[
            dir(2, 2, ""), // ext4's root inode
            dir(100, 2, "home"),
            file(200, 100, "notes.md", 12),
        ]),
        ControlFlow::Continue(())
    );
    let (index, _) = builder.finish();

    assert_eq!(index.path(at(&index, "/home/notes.md")), "/home/notes.md");
    assert_eq!(index.path(index.root()), "/");
}

#[test]
fn ancestors_walk_up_to_the_root() {
    let (index, _) = build(&[
        root_entry(),
        dir(100, ROOT_FS, "a"),
        dir(200, 100, "b"),
        file(300, 200, "f.txt", 1),
    ]);

    let f = at(&index, r"C:\a\b\f.txt");
    let names: Vec<&str> = index.ancestors(f).map(|id| index.name(id)).collect();
    assert_eq!(names, ["b", "a", ""]);
    assert_eq!(index.ancestors(index.root()).count(), 0);
}

#[test]
fn a_cancelled_builder_stops_accepting_entries() {
    let cancel = CancellationToken::new();
    let mut builder = IndexBuilder::new(caps(), cancel.clone());

    assert_eq!(
        builder.push_batch(&[root_entry(), file(100, ROOT_FS, "kept.txt", 1)]),
        ControlFlow::Continue(())
    );

    cancel.cancel();
    assert_eq!(
        builder.push_batch(&[file(200, ROOT_FS, "dropped.txt", 1)]),
        ControlFlow::Break(()),
        "a cancelled sink must tell the scanner to stop"
    );

    // What arrived before cancellation still builds into a usable index.
    let (index, _) = builder.finish();
    assert_eq!(index.len(), 2);
    assert_eq!(index.node(index.root()).file_count(), 1);
}
