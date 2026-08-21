//! The scan cache, through the public API only (roadmap M5).
//!
//! The exit criterion these cover: *a scan saved and reloaded is byte-identical
//! in its query results to the original*. That is asserted the strict way -
//! every node, every path, every rollup total, and the results of a battery of
//! queries - because a snapshot that is subtly wrong is worse than no snapshot
//! at all: it looks like a scan.
//!
//! The other half is hostility. A cache file lives in a user-writable
//! directory, and `Index` slices its arena raw, so every structural claim a
//! snapshot makes has to be checked on the way in. These tests corrupt one
//! thing at a time and require a clean refusal, never a panic.

use std::path::PathBuf;

use emfit_core::model::builder::{IndexBuilder, ORPHAN_FOLDER_NAME};
use emfit_core::model::caps::VolumeCaps;
use emfit_core::model::entry::{EntryFlags, RawEntry, Times};
use emfit_core::model::index::{Index, NodeId};
use emfit_core::model::sink::{EntrySink, RecordedEntry};
use emfit_core::service::cache::manifest::{JournalStamp, Manifest, ScanStamp, VolumeStamp};
use emfit_core::service::cache::{Patch, snapshot};
use emfit_core::service::fold::CaseFold;
use emfit_core::service::query::{Query, RawQuery};
use emfit_core::service::search;
use emfit_core::service::task::CancellationToken;
use emfit_core::service::view::{self, SortKey};

const ROOT_FS: u64 = 5;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn caps() -> VolumeCaps {
    VolumeCaps {
        root_label: "C:".to_string(),
        path_separator: '\\',
        has_stable_ids: true,
        ..VolumeCaps::default()
    }
}

fn entry(fs_id: u64, parent_id: u64, name: &str, size: u64, flags: EntryFlags) -> RawEntry<'_> {
    RawEntry {
        fs_id,
        parent_id,
        name,
        size,
        allocated: size.div_ceil(4096) * 4096,
        times: Times {
            mtime: 1_700_000_000_000_000_000 + fs_id as i64,
            crtime: 1_600_000_000_000_000_000 + fs_id as i64,
        },
        flags,
    }
}

fn dir(fs_id: u64, parent_id: u64, name: &str) -> RawEntry<'_> {
    entry(fs_id, parent_id, name, 0, EntryFlags::DIRECTORY)
}

fn file(fs_id: u64, parent_id: u64, name: &str, size: u64) -> RawEntry<'_> {
    entry(fs_id, parent_id, name, size, EntryFlags::empty())
}

/// A volume with every shape the snapshot has to survive: nested directories,
/// a hard-linked file under two names, hidden and system attributes, a
/// non-ASCII name, an entry whose parent never arrived (so the builder invents
/// the orphan folder), and the synthetic free-space row.
fn sample_index() -> Index {
    let mut builder = IndexBuilder::new(caps(), CancellationToken::new());
    let alias = (1u64 << 48) | 30; // a second name for record 30

    let batch = vec![
        dir(ROOT_FS, ROOT_FS, ""),
        dir(16, ROOT_FS, "Users"),
        dir(17, 16, "Admin"),
        file(18, 17, "Report.PDF", 2 << 20),
        entry(19, 17, "notes.txt", 10, EntryFlags::HIDDEN),
        entry(20, ROOT_FS, "pagefile.sys", 4 << 30, EntryFlags::SYSTEM),
        file(21, 17, "r\u{e9}sum\u{e9}.docx", 4096),
        dir(22, ROOT_FS, "Windows"),
        dir(23, 22, "WinSxS"),
        file(30, 23, "shared.dll", 1 << 20),
        entry(alias, 17, "shared.dll", 1 << 20, EntryFlags::ALIAS),
        // Parent 9999 was never pushed: this lands under [Unreachable].
        file(40, 9999, "lost.bin", 512),
        entry(
            u64::MAX,
            ROOT_FS,
            "Free space",
            8 << 30,
            EntryFlags::SYNTHETIC,
        ),
    ];
    let _ = builder.push_batch(&batch);
    builder.finish().0
}

