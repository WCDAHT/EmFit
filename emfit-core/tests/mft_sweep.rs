//! The full sweep pipeline against synthetic NTFS images.
//!
//! Every test here builds a real volume image - boot sector, fragmented MFT,
//! records with valid update-sequence arrays - writes it to disk, and runs the
//! production path over it: `FileBlockSource` -> `bootstrap::probe` ->
//! `scanner::sweep`. Nothing is mocked below the filesystem, so the offset
//! arithmetic, fixup repair, deferral, and hard-link accounting are all
//! exercised together, on any platform CI runs.

mod common;

use std::ops::ControlFlow;

use common::*;
use emfit_core::model::sink::{EntrySink, RecordingSink, ValidatingSink};
use emfit_core::parser::ntfs::scanner::SweepOutcome;
use emfit_core::parser::ntfs::{ScanOptions, bootstrap, scanner};
use emfit_core::service::scan;
use emfit_core::service::task::CancellationToken;

/// 2024-01-01T00:00:00Z as a FILETIME.
const FT_2024: u64 = 133_479_936_000_000_000;

/// Build, write, probe, and sweep an image into `sink`.
fn sweep_into(spec: &ImageSpec, tag: &str, sink: &mut dyn EntrySink) -> SweepOutcome {
    let path = spec.write(tag);
    let source = open_image(&path);
    let layout = bootstrap::probe(&source).expect("bootstrap from record 0");
    scanner::sweep(
        &source,
        &layout,
        sink,
        &mut |_| {},
        &CancellationToken::new(),
        ScanOptions::default(),
    )
    .expect("sweep")
}

/// The standard fixture: a three-fragment MFT with a small tree.
///
/// Fragment 1 (LCN 4, 2 clusters) holds records 0..8, fragment 2 (LCN 20,
/// 3 clusters) records 8..20, fragment 3 (LCN 40, 1 cluster) records 20..24 -
/// so the tree spans every fragment and a batch that ignored the boundaries
/// would read garbage.
fn standard_image() -> ImageSpec {
    ImageSpec {
        geometry: GEOMETRY_512N,
        fragments: vec![(4, 2), (20, 3), (40, 1)],
        records: vec![
            RecordSpec::new(5)
                .directory()
                .attr(std_info_attr(FT_2024, FT_2024, 0x06))
                .attr(file_name_attr(5, 3, ".")),
            RecordSpec::new(16)
                .directory()
                .attr(std_info_attr(FT_2024, FT_2024, 0))
                .attr(file_name_attr(5, 1, "Users")),
            RecordSpec::new(17)
                .attr(std_info_attr(FT_2024, FT_2024 + 10_000_000, 0))
                .attr(file_name_attr(16, 1, "report.pdf"))
                .attr(non_resident_attr(
                    0x80,
                    30_000,
                    32_768,
                    &encode_runs(&[(100, 8)]),
                )),
            RecordSpec::new(18)
                .attr(std_info_attr(FT_2024, FT_2024, 0))
                .attr(file_name_attr(16, 1, "notes.txt"))
                .attr(resident_attr(0x80, b"twenty bytes of text")),
            // A deleted file: parses fine, must not be emitted.
            RecordSpec::new(19)
                .free()
                .attr(file_name_attr(16, 1, "deleted.tmp")),
            // Lives in the third fragment.
            RecordSpec::new(21)
                .attr(std_info_attr(FT_2024, FT_2024, 0x02))
                .attr(file_name_attr(5, 1, "tail.bin"))
                .attr(resident_attr(0x80, &[0xAB; 64])),
        ],
    }
}

