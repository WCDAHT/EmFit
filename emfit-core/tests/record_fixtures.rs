//! Parser tests against fixture MFT records on disk (STANDARDS §5.1,
//! roadmap M1 exit criteria).
//!
//! The `.bin` files under `tests/fixtures/` are byte-for-byte MFT records —
//! valid headers, update-sequence arrays applied, real attribute chains. Each
//! test reads the committed file and runs the production parser over it, so a
//! parser regression fails against stable bytes rather than against a builder
//! that may have regressed in sympathy.
//!
//! Regeneration: the fixtures are written by the deterministic builders in
//! `tests/common/`. Delete a `.bin` and run this suite once; the missing file
//! is rebuilt (and the test then verifies it like any other run). Commit the
//! result.

mod common;

use std::path::PathBuf;

use common::*;
use emfit_core::parser::ntfs::FileName;
use emfit_core::parser::ntfs::attr::AttributeType;
use emfit_core::parser::ntfs::record::apply_fixup;
use emfit_core::parser::ntfs::record::{Record, RecordHeader};

/// 2024-01-01T00:00:00Z as a FILETIME.
const FT_2024: u64 = 133_479_936_000_000_000;

fn fixture(name: &str, build: impl FnOnce() -> Vec<u8>) -> Vec<u8> {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "tests", "fixtures", name]
        .iter()
        .collect();
    if !path.exists() {
        std::fs::write(&path, build()).expect("write fixture");
        eprintln!("generated {}; commit it", path.display());
    }
    std::fs::read(&path).expect("read fixture")
}

#[test]
fn plain_file_record() {
    let mut bytes = fixture("record_plain_file.bin", || {
        RecordSpec::new(17)
            .attr(std_info_attr(FT_2024, FT_2024 + 10_000_000, 0x02))
            .attr(file_name_attr(16, 1, "report.pdf"))
            .attr(non_resident_attr(
                0x80,
                30_000,
                32_768,
                &encode_runs(&[(100, 8)]),
            ))
            .build(GEOMETRY_512N)
    });

    let record = Record::parse(&mut bytes, SECTOR).expect("valid record");
    assert!(record.is_in_use());
    assert!(!record.is_directory());
    assert_eq!(record.header().record_number, 17);

    let kinds: Vec<_> = record.attributes().map(|a| a.kind()).collect();
    assert_eq!(
        kinds,
        vec![
            AttributeType::StandardInformation,
            AttributeType::FileName,
            AttributeType::Data,
        ]
    );

    let fname = record
        .attributes()
        .find(|a| a.kind() == AttributeType::FileName)
        .and_then(|a| a.resident_value())
        .and_then(FileName::parse)
        .expect("$FILE_NAME parses");
    assert_eq!(fname.to_name(), "report.pdf");
    assert_eq!(fname.parent_record(), 16);

    let data = record
        .attributes()
        .find(|a| a.kind() == AttributeType::Data)
        .expect("$DATA present");
    let nr = data.non_resident().expect("non-resident");
    assert_eq!(nr.data_size(), 30_000);
    assert_eq!(nr.physical_size(), 32_768);
    let runs = nr.data_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].lcn, Some(100));
    assert_eq!(runs[0].cluster_count, 8);
}

#[test]
fn hard_linked_record_carries_every_name() {
    let mut bytes = fixture("record_hard_links.bin", || {
        RecordSpec::new(20)
            .attr(std_info_attr(FT_2024, FT_2024, 0))
            .attr(file_name_attr(5, 1, "app.exe"))
            .attr(file_name_attr(16, 1, "app-link.exe"))
            .attr(resident_attr(0x80, &[0u8; 16]))
            .build(GEOMETRY_512N)
    });

    let record = Record::parse(&mut bytes, SECTOR).unwrap();
    let names: Vec<(String, u64)> = record
        .attributes()
        .filter(|a| a.kind() == AttributeType::FileName)
        .filter_map(|a| a.resident_value())
        .filter_map(FileName::parse)
        .map(|f| (f.to_name(), f.parent_record()))
        .collect();

    assert_eq!(
        names,
        vec![("app.exe".to_string(), 5), ("app-link.exe".to_string(), 16),]
    );
}

