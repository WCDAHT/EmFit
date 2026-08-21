//! Reading a chosen handful of MFT records instead of sweeping the table.
//!
//! This is the half of journal replay that touches the disk (`caching.md`
//! sec 6.4), and the property that matters is simple to state and easy to get
//! wrong: **a targeted read of a record must produce exactly what a full sweep
//! produced for it.** If it does not, a replayed index quietly disagrees with
//! a scanned one, which is the failure mode a cache must never have.
//!
//! So every test here sweeps the image first and compares against that, rather
//! than against hand-written expectations. The cases are the ones where a
//! sweep gets an answer for free that a targeted read has to work for:
//! fragmented MFTs, records whose attributes live in another record, 4Kn
//! geometry, and hard links.

mod common;

use common::*;
use emfit_core::model::sink::{RecordedEntry, RecordingSink, ValidatingSink};
use emfit_core::parser::ntfs::{ScanOptions, bootstrap, scanner};
use emfit_core::service::task::CancellationToken;

/// 2024-01-01T00:00:00Z as a FILETIME.
const FT_2024: u64 = 133_479_936_000_000_000;

/// Sweep the whole image, as a scan does.
fn sweep_all(spec: &ImageSpec, tag: &str) -> Vec<RecordedEntry> {
    let path = spec.write(tag);
    let source = open_image(&path);
    let layout = bootstrap::probe(&source).expect("bootstrap");
    let mut sink = ValidatingSink::new(RecordingSink::new());
    scanner::sweep(
        &source,
        &layout,
        &mut sink,
        &mut |_| {},
        &CancellationToken::new(),
        ScanOptions::default(),
    )
    .expect("sweep");
    let (recorded, violations) = sink.into_parts();
    assert!(violations.is_empty(), "sink contract held: {violations:?}");
    recorded.into_entries()
}

/// Read only `records`, as a replay does.
fn read_some(spec: &ImageSpec, tag: &str, records: &[u64]) -> (Vec<RecordedEntry>, Vec<u64>) {
    let path = spec.write(tag);
    let source = open_image(&path);
    let layout = bootstrap::probe(&source).expect("bootstrap");
    let mut sink = ValidatingSink::new(RecordingSink::new());
    let outcome = scanner::read_records(
        &mut scanner::RecordSource::Blocks(&source),
        &layout,
        records,
        &mut sink,
        &CancellationToken::new(),
    )
    .expect("targeted read");
    let (recorded, violations) = sink.into_parts();
    assert!(violations.is_empty(), "sink contract held: {violations:?}");
    assert!(
        outcome.opaque.is_empty(),
        "no record here has a non-resident attribute list"
    );
    (recorded.into_entries(), outcome.read)
}

/// Entries for one record, in a stable order, as comparable strings.
fn describe(entries: &[RecordedEntry], record: u64) -> Vec<String> {
    const MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
    let mut out: Vec<String> = entries
        .iter()
        .filter(|e| e.fs_id & MASK == record)
        .map(|e| {
            format!(
                "{:#x}|{}|{}|{}|{}|{}|{}|{:#x}",
                e.fs_id,
                e.parent_id,
                e.name,
                e.size,
                e.allocated,
                e.times.mtime,
                e.times.crtime,
                e.flags.bits()
            )
        })
        .collect();
    out.sort();
    out
}

/// The property every test asserts: same record, same entries, either way.
fn assert_agrees(spec: &ImageSpec, tag: &str, records: &[u64]) -> Vec<RecordedEntry> {
    let swept = sweep_all(spec, tag);
    let (targeted, _) = read_some(spec, tag, records);
    for &record in records {
        assert_eq!(
            describe(&targeted, record),
            describe(&swept, record),
            "record {record} came out differently"
        );
    }
    targeted
}