#[test]
fn a_fragmented_mft_sweeps_into_the_right_tree() {
    let mut sink = ValidatingSink::new(RecordingSink::new());
    let outcome = sweep_into(&standard_image(), "std", &mut sink);
    let (recorded, violations) = sink.into_parts();

    assert!(violations.is_empty(), "sink contract held: {violations:?}");

    // The root arrives with the empty name the sink contract reserves for it,
    // not NTFS's internal ".".
    let root = recorded
        .entries()
        .iter()
        .find(|e| e.fs_id == 5)
        .expect("root emitted");
    assert_eq!(root.name, "");
    assert_eq!(root.parent_id, 5, "the root parents itself");
    assert!(root.is_directory());

    let report = recorded.find("report.pdf").expect("crosses fragment 2");
    assert_eq!(report.parent_id, 16);
    assert_eq!(report.size, 30_000);
    assert_eq!(report.allocated, 32_768);
    assert!(
        report.times.crtime > 1_700_000_000_000_000_000,
        "after 2023"
    );
    assert_eq!(
        report.times.mtime - report.times.crtime,
        1_000_000_000,
        "the one-second gap survives FILETIME conversion"
    );

    let notes = recorded.find("notes.txt").expect("resident $DATA");
    assert_eq!(notes.size, 20);
    assert_eq!(notes.allocated, 0, "resident contents occupy no clusters");

    let tail = recorded
        .find("tail.bin")
        .expect("record in the last fragment");
    assert_eq!(tail.parent_id, 5);

    // $MFT itself is a real file occupying real space.
    let mft = recorded
        .find("$MFT")
        .expect("record 0 is scanned like any file");
    assert_eq!(mft.size, 24 * 1024, "24 record slots of 1 KiB");

    assert!(
        recorded.find("deleted.tmp").is_none(),
        "free records are skipped"
    );

    let stats = outcome.stats;
    assert_eq!(stats.records_read, 24, "the full fragment capacity");
    assert_eq!(stats.records_in_use, 6);
    assert_eq!(stats.entries_emitted, 6);
    assert_eq!(stats.files, 4, "$MFT, report.pdf, notes.txt, tail.bin");
    assert_eq!(stats.directories, 2, "root and Users");
    assert_eq!(
        stats.never_used_slots,
        24 - 7,
        "written slots incl. the free one"
    );
    assert_eq!(stats.bad_records, 0);
    assert_eq!(stats.failed_fixup, 0);
    assert!(
        stats.reads >= 3,
        "at least one read per fragment, got {}",
        stats.reads
    );
}

#[test]
fn hard_links_and_alternate_streams_are_accounted_once() {
    let spec = ImageSpec {
        geometry: GEOMETRY_512N,
        fragments: vec![(4, 6)],
        records: vec![
            RecordSpec::new(5)
                .directory()
                .attr(file_name_attr(5, 3, ".")),
            RecordSpec::new(16)
                .directory()
                .attr(file_name_attr(5, 1, "bin")),
            RecordSpec::new(20)
                .attr(std_info_attr(FT_2024, FT_2024, 0))
                .attr(file_name_attr(5, 1, "app.exe"))
                .attr(file_name_attr(16, 1, "app-link.exe"))
                .attr(non_resident_attr(
                    0x80,
                    8_000,
                    8_192,
                    &encode_runs(&[(60, 2)]),
                ))
                .attr(named_non_resident_attr(
                    0x80,
                    "Zone.Identifier",
                    100,
                    4_096,
                    &encode_runs(&[(62, 1)]),
                ))
                .attr(named_resident_attr(0x80, "notes", b"resident ads")),
        ],
    };

    let mut sink = ValidatingSink::new(RecordingSink::new());
    let outcome = sweep_into(&spec, "links-ads", &mut sink);
    let (recorded, violations) = sink.into_parts();
    assert!(violations.is_empty(), "{violations:?}");

    let owner = recorded.find("app.exe").expect("first name owns the bytes");
    let alias = recorded
        .find("app-link.exe")
        .expect("second name is emitted too");

    assert_eq!(owner.size, 8_000);
    assert_eq!(
        owner.allocated,
        8_192 + 4_096,
        "the file's clusters plus its non-resident stream's"
    );
    assert!(!owner.flags.is_alias());

    assert_eq!(alias.size, 8_000, "every link shows the full logical size");
    assert_eq!(alias.allocated, 0, "but only one carries the disk cost");
    assert!(alias.flags.is_alias());
    assert_eq!(alias.parent_id, 16, "each link lives in its own directory");

    let stats = &outcome.stats;
    assert_eq!(stats.multi_linked_records, 1);
    assert_eq!(stats.hard_link_aliases, 1);
    assert_eq!(stats.ads_streams, 2);
    assert_eq!(
        stats.ads_bytes, 4_096,
        "resident streams occupy no clusters"
    );

    let mut ads = outcome.ads.clone();
    ads.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(ads.len(), 2);
    assert_eq!(ads[0].owner, 20);
    assert_eq!(ads[0].name, "Zone.Identifier");
    assert_eq!(ads[0].size, 100);
    assert_eq!(ads[0].allocated, 4_096);
    assert_eq!(ads[1].name, "notes");
    assert_eq!(ads[1].size, 12);
    assert_eq!(ads[1].allocated, 0);
}

