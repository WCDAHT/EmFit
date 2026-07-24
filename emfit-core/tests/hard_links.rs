//! Hard links: every name appears, the bytes are counted once.
//!
//! A hard-linked file is one blob of data reachable through several paths.
//! Both facts have to survive into the index, and they pull in opposite
//! directions:
//!
//! - Drop the extra names and the file is missing from most of its locations.
//! - Keep them naively and a WinSxS-heavy volume reports several times the
//!   disk it actually uses.
//!
//! The resolution — the same one WizTree reaches — is that every name is a
//! real entry with the file's logical size, while exactly one of them carries
//! the allocated bytes. `total_size` then answers "how much file is listed
//! here" and `total_allocated` answers "how much disk does this cost", and
//! both are true.
//!
//! These tests drive `IndexBuilder` directly with the entries a scanner would
//! produce, so they check the contract rather than the NTFS parsing.

use std::ops::ControlFlow;

use emfit_core::model::builder::IndexBuilder;
use emfit_core::model::caps::VolumeCaps;
use emfit_core::model::entry::{EntryFlags, RawEntry, Times};
use emfit_core::model::index::Index;
use emfit_core::model::sink::EntrySink;
use emfit_core::service::task::CancellationToken;

const ROOT: u64 = 5;
/// The one MFT record all four names in the fixture describe.
const SHARED_RECORD: u64 = 200;
const SIZE: u64 = 100 * 1024 * 1024;

fn caps() -> VolumeCaps {
    VolumeCaps {
        has_stable_ids: true,
        has_hard_links: true,
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

/// The link that owns the bytes.
fn owner(parent_id: u64, name: &str) -> RawEntry<'_> {
    RawEntry {
        fs_id: SHARED_RECORD,
        parent_id,
        name,
        size: SIZE,
        allocated: SIZE,
        times: Times::default(),
        flags: EntryFlags::empty(),
    }
}

/// An additional name for the same file: full logical size, no bytes.
fn alias(index: u64, parent_id: u64, name: &str) -> RawEntry<'_> {
    RawEntry {
        fs_id: SHARED_RECORD | (index << 48),
        parent_id,
        name,
        size: SIZE,
        allocated: 0,
        times: Times::default(),
        flags: EntryFlags::ALIAS,
    }
}

/// `C:\hltest` holding one 100 MiB file under four names — the fixture the
/// WizTree comparison was run against.
fn build_fixture() -> Index {
    let mut builder = IndexBuilder::new(caps(), CancellationToken::new());
    let entries = vec![
        dir(ROOT, ROOT, ""),
        dir(100, ROOT, "hltest"),
        owner(100, "original.bin"),
        alias(1, 100, "link1.bin"),
        alias(2, 100, "link2.bin"),
        alias(3, 100, "link3.bin"),
    ];
    assert_eq!(builder.push_batch(&entries), ControlFlow::Continue(()));
    let (index, warnings) = builder.finish();
    assert!(warnings.is_empty(), "{warnings:?}");
    index
}

fn at(index: &Index, path: &str) -> emfit_core::model::index::NodeId {
    index
        .ids()
        .find(|&id| index.path(id) == path)
        .unwrap_or_else(|| panic!("no node at {path}"))
}

#[test]
fn every_name_appears_as_its_own_path() {
    let index = build_fixture();

    for name in ["original.bin", "link1.bin", "link2.bin", "link3.bin"] {
        let path = format!(r"C:\hltest\{name}");
        let id = at(&index, &path);
        assert_eq!(
            index.node(id).size(),
            SIZE,
            "{name} is a real 100 MiB file by any name"
        );
    }

    let folder = at(&index, r"C:\hltest");
    assert_eq!(index.children(folder).len(), 4);
}

#[test]
fn the_folder_lists_four_files_but_costs_one() {
    // The pair of numbers WizTree shows for this folder: 400 MB of files
    // occupying 100 MB of disk.
    let index = build_fixture();
    let folder = index.node(at(&index, r"C:\hltest"));

    assert_eq!(folder.file_count(), 4);
    assert_eq!(folder.total_size(), 4 * SIZE, "four listed files");
    assert_eq!(
        folder.total_allocated(),
        SIZE,
        "one file's worth of disk — counting each link would quadruple it"
    );
}

#[test]
fn the_volume_total_is_not_inflated_by_links() {
    let index = build_fixture();
    let root = index.node(index.root());

    assert_eq!(
        root.total_allocated(),
        SIZE,
        "a WinSxS-heavy volume would otherwise report several times its size"
    );
}

#[test]
fn aliases_are_identifiable_and_the_owner_is_not() {
    // What lets the UI render the non-owning links' size in parentheses.
    let index = build_fixture();

    assert!(!index.node(at(&index, r"C:\hltest\original.bin")).is_alias());
    for name in ["link1.bin", "link2.bin", "link3.bin"] {
        let node = index.node(at(&index, &format!(r"C:\hltest\{name}")));
        assert!(node.is_alias(), "{name}");
        assert_eq!(
            node.allocated(),
            0,
            "{name} reports no bytes because they are counted under the owner"
        );
    }
}

#[test]
fn every_link_reports_the_one_record_it_describes() {
    // The alias ids are distinct so the builder can key on them, but each must
    // still identify the single file on disk — that is what open-by-id and
    // change tracking rely on.
    let index = build_fixture();

    for name in ["original.bin", "link1.bin", "link2.bin", "link3.bin"] {
        let id = at(&index, &format!(r"C:\hltest\{name}"));
        assert_eq!(
            index.native_id(id),
            Some(SHARED_RECORD),
            "{name} must point at record {SHARED_RECORD}"
        );
    }
}

#[test]
fn links_in_different_directories_each_get_their_own_path() {
    // The real shape of the problem: the same file in WinSxS and in System32.
    let mut builder = IndexBuilder::new(caps(), CancellationToken::new());
    let entries = vec![
        dir(ROOT, ROOT, ""),
        dir(100, ROOT, "WinSxS"),
        dir(101, ROOT, "System32"),
        owner(100, "shared.dll"),
        alias(1, 101, "shared.dll"),
    ];
    assert_eq!(builder.push_batch(&entries), ControlFlow::Continue(()));
    let (index, _) = builder.finish();

    assert_eq!(index.node(at(&index, r"C:\WinSxS\shared.dll")).size(), SIZE);
    assert_eq!(
        index.node(at(&index, r"C:\System32\shared.dll")).size(),
        SIZE
    );

    // Each directory lists the file, but only one of them pays for it.
    let winsxs = index.node(at(&index, r"C:\WinSxS"));
    let system32 = index.node(at(&index, r"C:\System32"));
    assert_eq!(winsxs.total_size(), SIZE);
    assert_eq!(system32.total_size(), SIZE);
    assert_eq!(winsxs.total_allocated() + system32.total_allocated(), SIZE);

    assert_eq!(index.node(index.root()).total_allocated(), SIZE);
}

#[test]
fn a_file_with_one_name_is_untouched() {
    // The overwhelmingly common case must not pay for any of this.
    let mut builder = IndexBuilder::new(caps(), CancellationToken::new());
    let entries = vec![dir(ROOT, ROOT, ""), owner(ROOT, "solo.bin")];
    assert_eq!(builder.push_batch(&entries), ControlFlow::Continue(()));
    let (index, _) = builder.finish();

    let node = index.node(at(&index, r"C:\solo.bin"));
    assert!(!node.is_alias());
    assert_eq!(node.size(), SIZE);
    assert_eq!(node.allocated(), SIZE);
    assert_eq!(index.node(index.root()).total_allocated(), SIZE);
}