fn manifest(index: &Index) -> Manifest {
    Manifest {
        schema_version: 1,
        app_version: "test".to_string(),
        volume: VolumeStamp {
            fingerprint: "0123456789abcdef".to_string(),
            serial: 0xDEAD_BEEF,
            root_label: "C:".to_string(),
            total_bytes: 512 << 30,
            bytes_per_cluster: 4096,
            free_bytes: 8 << 30,
        },
        caps: index.caps().clone(),
        scan: ScanStamp::now(13_553, "physical", index.len() as u64, 0),
        journal: Some(JournalStamp {
            id: 0x1234,
            next_usn: 4_823_901_184,
        }),
    }
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("emfit-cache-tests");
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join(format!("{name}.emfit"))
}

/// Save, load, and rebuild - the whole cache round trip.
fn round_trip(index: &Index, name: &str, patch: &Patch) -> Index {
    let path = temp(name);
    let orders: Vec<_> = [SortKey::Name, SortKey::Path]
        .iter()
        .map(|&key| (key, view::build_order(index, key)))
        .collect();
    snapshot::save(&path, index, &CaseFold::Simple, &manifest(index), &orders).expect("save");

    let loaded = snapshot::load(&path).expect("load");
    let (rebuilt, _, _) = snapshot::rebuild(
        &loaded.columns,
        index.caps().clone(),
        patch,
        &CancellationToken::new(),
    )
    .expect("rebuild");
    rebuilt
}

/// Everything about a node that a user can observe, as one comparable string.
fn describe(index: &Index, id: NodeId) -> String {
    let node = index.node(id);
    format!(
        "{}|{}|{}|{}|{:#x}|{}|{}|{}|{}|{}|{:?}",
        index.path(id),
        index.name(id),
        node.size(),
        node.allocated(),
        node.flags().bits(),
        node.times().mtime,
        node.times().crtime,
        node.total_size(),
        node.total_allocated(),
        node.file_count(),
        index.native_id(id),
    )
}

/// Every node, in path order so two indexes are comparable even if their node
/// ids differ.
fn describe_all(index: &Index) -> Vec<String> {
    let mut out: Vec<String> = index.ids().map(|id| describe(index, id)).collect();
    out.sort();
    out
}