#[test]
fn a_sparse_system_stream_does_not_count_the_volume_twice() {
    // $BadClus:$Bad in miniature: a named stream whose header claims the
    // whole volume as its span, backed by one all-sparse run. Counting the
    // claimed span would add the entire disk a second time; the run list
    // says it owns nothing.
    // Crucially NO sparse flag on this stream: the real $BadClus:$Bad
    // carries its volume-spanning hole without one, so hole detection must
    // come from the run list, never the attribute flags.
    let span = 1u64 << 40; // "1 TiB" of hole
    let clusters = span / u64::from(CLUSTER);
    let bad_stream =
        named_non_resident_attr(0x80, "$Bad", span, span, &encode_sparse_run(clusters));

    let spec = ImageSpec {
        geometry: GEOMETRY_512N,
        // 6 clusters x 4 records each: room for records 20 and 21 below.
        fragments: vec![(4, 6)],
        records: vec![
            RecordSpec::new(5)
                .directory()
                .attr(file_name_attr(5, 3, ".")),
            RecordSpec::new(20)
                .attr(std_info_attr(FT_2024, FT_2024, 0x06))
                .attr(file_name_attr(5, 3, "$BadClus"))
                .attr(resident_attr(0x80, &[]))
                .attr(bad_stream),
            // A genuinely sparse user file: 16 real clusters, then a hole.
            {
                let mut runs = encode_runs(&[(60, 16)]);
                runs.pop(); // drop the terminator; the hole continues the list
                runs.extend_from_slice(&encode_sparse_run(1024));
                let mut data = non_resident_attr(0x80, 5_000_000, 5_000_000, &runs);
                set_attr_flags(&mut data, 0x8000);
                RecordSpec::new(21)
                    .attr(file_name_attr(5, 1, "sparse.dat"))
                    .attr(data)
            },
        ],
    };

    let mut sink = ValidatingSink::new(RecordingSink::new());
    let outcome = sweep_into(&spec, "badclus", &mut sink);
    let (recorded, violations) = sink.into_parts();
    assert!(violations.is_empty(), "{violations:?}");

    let badclus = recorded.find("$BadClus").expect("metafile emitted");
    assert_eq!(badclus.allocated, 0, "the hole costs nothing on disk");
    assert_eq!(badclus.size, 0, "its own unnamed $DATA is empty");

    let ads = outcome.ads.iter().find(|a| a.name == "$Bad").unwrap();
    assert_eq!(ads.size, span, "the logical span is still reported");
    assert_eq!(ads.allocated, 0, "but occupies nothing");
    assert_eq!(outcome.stats.ads_bytes, 0);

    let sparse = recorded.find("sparse.dat").expect("sparse user file");
    assert_eq!(sparse.size, 5_000_000);
    assert_eq!(
        sparse.allocated,
        16 * u64::from(CLUSTER),
        "only the written clusters count, not the reserved span"
    );
}

#[test]
fn extension_records_are_joined_without_seeking() {
    let spec = ImageSpec {
        geometry: GEOMETRY_512N,
        fragments: vec![(4, 8)],
        records: vec![
            RecordSpec::new(5)
                .directory()
                .attr(file_name_attr(5, 3, ".")),
            // The base: everything interesting lives elsewhere.
            RecordSpec::new(21)
                .attr(std_info_attr(FT_2024, FT_2024, 0))
                .attr(attribute_list_attr(&[
                    (0x10, 0, 21),
                    (0x30, 0, 22),
                    (0x80, 0, 23),
                ])),
            // Extension holding the name...
            RecordSpec::new(22)
                .extension_of(21)
                .attr(file_name_attr(5, 1, "big.bin")),
            // ...and the one holding the data.
            RecordSpec::new(23).extension_of(21).attr(non_resident_attr(
                0x80,
                1_000_000,
                1_003_520,
                &encode_runs(&[(70, 245)]),
            )),
            // An extension whose base never appears: its attributes are lost,
            // and that loss must be counted rather than silent.
            RecordSpec::new(24)
                .extension_of(99)
                .attr(file_name_attr(5, 1, "ghost.dat")),
        ],
    };

    let mut sink = ValidatingSink::new(RecordingSink::new());
    let outcome = sweep_into(&spec, "extensions", &mut sink);
    let (recorded, violations) = sink.into_parts();
    assert!(violations.is_empty(), "{violations:?}");

    let big = recorded
        .find("big.bin")
        .expect("assembled from three records");
    assert_eq!(big.fs_id, 21, "emitted under the base record's id");
    assert_eq!(big.size, 1_000_000);
    assert_eq!(big.allocated, 1_003_520);

    assert!(
        recorded.find("ghost.dat").is_none(),
        "an orphaned extension's name belongs to no live file"
    );

    let stats = &outcome.stats;
    assert_eq!(stats.deferred_bases, 1);
    assert_eq!(stats.extension_records, 3);
    assert_eq!(stats.resolved_from_extensions, 1);
    assert_eq!(stats.orphaned_extensions, 1);
}

