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
    let (rebuilt, _, _, _) = snapshot::rebuild(
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
    let (rebuilt, _, _, _) = snapshot::rebuild(
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

// ---------------------------------------------------------------------------
// repairing an order instead of rebuilding it
// ---------------------------------------------------------------------------

/// What [`repair_round_trip`] hands back: the rebuilt index, each cached
/// column as it was repaired onto it (`None` where the repair refused), and
/// what the patch did to the paths of the nodes it never touched.
type Repaired = (
    Index,
    Vec<(SortKey, Option<Vec<u32>>)>,
    snapshot::PathDamage,
);

/// Save `index` with every cached column, rebuild it under `patch`, and return
/// the repaired orders beside the index they now have to address.
fn repair_round_trip(index: &Index, name: &str, patch: &Patch) -> Repaired {
    let path = temp(name);
    let orders: Vec<_> = snapshot::CACHED_ORDERS
        .iter()
        .map(|&key| (key, view::build_order(index, key)))
        .collect();
    snapshot::save(&path, index, &CaseFold::Simple, &manifest(index), &orders).expect("save");

    let loaded = snapshot::load(&path).expect("load");
    let damage = snapshot::path_damage(&loaded.columns, patch);
    let (rebuilt, _, _, remap) = snapshot::rebuild(
        &loaded.columns,
        index.caps().clone(),
        patch,
        &CancellationToken::new(),
    )
    .expect("rebuild");

    let repaired = snapshot::CACHED_ORDERS
        .iter()
        .map(|&key| {
            let stored = &loaded.orders[&key];
            let displaced: &[u32] = match key {
                SortKey::Path => &damage.stranded,
                _ => &[],
            };
            (
                key,
                view::repair_order(&rebuilt, key, stored, &remap, displaced),
            )
        })
        .collect();
    (rebuilt, repaired, damage)
}

/// A patch that replaces `replace` wholesale and puts `entries` back. A record
/// with no entry was deleted.
fn patch_of(replace: &[u64], entries: Vec<RecordedEntry>) -> Patch {
    Patch {
        replace: replace.iter().copied().collect(),
        entries,
        free_bytes: None,
    }
}

fn recorded(fs_id: u64, parent_id: u64, name: &str, size: u64, flags: EntryFlags) -> RecordedEntry {
    RecordedEntry {
        fs_id,
        parent_id,
        name: name.to_string(),
        size,
        allocated: size.div_ceil(4096) * 4096,
        times: Times {
            mtime: 1_800_000_000_000_000_000 + fs_id as i64,
            crtime: 1_600_000_000_000_000_000 + fs_id as i64,
        },
        flags,
    }
}

#[test]
fn a_repaired_order_is_the_order_a_rebuild_would_have_produced() {
    // The whole contract. A repair carries the survivors across and binary
    // searches the new nodes in; if that ever disagrees with sorting the
    // rebuilt index from scratch, the view silently sorts by the wrong thing.
    let index = sample_index();
    let cases: Vec<(&str, Patch)> = vec![
        ("repair-nothing", Patch::default()),
        (
            // A file rewritten in place: dropped and re-added, same name.
            "repair-touched",
            patch_of(
                &[18],
                vec![recorded(18, 17, "Report.PDF", 9 << 20, EntryFlags::empty())],
            ),
        ),
        (
            // Deletions only, so every survivor after them is renumbered.
            "repair-deleted",
            patch_of(&[19, 21], Vec::new()),
        ),
        (
            // New files, sorting first, last and in the middle of the columns.
            "repair-added",
            patch_of(
                &[],
                vec![
                    recorded(50, 17, "aaa-first.aaa", 7, EntryFlags::empty()),
                    recorded(51, 17, "zzz-last.zzz", 8, EntryFlags::empty()),
                    recorded(52, 22, "middle.dll", 9, EntryFlags::empty()),
                ],
            ),
        ),
        (
            // A renamed file: same node, different key, so it has to move.
            "repair-renamed",
            patch_of(
                &[20],
                vec![recorded(
                    20,
                    ROOT_FS,
                    "aardvark.sys",
                    4 << 30,
                    EntryFlags::SYSTEM,
                )],
            ),
        ),
        (
            // A new file with a name a survivor already has. Both orders are
            // sorted; they only agree about which of the two equals comes
            // first because ties break by node id.
            "repair-tied",
            patch_of(
                &[],
                vec![
                    recorded(53, 22, "notes.txt", 10, EntryFlags::empty()),
                    recorded(54, 16, "Report.PDF", 1, EntryFlags::empty()),
                ],
            ),
        ),
        (
            // Everything at once, including a directory that is merely touched
            // and comes back unchanged.
            "repair-mixed",
            patch_of(
                &[18, 19, 17],
                vec![
                    recorded(17, 16, "Admin", 0, EntryFlags::DIRECTORY),
                    recorded(18, 17, "Report.PDF", 1, EntryFlags::empty()),
                    recorded(60, 17, "brand.new", 2, EntryFlags::empty()),
                ],
            ),
        ),
    ];

    for (name, patch) in cases {
        let (rebuilt, repaired, damage) = repair_round_trip(&index, name, &patch);
        assert_eq!(damage.stranded.len(), 0, "{name}: no directory moved");
        for (key, order) in repaired {
            let order = order.unwrap_or_else(|| panic!("{name}: {key:?} refused to repair"));
            assert_eq!(order, view::build_order(&rebuilt, key), "{name}: {key:?}");
            // And the ranks derived from it are the ranks the sort path builds,
            // which is what the view actually consumes.
            assert_eq!(
                view::ranks_from_order(&order),
                view::build_ranks(&[&rebuilt], key),
                "{name}: {key:?} ranks"
            );
        }
    }
}

#[test]
fn a_moved_directory_takes_its_descendants_with_it() {
    // The case the whole Path column turns on. NTFS emits one event for a
    // renamed directory and nothing at all for what lives under it, so those
    // descendants survive the rebuild with every column intact except the one
    // that is assembled from ancestors. They have to be lifted out of the
    // stored order and placed again - and then the Path column repairs like
    // any other, which is the difference between repairing it and spending
    // seconds rebuilding it.
    let index = sample_index();

    // `Admin` holds four files and is itself under `Users`.
    let moves: [(&str, Patch); 3] = [
        (
            "repair-dir-renamed",
            patch_of(
                &[17],
                vec![recorded(17, 16, "Administrator", 0, EntryFlags::DIRECTORY)],
            ),
        ),
        (
            "repair-dir-moved",
            patch_of(
                &[17],
                vec![recorded(17, ROOT_FS, "Admin", 0, EntryFlags::DIRECTORY)],
            ),
        ),
        // Deleted, not renamed: its survivors can no longer reach the root, so
        // the rebuild re-files them under the orphan folder. Their paths change
        // just as surely.
        ("repair-dir-deleted", patch_of(&[17], Vec::new())),
    ];

    for (name, patch) in moves {
        let (rebuilt, repaired, damage) = repair_round_trip(&index, name, &patch);
        assert_eq!(damage.moved, 1, "{name}: one directory moved");
        assert!(
            damage.stranded.len() >= 4,
            "{name}: the files under it are stranded, got {}",
            damage.stranded.len()
        );
        for (key, order) in repaired {
            let order = order.unwrap_or_else(|| panic!("{name}: {key:?} refused"));
            assert_eq!(order, view::build_order(&rebuilt, key), "{name}: {key:?}");
        }
    }

    // A directory deleted along with everything in it - which is what actually
    // happens to a build directory or a browser cache - strands nobody, so it
    // never reaches the re-placing path at all.
    // Record 30 too: one of its two names is a hard link living under Admin,
    // and a record's nodes are only ever dropped together.
    let whole_tree = patch_of(&[17, 18, 19, 21, 30], Vec::new());
    let (rebuilt, repaired, damage) = repair_round_trip(&index, "repair-dir-tree", &whole_tree);
    assert_eq!(damage.moved, 1, "the directory still counts as moved");
    assert!(
        damage.stranded.is_empty(),
        "but nothing survived under it: {:?}",
        damage.stranded
    );
    for (key, order) in repaired {
        assert_eq!(
            order.expect("repairs"),
            view::build_order(&rebuilt, key),
            "{key:?}"
        );
    }

    // And a directory merely touched because a child was added to it has not
    // moved anything, which is the case that has to stay cheap.
    let touched = patch_of(
        &[17],
        vec![recorded(17, 16, "Admin", 0, EntryFlags::DIRECTORY)],
    );
    let (rebuilt, repaired, damage) = repair_round_trip(&index, "repair-dir-touched", &touched);
    assert_eq!(damage.moved, 0, "same name, same parent, no damage");
    assert!(damage.stranded.is_empty());
    for (key, order) in repaired {
        assert_eq!(
            order.expect("repairs"),
            view::build_order(&rebuilt, key),
            "{key:?}"
        );
    }
}

#[test]
fn a_move_big_enough_to_cost_more_than_a_rebuild_is_declined() {
    // Placing a node again costs a binary search, and a binary search derives
    // a key per probe; a rebuild derives one key per node and sorts. So there
    // is a share of the volume past which repairing is the slower answer, and
    // the caller has to be told to rebuild instead of quietly taking longer.
    let index = crowded_index(400);
    let nodes = index.len();

    // One directory holding a quarter of the volume is still worth repairing.
    let small = snapshot::PathDamage {
        moved: 1,
        stranded: (0..nodes as u32 / 4).collect(),
        sample: Vec::new(),
    };
    assert!(small.repairable(nodes));

    // Half of it is not.
    let large = snapshot::PathDamage {
        moved: 1,
        stranded: (0..nodes as u32 / 2).collect(),
        sample: Vec::new(),
    };
    assert!(!large.repairable(nodes));

    // Nothing moved is always repairable, whatever the volume.
    assert!(snapshot::PathDamage::default().repairable(nodes));
}

#[test]
fn every_moved_directory_is_counted_not_just_the_first() {
    // The count is what a diagnostic reports and what the share guard is read
    // against, so it has to be the real number of directories rather than a
    // sample of them.
    let index = crowded_index(40);
    let dirs: Vec<u64> = (100..104).collect();
    let patch = patch_of(
        &dirs,
        dirs.iter()
            .map(|&fs_id| {
                recorded(
                    fs_id,
                    ROOT_FS,
                    &format!("renamed{fs_id}"),
                    0,
                    EntryFlags::DIRECTORY,
                )
            })
            .collect(),
    );

    let (rebuilt, repaired, damage) = repair_round_trip(&index, "repair-many-dirs", &patch);
    assert_eq!(damage.moved, 4, "all four, not one and not a sample");
    assert_eq!(
        damage.stranded.len(),
        40,
        "every file under them, across all four"
    );
    for (key, order) in repaired {
        let order = order.expect("repairs");
        assert_eq!(order, view::build_order(&rebuilt, key), "{key:?}");
    }
}

#[test]
fn a_nested_move_strands_the_whole_subtree_beneath_it() {
    // `Users` moving takes `Admin` with it, and everything under `Admin` too -
    // two levels down from the one event the journal emitted. A check that
    // only looked at the moved directory's own children would miss them.
    let index = sample_index();
    let patch = patch_of(
        &[16],
        vec![recorded(16, ROOT_FS, "People", 0, EntryFlags::DIRECTORY)],
    );
    let (rebuilt, repaired, damage) = repair_round_trip(&index, "repair-dir-nested", &patch);

    assert_eq!(damage.moved, 1);
    // Admin, and the four files under it.
    assert_eq!(damage.stranded.len(), 5, "the whole subtree, not one level");
    for (key, order) in repaired {
        let order = order.expect("repairs");
        assert_eq!(order, view::build_order(&rebuilt, key), "{key:?}");
    }
}

#[test]
fn an_order_from_some_other_snapshot_is_refused_rather_than_used() {
    // The remap says how many nodes the snapshot had, so an order of another
    // length came from another scan. Using it would seat rows at positions
    // that do not exist.
    let index = sample_index();
    let (rebuilt, ..) = repair_round_trip(&index, "repair-mismatch", &Patch::default());
    let short = vec![0u32; index.len() - 1];
    let remap: Vec<u32> = (0..index.len() as u32).collect();
    assert!(view::repair_order(&rebuilt, SortKey::Name, &short, &remap, &[]).is_none());

    // A remap that sends two old nodes to one new one is not a renumbering.
    let order = view::build_order(&index, SortKey::Name);
    let mut forged: Vec<u32> = (0..index.len() as u32).collect();
    forged[1] = forged[0];
    assert!(view::repair_order(&rebuilt, SortKey::Name, &order, &forged, &[]).is_none());
}

/// A deterministic stand-in for randomness: same sequence every run, so a
/// failure is reproducible from the seed alone.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A name drawn from the same small pool [`crowded_index`] uses, so a new node
/// lands on a name a survivor already has. That tie is the whole point: the
/// two orders are both sorted, and only agree about which of the equals comes
/// first if the repair breaks ties the way a stable sort over ascending ids
/// does.
fn crowded_name(rng: &mut Lcg) -> String {
    let ext = ["txt", "dll", "jpg", "", "TXT"][rng.below(5) as usize];
    let stem = rng.below(40);
    if ext.is_empty() {
        format!("file{stem:04}")
    } else {
        format!("file{stem:04}.{ext}")
    }
}

/// A volume big enough to have real ties: many files share an extension, a
/// kind and a size, which is where a repair that seats a new node on the wrong
/// side of an equal one shows up.
fn crowded_index(files: u64) -> Index {
    let mut builder = IndexBuilder::new(caps(), CancellationToken::new());
    let exts = ["txt", "dll", "jpg", "", "TXT"];
    let mut batch: Vec<(u64, u64, String, u64, EntryFlags)> =
        vec![(ROOT_FS, ROOT_FS, String::new(), 0, EntryFlags::DIRECTORY)];
    for d in 0..4u64 {
        batch.push((
            100 + d,
            ROOT_FS,
            format!("dir{d}"),
            0,
            EntryFlags::DIRECTORY,
        ));
    }
    for i in 0..files {
        let ext = exts[(i % exts.len() as u64) as usize];
        let name = if ext.is_empty() {
            format!("file{:04}", i % 40)
        } else {
            format!("file{:04}.{ext}", i % 40)
        };
        batch.push((1000 + i, 100 + (i % 4), name, i % 3, EntryFlags::empty()));
    }
    let raw: Vec<RawEntry<'_>> = batch
        .iter()
        .map(|(fs_id, parent, name, size, flags)| entry(*fs_id, *parent, name, *size, *flags))
        .collect();
    let _ = builder.push_batch(&raw);
    builder.finish().0
}

#[test]
fn repairs_match_rebuilds_across_a_thousand_random_patches() {
    // The small hand-written cases cover the shapes; this covers the seating.
    // Ties are the failure mode a repair has - a new node placed on either
    // side of an equal one is still "sorted", just not the same sorted the
    // rebuild produces, and the two would then disagree about what row 40 is.
    let index = crowded_index(200);
    let fs_ids: Vec<u64> = (0..index.len())
        .filter_map(|i| index.native_id(NodeId::new(i as u32)))
        .collect();
    let mut rng = Lcg(0x5EED);
    let mut next_fs = 90_000u64;

    for round in 0..40 {
        let mut replace: Vec<u64> = Vec::new();
        let mut entries: Vec<RecordedEntry> = Vec::new();

        // Touch a few existing files: drop some outright, put others back
        // under a new name so the key moves.
        for _ in 0..rng.below(6) + 1 {
            let victim = fs_ids[rng.below(fs_ids.len() as u64) as usize];
            if victim == ROOT_FS || victim < 1000 {
                continue; // directories are their own test
            }
            replace.push(victim);
            if rng.below(3) > 0 {
                entries.push(recorded(
                    victim,
                    100,
                    &crowded_name(&mut rng),
                    rng.below(3),
                    EntryFlags::empty(),
                ));
            }
        }

        // And add a few brand new ones.
        for _ in 0..rng.below(5) {
            next_fs += 1;
            entries.push(recorded(
                next_fs,
                101,
                &crowded_name(&mut rng),
                rng.below(3),
                EntryFlags::empty(),
            ));
        }

        let patch = patch_of(&replace, entries);
        let name = format!("repair-fuzz-{round}");
        let (rebuilt, repaired, damage) = repair_round_trip(&index, &name, &patch);
        assert_eq!(damage.stranded.len(), 0, "{name}: only files were touched");
        for (key, order) in repaired {
            let order = order.unwrap_or_else(|| panic!("{name}: {key:?} refused"));
            assert_eq!(order, view::build_order(&rebuilt, key), "{name}: {key:?}");
        }
    }
}