fn standard_image() -> ImageSpec {
    ImageSpec {
        geometry: GEOMETRY_512N,
        // Three fragments, so a run of wanted records can straddle a boundary
        // the reader must not cross.
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
            RecordSpec::new(19)
                .free()
                .attr(file_name_attr(16, 1, "deleted.tmp")),
            RecordSpec::new(21)
                .attr(std_info_attr(FT_2024, FT_2024, 0x02))
                .attr(file_name_attr(5, 1, "tail.bin"))
                .attr(resident_attr(0x80, &[0xAB; 64])),
        ],
    }
}

#[test]
fn a_targeted_read_matches_the_sweep_record_for_record() {
    let spec = standard_image();
    // 17 and 18 are neighbours (one read); 21 is in another fragment.
    let entries = assert_agrees(&spec, "targeted-std", &[5, 16, 17, 18, 21]);

    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"report.pdf"));
    assert!(names.contains(&"notes.txt"));
    assert!(names.contains(&"tail.bin"));
    assert!(
        !names.contains(&"$MFT"),
        "record 0 was not asked for and must not arrive"
    );
}

#[test]
fn records_that_were_not_asked_for_stay_out_of_the_result() {
    // A run of records is read in one go for the seek, but only the wanted
    // ones may be parsed - the rest still have live nodes in the index, and
    // emitting them again would duplicate every one of them.
    let spec = standard_image();
    let (entries, visited) = read_some(&spec, "targeted-gap", &[16, 21]);

    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names.len(), 2, "got {names:?}");
    assert!(names.contains(&"Users"));
    assert!(names.contains(&"tail.bin"));
    assert_eq!(visited, vec![16, 21], "and only those were visited");
}

#[test]
fn a_free_record_yields_nothing_which_is_how_a_deletion_is_seen() {
    // Replay learns a file was deleted by asking for its record and getting
    // no entry back, so "free record produces silence" is load-bearing.
    let spec = standard_image();
    let (entries, visited) = read_some(&spec, "targeted-free", &[19]);
    assert!(entries.is_empty(), "record 19 is not in use");
    assert_eq!(visited, vec![19], "it was still looked at");
}

#[test]
fn a_record_past_the_end_of_the_table_is_ignored() {
    let spec = standard_image();
    let (entries, visited) = read_some(&spec, "targeted-past-end", &[16, 9_999_999]);
    assert_eq!(entries.len(), 1);
    assert_eq!(visited, vec![16]);
}

#[test]
fn asking_twice_reads_once() {
    let spec = standard_image();
    let (entries, visited) = read_some(&spec, "targeted-dupes", &[17, 17, 17]);
    assert_eq!(visited, vec![17]);
    assert_eq!(entries.len(), 1, "one entry, not three");
}

#[test]
fn attributes_living_in_another_record_are_followed() {
    // The case a sweep gets for free: it reads every record and matches
    // extensions to bases afterwards. A targeted read has to follow the
    // attribute list to find them, and the name and size both live over there.
    let spec = ImageSpec {
        geometry: GEOMETRY_512N,
        // Eight clusters of four records each: enough to hold record 30.
        fragments: vec![(4, 8)],
        records: vec![
            RecordSpec::new(5)
                .directory()
                .attr(std_info_attr(FT_2024, FT_2024, 0x06))
                .attr(file_name_attr(5, 3, ".")),
            // The base holds only the list, which sends the name and the data
            // to record 30.
            RecordSpec::new(20)
                .attr(std_info_attr(FT_2024, FT_2024, 0))
                // (type, starting VCN, record holding it)
                .attr(attribute_list_attr(&[
                    (0x10, 0, 20),
                    (0x30, 0, 30),
                    (0x80, 0, 30),
                ])),
            RecordSpec::new(30)
                .extension_of(20)
                .attr(file_name_attr(5, 1, "split.bin"))
                .attr(non_resident_attr(
                    0x80,
                    50_000,
                    65_536,
                    &encode_runs(&[(200, 16)]),
                )),
        ],
    };

    // Ask for the base alone: the extension has to be found from its list.
    let entries = assert_agrees(&spec, "targeted-attrlist", &[20]);
    let split = entries
        .iter()
        .find(|e| e.name == "split.bin")
        .expect("the base was completed from its extension record");
    assert_eq!(split.fs_id, 20, "emitted under the base's record number");
    assert_eq!(split.size, 50_000);
    assert_eq!(split.allocated, 65_536);

    // And the other way round: touching the extension has to pull in the base,
    // because that is the record the index keys the file under.
    let (from_extension, visited) = read_some(&spec, "targeted-attrlist", &[30]);
    assert!(visited.contains(&20), "the base was pulled in: {visited:?}");
    assert_eq!(
        from_extension
            .iter()
            .find(|e| e.name == "split.bin")
            .map(|e| e.fs_id),
        Some(20)
    );
}