#[test]
fn a_4kn_volume_parses_with_the_wide_fixup_stride() {
    let spec = ImageSpec {
        geometry: GEOMETRY_4KN,
        // 4096-byte records on 4096-byte clusters: one record per cluster.
        fragments: vec![(2, 20)],
        records: vec![
            RecordSpec::new(5)
                .directory()
                .attr(file_name_attr(5, 3, ".")),
            RecordSpec::new(16)
                .attr(std_info_attr(FT_2024, FT_2024, 0))
                .attr(file_name_attr(5, 1, "sector-native.txt"))
                .attr(resident_attr(0x80, &[0x42; 100])),
        ],
    };

    let mut sink = ValidatingSink::new(RecordingSink::new());
    let outcome = sweep_into(&spec, "4kn", &mut sink);
    let (recorded, violations) = sink.into_parts();
    assert!(violations.is_empty(), "{violations:?}");

    // The proof is the name surviving intact: with a 512-byte stride the
    // fixup repair would have stamped garbage into the record body.
    let file = recorded.find("sector-native.txt").expect("parsed on 4Kn");
    assert_eq!(file.size, 100);
    assert_eq!(outcome.stats.failed_fixup, 0);
    assert_eq!(outcome.stats.records_in_use, 3);
}

#[test]
fn the_sink_stopping_stops_the_sweep() {
    let mut sink = RecordingSink::new().stop_after(2);
    let outcome = sweep_into(&standard_image(), "cancel", &mut sink);

    assert!(outcome.stats.cancelled, "Break must reach the sweep loop");
    assert_eq!(sink.len(), 2, "nothing pushed past the stop");
}

#[test]
fn cancellation_before_the_sweep_reads_nothing() {
    let path = standard_image().write("cancel-early");
    let source = open_image(&path);
    let layout = bootstrap::probe(&source).unwrap();

    let cancel = CancellationToken::new();
    cancel.cancel();

    let mut sink = RecordingSink::new();
    let outcome = scanner::sweep(
        &source,
        &layout,
        &mut sink,
        &mut |_| {},
        &cancel,
        ScanOptions::default(),
    )
    .unwrap();

    assert!(outcome.stats.cancelled);
    assert_eq!(sink.len(), 0);
    assert_eq!(outcome.stats.reads, 0, "cancel precedes the first read");
}

#[test]
fn tiny_chunks_still_respect_fragment_boundaries() {
    // One record per read: maximum pressure on the clamp and the
    // reader/parser handoff.
    let path = standard_image().write("tiny-chunks");
    let source = open_image(&path);
    let layout = bootstrap::probe(&source).unwrap();

    let mut sink = RecordingSink::new();
    let outcome = scanner::sweep(
        &source,
        &layout,
        &mut sink,
        &mut |_| {},
        &CancellationToken::new(),
        ScanOptions {
            chunk_bytes: 1, // clamps up to one record
            ..ScanOptions::default()
        },
    )
    .unwrap();

    assert_eq!(outcome.stats.records_read, 24);
    assert_eq!(outcome.stats.reads, 24, "one per record");
    assert!(sink.find("report.pdf").is_some());
    assert!(sink.find("tail.bin").is_some());
}

#[test]
fn scan_image_builds_a_rolled_up_index() {
    // The service end to end: image -> sweep -> builder -> finished index.
    let path = standard_image().write("service");
    let cancel = CancellationToken::new();
    let outcome = scan::scan_image(
        &path,
        &scan::VolumeScanOptions::default(),
        &mut |_| {},
        &cancel,
    )
    .expect("scan_image");

    let index = &outcome.index;
    let root = index.node(index.root());

    // 6 emitted entries; the root is one of them, so no synthesis happened.
    assert_eq!(index.len(), 6);
    assert_eq!(root.file_count(), 4);
    assert_eq!(root.dir_count(), 2);
    assert!(
        outcome.warnings.is_empty(),
        "a clean image scans without warnings: {:?}",
        outcome.warnings
    );

    // Rollup: root totals are every file's logical bytes.
    let expected_total = (24 * 1024) + 30_000 + 20 + 64;
    assert_eq!(root.total_size(), expected_total);

    // Paths assemble from the image's label, never a hardcoded drive.
    let report = index
        .ids()
        .find(|&id| index.name(id) == "report.pdf")
        .expect("report.pdf in the index");
    let label = index.caps().root_label.clone();
    assert_eq!(index.path(report), format!("{label}\\Users\\report.pdf"));

    // Native ids survive into the side table.
    assert_eq!(index.native_id(report), Some(17));
    assert_eq!(outcome.mode, scan::AccessMode::Image);
}

#[test]
fn a_recorded_sweep_replays_into_the_same_entries() {
    // RecordingSink round-trip: what the scanner pushed can be replayed into
    // any other sink - the mechanism for turning a field bug into a fixture.
    let mut sink = RecordingSink::new();
    sweep_into(&standard_image(), "replay", &mut sink);

    let mut replayed = RecordingSink::new();
    for entry in sink.entries() {
        let flow = replayed.push_batch(&[entry.as_raw()]);
        assert_eq!(flow, ControlFlow::Continue(()));
    }
    assert_eq!(replayed.entries(), sink.entries());
}