fn search_results(index: &Index, text: &str) -> Vec<String> {
    let raw = RawQuery {
        text: text.to_string(),
        include_hidden: true,
        include_system: true,
        ..RawQuery::default()
    };
    let query = Query::parse(&raw);
    let fold = CaseFold::Simple;
    let hits = search::search_all(&[(index, &fold)], &query, &CancellationToken::new())
        .expect("search completes");
    let mut out: Vec<String> = hits.iter().map(|&(_, id)| index.path(id)).collect();
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// the round trip
// ---------------------------------------------------------------------------

#[test]
fn a_reloaded_scan_is_the_scan() {
    let original = sample_index();
    let reloaded = round_trip(&original, "identity", &Patch::default());

    assert_eq!(reloaded.len(), original.len(), "node count");
    assert_eq!(
        describe_all(&reloaded),
        describe_all(&original),
        "every node, field for field"
    );
    assert_eq!(
        reloaded.path(reloaded.root()),
        original.path(original.root())
    );
}

#[test]
fn node_ids_survive_a_round_trip_with_no_changes() {
    // The cached sort orders are keyed by node id, so an unchanged reload that
    // renumbered nodes would silently sort the wrong rows.
    let original = sample_index();
    let reloaded = round_trip(&original, "ids", &Patch::default());

    for id in original.ids() {
        assert_eq!(
            describe(&reloaded, id),
            describe(&original, id),
            "node {id:?} moved"
        );
    }
}

#[test]
fn queries_return_the_same_rows_after_a_reload() {
    let original = sample_index();
    let reloaded = round_trip(&original, "queries", &Patch::default());

    for query in [
        "",
        "shared",
        "*.pdf",
        "ext:txt;docx",
        "size:>1mb",
        "folder:",
        "file:",
        "`C:\\Users\\Admin`",
        "r\u{e9}sum\u{e9}",
    ] {
        assert_eq!(
            search_results(&reloaded, query),
            search_results(&original, query),
            "query `{query}`"
        );
    }
}

#[test]
fn the_folders_the_builder_invented_come_back_as_themselves() {
    // A synthesized orphan folder and the free-space row have no filesystem
    // records behind them. If the snapshot did not carry reserved ids for
    // them, reloading would either lose them or collect their children a
    // second time under a nested duplicate.
    let original = sample_index();
    let reloaded = round_trip(&original, "synthetic", &Patch::default());

    let orphans = |index: &Index| -> Vec<String> {
        index
            .ids()
            .filter(|&id| index.name(id) == ORPHAN_FOLDER_NAME)
            .map(|id| index.path(id))
            .collect()
    };
    assert_eq!(orphans(&original).len(), 1, "the sample has one orphan");
    assert_eq!(orphans(&reloaded), orphans(&original));

    let lost = reloaded
        .ids()
        .find(|&id| reloaded.name(id) == "lost.bin")
        .expect("the unreachable file survives");
    assert!(reloaded.path(lost).contains(ORPHAN_FOLDER_NAME));

    let free = reloaded
        .ids()
        .find(|&id| reloaded.name(id) == "Free space")
        .expect("the free-space row survives");
    assert!(reloaded.node(free).is_synthetic());
    assert_eq!(reloaded.native_id(free), None);
}

#[test]
fn hard_links_keep_both_names_and_count_their_bytes_once() {
    let original = sample_index();
    let reloaded = round_trip(&original, "links", &Patch::default());

    let named: Vec<NodeId> = reloaded
        .ids()
        .filter(|&id| reloaded.name(id) == "shared.dll")
        .collect();
    assert_eq!(named.len(), 2, "both links are still there");
    assert_eq!(
        named
            .iter()
            .filter(|&&id| reloaded.node(id).is_alias())
            .count(),
        1,
        "exactly one is the alias"
    );
    assert_eq!(
        reloaded.node(reloaded.root()).total_allocated(),
        original.node(original.root()).total_allocated(),
        "the alias still contributes no bytes"
    );
}

#[test]
fn cached_sort_orders_come_back_usable() {
    let index = sample_index();
    let path = temp("orders");
    let orders: Vec<_> = [SortKey::Name, SortKey::Path]
        .iter()
        .map(|&key| (key, view::build_order(&index, key)))
        .collect();
    snapshot::save(&path, &index, &CaseFold::Simple, &manifest(&index), &orders).expect("save");

    let loaded = snapshot::load(&path).expect("load");
    assert_eq!(loaded.orders.len(), 2);
    for (key, built) in &orders {
        assert_eq!(loaded.orders.get(key), Some(built), "{key:?}");
        assert_eq!(
            view::ranks_from_order(&loaded.orders[key]),
            view::build_ranks(&[&index], *key),
            "{key:?} ranks"
        );
    }
}

#[test]
fn orders_arrive_after_the_snapshot_and_only_once() {
    // The truth is written the moment a scan ends; the sort columns take ten
    // seconds to build and are appended when they exist. A snapshot without
    // them has to be perfectly usable in the meantime - that is the whole
    // reason the two are separated.
    let index = sample_index();
    let path = temp("appended-orders");
    snapshot::save(&path, &index, &CaseFold::Simple, &manifest(&index), &[]).expect("save");

    let loaded = snapshot::load(&path).expect("a truth-only snapshot loads");
    assert!(loaded.orders.is_empty(), "no columns yet");
    assert_eq!(
        loaded.columns.len(),
        index.len(),
        "and all the nodes are there"
    );

    let orders: Vec<_> = [SortKey::Name, SortKey::Path]
        .iter()
        .map(|&key| (key, view::build_order(&index, key)))
        .collect();
    assert_eq!(
        snapshot::append_orders(&path, index.len(), &orders).expect("append"),
        2
    );

    let loaded = snapshot::load(&path).expect("still loads");
    assert_eq!(loaded.orders.len(), 2);
    for (key, built) in &orders {
        assert_eq!(loaded.orders.get(key), Some(built), "{key:?}");
    }
    // The truth has to survive the append untouched.
    let (rebuilt, _, _) = snapshot::rebuild(
        &loaded.columns,
        index.caps().clone(),
        &Patch::default(),
        &CancellationToken::new(),
    )
    .expect("rebuild");
    assert_eq!(describe_all(&rebuilt), describe_all(&index));

    // Idempotent: appending the same columns again changes nothing.
    assert_eq!(
        snapshot::append_orders(&path, index.len(), &orders).expect("append again"),
        0
    );
    assert_eq!(snapshot::load(&path).expect("loads").orders.len(), 2);
}

#[test]
fn orders_for_a_different_scan_are_refused() {
    // A length mismatch means the columns index into some other index; writing
    // them would produce a snapshot whose sort order points at the wrong rows.
    let index = sample_index();
    let path = temp("mismatched-orders");
    snapshot::save(&path, &index, &CaseFold::Simple, &manifest(&index), &[]).expect("save");

    let mut order = view::build_order(&index, SortKey::Name);
    order.pop();
    let orders = vec![(SortKey::Name, order)];
    assert_eq!(
        snapshot::append_orders(&path, index.len(), &orders).expect("append"),
        0
    );
    assert!(snapshot::load(&path).expect("loads").orders.is_empty());
}

#[test]
fn the_case_folding_table_travels_with_the_snapshot() {
    // Loading must not need the volume: search folds names with the table the
    // filesystem itself compares with, and re-reading $UpCase would mean
    // opening the device again.
    let index = sample_index();
    let path = temp("upcase");
    let table: Vec<u16> = (0..0x1_0000u32)
        .map(|c| char::from_u32(c).map_or(c as u16, |c| c.to_ascii_uppercase() as u16))
        .collect();
    let fold = CaseFold::from_upcase(table);
    assert!(
        matches!(fold, CaseFold::Table(_)),
        "the sample table is valid"
    );

    snapshot::save(&path, &index, &fold, &manifest(&index), &[]).expect("save");
    let loaded = snapshot::load(&path).expect("load");
    assert!(
        matches!(loaded.fold, CaseFold::Table(_)),
        "the table came back, not the fallback rule"
    );
}

// ---------------------------------------------------------------------------
// patching, as a replay would
// ---------------------------------------------------------------------------

#[test]
fn a_patch_replaces_deletes_and_adds_exactly_what_it_names() {
    let original = sample_index();

    let mut patch = Patch::default();
    // Record 19 (notes.txt) grew and lost its hidden flag; record 18
    // (Report.PDF) was deleted; record 50 is new.
    patch.replace.insert(19);
    patch.replace.insert(18);
    patch.replace.insert(50);
    patch.entries.push(RecordedEntry {
        fs_id: 19,
        parent_id: 17,
        name: "notes.txt".to_string(),
        size: 9999,
        allocated: 12288,
        times: Times::default(),
        flags: EntryFlags::empty(),
    });
    patch.entries.push(RecordedEntry {
        fs_id: 50,
        parent_id: 17,
        name: "new.log".to_string(),
        size: 77,
        allocated: 4096,
        times: Times::default(),
        flags: EntryFlags::empty(),
    });

    let patched = round_trip(&original, "patched", &patch);

    let by_name = |index: &Index, name: &str| index.ids().find(|&id| index.name(id) == name);
    assert!(by_name(&patched, "Report.PDF").is_none(), "deleted");
    let notes = by_name(&patched, "notes.txt").expect("still there");
    assert_eq!(patched.node(notes).size(), 9999, "the new size won");
    assert!(
        !patched.node(notes).flags().contains(EntryFlags::HIDDEN),
        "the record's current attributes won"
    );
    let new = by_name(&patched, "new.log").expect("added");
    assert_eq!(patched.path(new), "C:\\Users\\Admin\\new.log");

    // The rollup is re-run from scratch, so the totals have to agree with the
    // nodes that are actually present.
    let root = patched.node(patched.root());
    let expected: u64 = patched
        .ids()
        .filter(|&id| !patched.node(id).is_directory())
        .map(|id| patched.node(id).size())
        .sum();
    assert_eq!(
        root.total_size(),
        expected,
        "totals match the surviving files"
    );
}

#[test]
fn dropping_one_hard_link_leaves_the_other() {
    // Every node of a record is replaced together, so the record's current set
    // of names is what survives - which is the only way to remove one link of
    // a file without removing the file.
    let original = sample_index();
    let mut patch = Patch::default();
    patch.replace.insert(30);
    patch.entries.push(RecordedEntry {
        fs_id: 30,
        parent_id: 23,
        name: "shared.dll".to_string(),
        size: 1 << 20,
        allocated: 1 << 20,
        times: Times::default(),
        flags: EntryFlags::empty(),
    });

    let patched = round_trip(&original, "unlink", &patch);
    let named: Vec<NodeId> = patched
        .ids()
        .filter(|&id| patched.name(id) == "shared.dll")
        .collect();
    assert_eq!(named.len(), 1, "one name left");
    assert!(!patched.node(named[0]).is_alias());
}

#[test]
fn free_space_is_refreshed_rather_than_replayed() {
    let original = sample_index();
    let patch = Patch {
        free_bytes: Some(1 << 30),
        ..Patch::default()
    };
    let patched = round_trip(&original, "free", &patch);

    let free = patched
        .ids()
        .find(|&id| patched.name(id) == "Free space")
        .expect("the row is there");
    assert_eq!(patched.node(free).size(), 1 << 30, "the current figure won");
}

// ---------------------------------------------------------------------------
// hostile input
// ---------------------------------------------------------------------------

/// Save the sample, then hand `mangle` the file's bytes to ruin.
fn corrupt(name: &str, mangle: impl Fn(&mut Vec<u8>)) -> emfit_core::error::Result<()> {
    let index = sample_index();
    let path = temp(name);
    snapshot::save(&path, &index, &CaseFold::Simple, &manifest(&index), &[]).expect("save");

    let mut bytes = std::fs::read(&path).expect("read");
    mangle(&mut bytes);
    std::fs::write(&path, &bytes).expect("write");

    snapshot::load(&path).map(|_| ())
}

#[test]
fn a_truncated_snapshot_is_refused() {
    let result = corrupt("truncated", |bytes| {
        bytes.truncate(bytes.len() / 2);
    });
    assert!(result.is_err(), "half a file is not a scan");
}

#[test]
fn a_snapshot_with_a_forged_header_is_refused() {
    let result = corrupt("forged-magic", |bytes| {
        bytes[0] = b'X';
    });
    assert!(result.is_err());

    let result = corrupt("forged-version", |bytes| {
        bytes[8] = 99;
    });
    assert!(result.is_err());
}

#[test]
fn flipped_payload_bytes_never_reach_the_index() {
    // The checksum has to catch this, because a bad name offset would panic
    // inside `Index::name` rather than fail. Offsets are measured from the end
    // of the file so they land in a payload wherever the table happens to end.
    for back in [64usize, 4096, 20_000] {
        let name = format!("flipped-{back}");
        let result = corrupt(&name, |bytes| {
            let at = bytes.len().saturating_sub(back.max(16));
            let end = (at + 8).min(bytes.len());
            for byte in &mut bytes[at..end] {
                *byte ^= 0xFF;
            }
        });
        assert!(
            result.is_err(),
            "corruption {back} bytes from the end survived"
        );
    }
}

#[test]
fn a_corrupt_section_table_is_caught() {
    // The rows themselves carry no checksum - they are what the checksums are
    // read from - so the bounds checks in the reader are the only guard.
    let result = corrupt("forged-table", |bytes| {
        // Row 0's offset field: point the first section past the file.
        bytes[32 + 16..32 + 24].copy_from_slice(&u64::MAX.to_le_bytes());
    });
    assert!(result.is_err());
}

#[test]
fn a_volume_without_stable_ids_is_not_cached_at_all() {
    // FAT32 mints its ids from a counter, so nothing survives a rescan to key
    // a replay by. Writing a snapshot that could never be loaded would be
    // worse than writing none.
    let caps = VolumeCaps {
        has_stable_ids: false,
        ..caps()
    };
    let mut builder = IndexBuilder::new(caps, CancellationToken::new());
    let _ = builder.push_batch(&[dir(ROOT_FS, ROOT_FS, ""), file(16, ROOT_FS, "a.txt", 1)]);
    let index = builder.finish().0;

    let path = temp("no-stable-ids");
    let _ = std::fs::remove_file(&path);
    assert!(
        snapshot::save(&path, &index, &CaseFold::Simple, &manifest(&index), &[]).is_err(),
        "refused"
    );
    assert!(!path.exists(), "and left nothing behind");
}

#[test]
fn a_snapshot_of_nothing_is_refused() {
    let path = temp("empty");
    std::fs::write(&path, b"").expect("write");
    assert!(snapshot::load(&path).is_err());
}