#[test]
fn hard_links_come_back_whole() {
    // Every node of a record is replaced together during a replay, so a
    // targeted read has to produce every name the record carries - and keep
    // the bytes on exactly one of them.
    let spec = ImageSpec {
        geometry: GEOMETRY_512N,
        fragments: vec![(4, 6)],
        records: vec![
            RecordSpec::new(5)
                .directory()
                .attr(std_info_attr(FT_2024, FT_2024, 0x06))
                .attr(file_name_attr(5, 3, ".")),
            RecordSpec::new(16)
                .directory()
                .attr(std_info_attr(FT_2024, FT_2024, 0))
                .attr(file_name_attr(5, 1, "Windows")),
            RecordSpec::new(20)
                .attr(std_info_attr(FT_2024, FT_2024, 0))
                .attr(file_name_attr(5, 1, "shared.dll"))
                .attr(file_name_attr(16, 1, "shared.dll"))
                .attr(non_resident_attr(
                    0x80,
                    4096,
                    4096,
                    &encode_runs(&[(300, 1)]),
                )),
        ],
    };

    let entries = assert_agrees(&spec, "targeted-links", &[20]);
    let links: Vec<&RecordedEntry> = entries.iter().filter(|e| e.name == "shared.dll").collect();
    assert_eq!(links.len(), 2, "both names arrived");
    assert_eq!(
        links.iter().filter(|e| e.flags.is_alias()).count(),
        1,
        "exactly one alias"
    );
    assert_eq!(
        links.iter().map(|e| e.allocated).sum::<u64>(),
        4096,
        "the bytes are counted once"
    );
    // Distinct ids, or the builder's map would drop one of them.
    assert_ne!(links[0].fs_id, links[1].fs_id);
}

#[test]
fn a_4kn_volume_reads_the_same_way() {
    // Native 4 KiB sectors with 4 KiB records: one fixup entry per record
    // rather than eight, and a read window that lands differently. The sweep
    // covers this geometry; a targeted read has to agree with it here too.
    let spec = ImageSpec {
        geometry: GEOMETRY_4KN,
        fragments: vec![(4, 16)],
        records: vec![
            RecordSpec::new(5)
                .directory()
                .attr(std_info_attr(FT_2024, FT_2024, 0x06))
                .attr(file_name_attr(5, 3, ".")),
            RecordSpec::new(6)
                .attr(std_info_attr(FT_2024, FT_2024, 0))
                .attr(file_name_attr(5, 1, "one.txt"))
                .attr(resident_attr(0x80, b"one")),
            RecordSpec::new(7)
                .attr(std_info_attr(FT_2024, FT_2024, 0))
                .attr(file_name_attr(5, 1, "two.txt"))
                .attr(resident_attr(0x80, b"two")),
        ],
    };

    let entries = assert_agrees(&spec, "targeted-4kn", &[6, 7]);
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"one.txt"), "got {names:?}");
    assert!(names.contains(&"two.txt"), "got {names:?}");
}

#[test]
fn asking_for_nothing_does_nothing() {
    let spec = standard_image();
    let (entries, visited) = read_some(&spec, "targeted-empty", &[]);
    assert!(entries.is_empty());
    assert!(visited.is_empty());
}