#[test]
fn alternate_stream_record_distinguishes_its_streams() {
    let mut bytes = fixture("record_ads.bin", || {
        RecordSpec::new(30)
            .attr(file_name_attr(5, 1, "carrier.txt"))
            .attr(resident_attr(0x80, b"main contents"))
            .attr(named_resident_attr(0x80, "secret", b"hidden payload"))
            .build(GEOMETRY_512N)
    });

    let record = Record::parse(&mut bytes, SECTOR).unwrap();
    let data: Vec<_> = record
        .attributes()
        .filter(|a| a.kind() == AttributeType::Data)
        .collect();

    assert_eq!(data.len(), 2);
    assert!(data[0].is_unnamed(), "the file's own contents");
    assert_eq!(data[0].resident_value().unwrap().len(), 13);
    assert!(!data[1].is_unnamed(), "the ADS");
    assert_eq!(data[1].resident_value().unwrap(), b"hidden payload");
}

#[test]
fn extension_record_names_its_base() {
    let mut bytes = fixture("record_extension.bin", || {
        RecordSpec::new(23)
            .extension_of(21)
            .attr(non_resident_attr(
                0x80,
                1_000_000,
                1_003_520,
                &encode_runs(&[(70, 245)]),
            ))
            .build(GEOMETRY_512N)
    });

    let header = RecordHeader::parse(&bytes).unwrap();
    assert_eq!(header.base_record(), Some(21), "sequence bits masked off");

    let record = Record::parse(&mut bytes, SECTOR).unwrap();
    let nr = record.attributes().next().unwrap().non_resident().unwrap();
    assert_eq!(nr.data_size(), 1_000_000);
}

#[test]
fn attribute_list_record_points_elsewhere() {
    let mut bytes = fixture("record_attribute_list.bin", || {
        RecordSpec::new(21)
            .attr(std_info_attr(FT_2024, FT_2024, 0))
            .attr(attribute_list_attr(&[
                (0x10, 0, 21),
                (0x30, 0, 22),
                (0x80, 0, 23),
            ]))
            .build(GEOMETRY_512N)
    });

    let record = Record::parse(&mut bytes, SECTOR).unwrap();
    let list = record
        .attributes()
        .find(|a| a.kind() == AttributeType::AttributeList)
        .and_then(|a| a.resident_value())
        .expect("$ATTRIBUTE_LIST present");

    let entries = emfit_core::parser::ntfs::attr::parse_attribute_list(list);
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[1].type_code, 0x30);
    assert_eq!(entries[1].record_number, 22);
    assert_eq!(entries[2].type_code, 0x80);
    assert_eq!(entries[2].record_number, 23);
}

#[test]
fn four_kn_record_needs_the_wide_stride() {
    let mut bytes = fixture("record_4kn.bin", || {
        RecordSpec::new(16)
            .attr(std_info_attr(FT_2024, FT_2024, 0))
            .attr(file_name_attr(5, 1, "sector-native.txt"))
            .attr(resident_attr(0x80, &[0x42; 100]))
            .build(GEOMETRY_4KN)
    });

    // The right stride repairs it…
    let mut repaired = bytes.clone();
    let record = Record::parse(&mut repaired, 4096).expect("parses at 4096");
    let fname = record
        .attributes()
        .find(|a| a.kind() == AttributeType::FileName)
        .and_then(|a| a.resident_value())
        .and_then(FileName::parse)
        .unwrap();
    assert_eq!(fname.to_name(), "sector-native.txt");

    // …and the legacy stride is rejected rather than silently mangling it.
    let header = RecordHeader::parse(&bytes).unwrap();
    assert!(apply_fixup(&mut bytes, &header, 512).is_err());
}
