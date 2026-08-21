//! Locating the MFT, end to end, against a synthetic volume image.
//!
//! Builds a small NTFS-shaped image on disk, reads it back through the real
//! [`FileBlockSource`], parses the boot sector, and checks that the extent map
//! points at the bytes actually written there. Runs on any platform, so the
//! offset arithmetic - the part that silently corrupts a scan when it is wrong -
//! is covered on Linux and macOS CI where no real volume exists.

use std::fs;
use std::path::{Path, PathBuf};

use emfit_core::parser::block::{BlockSource, FileBlockSource};
use emfit_core::parser::ntfs::runs::{self, DataRun};
use emfit_core::parser::ntfs::{BootSector, MftExtents};

const SECTOR: u32 = 512;
const CLUSTER: u32 = 4096;
const RECORD: u32 = 1024;
const RECORDS_PER_CLUSTER: u64 = (CLUSTER / RECORD) as u64;

/// Where the MFT's first fragment starts, in clusters.
const MFT_LCN: u64 = 4;
/// A partition offset, so the test also proves reads are partition-relative.
const PARTITION_OFFSET: u64 = 64 * 1024;
const IMAGE_CLUSTERS: u64 = 48;

// ---------------------------------------------------------------------------
// image construction
// ---------------------------------------------------------------------------

fn boot_sector_bytes() -> Vec<u8> {
    let mut buf = vec![0u8; SECTOR as usize];
    buf[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
    buf[3..11].copy_from_slice(b"NTFS    ");
    buf[0x0B..0x0D].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    buf[0x0D] = (CLUSTER / SECTOR) as u8;
    buf[0x15] = 0xF8;
    buf[0x28..0x30].copy_from_slice(&(IMAGE_CLUSTERS * u64::from(CLUSTER / SECTOR)).to_le_bytes());
    buf[0x30..0x38].copy_from_slice(&MFT_LCN.to_le_bytes());
    buf[0x38..0x40].copy_from_slice(&2u64.to_le_bytes());
    buf[0x40] = 0xF6; // -10 -> 1024-byte records
    buf[0x48..0x50].copy_from_slice(&0xDEAD_BEEF_CAFE_F00Du64.to_le_bytes());
    buf[510] = 0x55;
    buf[511] = 0xAA;
    buf
}

/// A recognizable 1 KB record body, so a wrong offset produces a wrong tag
/// rather than plausible-looking garbage.
fn record_tag(record_number: u64) -> Vec<u8> {
    let mut buf = vec![0u8; RECORD as usize];
    let tag = format!("RECORD-{record_number:06}");
    buf[..tag.len()].copy_from_slice(tag.as_bytes());
    buf
}

/// The MFT's fragments in this image: deliberately three pieces, so the
/// boundary arithmetic is exercised rather than assumed.
fn fragments() -> Vec<(u64, u64)> {
    vec![
        (MFT_LCN, 2), // VCN 0..2   -> records 0..8
        (20, 3),      // VCN 2..5   -> records 8..20
        (40, 1),      // VCN 5..6   -> records 20..24
    ]
}

fn image(tag: &str) -> (PathBuf, MftExtents) {
    let mut dir = std::env::temp_dir();
    dir.push(format!("emfit-ntfs-test-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("{tag}.img"));

    let volume_len = (IMAGE_CLUSTERS * u64::from(CLUSTER)) as usize;
    let mut volume = vec![0u8; volume_len];

    // Boot sector at the start of the *partition*.
    volume[..SECTOR as usize].copy_from_slice(&boot_sector_bytes());

    // Lay tagged records into each fragment, in virtual order.
    let map = MftExtents::new(
        fragments()
            .iter()
            .scan(0u64, |vcn, &(lcn, count)| {
                let extent = emfit_core::parser::ntfs::Extent {
                    vcn: *vcn,
                    lcn,
                    cluster_count: count,
                };
                *vcn += count;
                Some(extent)
            })
            .collect(),
        CLUSTER,
        RECORD,
    )
    .expect("fragment map is well formed");

    for record in 0..map.capacity_records() {
        let at = map.locate(record).expect("mapped").byte_offset as usize;
        volume[at..at + RECORD as usize].copy_from_slice(&record_tag(record));
    }

    // Prefix with junk so offset 0 of the partition is not offset 0 of the
    // file - a source that ignores its base offset then reads the junk.
    let mut file = vec![0xEEu8; PARTITION_OFFSET as usize];
    file.extend_from_slice(&volume);
    fs::write(&path, &file).expect("write image");

    (path, map)
}

fn open(path: &Path) -> FileBlockSource {
    FileBlockSource::open_image(path)
        .expect("open image")
        .partition(PARTITION_OFFSET, Some(IMAGE_CLUSTERS * u64::from(CLUSTER)))
        .with_sector_size(SECTOR)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[test]
fn reads_and_parses_the_boot_sector_through_a_partition() {
    let (path, _) = image("boot");
    let src = open(&path);

    let mut buf = vec![0u8; SECTOR as usize];
    src.read_exact_at(0, &mut buf).unwrap();

    let boot = BootSector::parse(&buf).unwrap();
    assert_eq!(boot.bytes_per_sector, SECTOR);
    assert_eq!(boot.bytes_per_cluster, CLUSTER);
    assert_eq!(boot.bytes_per_record, RECORD);
    assert_eq!(boot.mft_start_lcn, MFT_LCN);
    assert_eq!(boot.serial, 0xDEAD_BEEF_CAFE_F00D);

    // Volume-relative, not file-relative: the MFT is at cluster 4 of the
    // partition, which is 64 KiB further into the file.
    assert_eq!(boot.mft_byte_offset(), MFT_LCN * u64::from(CLUSTER));
}

#[test]
fn the_bootstrap_map_finds_record_zero() {
    // Before the real extent map is known, all we have is the boot sector's
    // mft_start_lcn - which is enough to read the first fragment.
    let (path, _) = image("bootstrap");
    let src = open(&path);

    let mut sector = vec![0u8; SECTOR as usize];
    src.read_exact_at(0, &mut sector).unwrap();
    let boot = BootSector::parse(&sector).unwrap();

    let bootstrap = MftExtents::contiguous(
        boot.mft_start_lcn,
        1,
        boot.bytes_per_cluster,
        boot.bytes_per_record,
    )
    .unwrap();

    let loc = bootstrap.locate(0).unwrap();
    assert_eq!(loc.contiguous_records, RECORDS_PER_CLUSTER);

    let mut buf = vec![0u8; RECORD as usize];
    src.read_exact_at(loc.byte_offset, &mut buf).unwrap();
    assert_eq!(&buf[..13], b"RECORD-000000");
}

#[test]
fn every_record_in_a_fragmented_mft_is_where_the_map_says() {
    // The end-to-end property: locate a record, read those bytes off the
    // image, and get the record that was written there. A one-fragment
    // off-by-one shows up as the wrong tag rather than as plausible garbage.
    let (path, map) = image("all-records");
    let src = open(&path);

    assert_eq!(map.fragment_count(), 3);
    assert_eq!(map.capacity_records(), 24);

    let mut buf = vec![0u8; RECORD as usize];
    for record in 0..map.capacity_records() {
        let loc = map.locate(record).unwrap();
        src.read_exact_at(loc.byte_offset, &mut buf).unwrap();

        let expected = format!("RECORD-{record:06}");
        assert_eq!(
            &buf[..expected.len()],
            expected.as_bytes(),
            "record {record} read from byte {}",
            loc.byte_offset
        );
    }
}

#[test]
fn a_batch_clamped_to_the_run_length_reads_only_real_records() {
    // What the bulk reader will do: ask for a location, read
    // `contiguous_records` worth in one I/O, and trust every record in it.
    // v1 ignored the run length and read `count x 1024` straight through,
    // which past a fragment boundary is unrelated disk content.
    let (path, map) = image("batched");
    let src = open(&path);

    let mut record = 0u64;
    let mut batches = 0;

    while record < map.capacity_records() {
        let loc = map.locate(record).unwrap();
        assert!(loc.contiguous_records > 0);

        let len = (loc.contiguous_records * u64::from(RECORD)) as usize;
        let mut batch = vec![0u8; len];
        src.read_exact_at(loc.byte_offset, &mut batch).unwrap();

        for i in 0..loc.contiguous_records {
            let at = (i * u64::from(RECORD)) as usize;
            let expected = format!("RECORD-{:06}", record + i);
            assert_eq!(
                &batch[at..at + expected.len()],
                expected.as_bytes(),
                "record {} within the batch starting at {record}",
                record + i
            );
        }

        record += loc.contiguous_records;
        batches += 1;
    }

    assert_eq!(
        batches, 3,
        "one batch per fragment - the clamp split the read exactly at the boundaries"
    );
}

#[test]
fn an_unclamped_batch_would_have_read_the_wrong_bytes() {
    // Proves the clamp is load-bearing rather than decorative: reading past
    // the first fragment as if it were contiguous yields the wrong record, and
    // nothing about the read itself fails.
    let (path, map) = image("unclamped");
    let src = open(&path);

    let first = map.locate(0).unwrap();
    assert_eq!(
        first.contiguous_records, 8,
        "fragment one holds eight records"
    );

    // Ask for twelve anyway, as v1 would have.
    let mut over = vec![0u8; 12 * RECORD as usize];
    src.read_exact_at(first.byte_offset, &mut over).unwrap();

    let at = 8 * RECORD as usize;
    assert_ne!(
        &over[at..at + 13],
        b"RECORD-000008",
        "record 8 lives in another fragment; an unclamped read cannot have found it"
    );
}

#[test]
fn the_extent_map_round_trips_through_a_data_run_list() {
    // The fallback path: no filesystem driver to ask, so the map comes from
    // decoding record 0's $DATA run list. It must agree with the map the
    // driver would have given.
    let (_, from_driver) = image("runs");

    // Encode the same fragments as NTFS would, with signed deltas.
    let mut encoded = Vec::new();
    let mut previous_lcn = 0i64;
    for extent in from_driver.extents() {
        let delta = extent.lcn as i64 - previous_lcn;
        previous_lcn = extent.lcn as i64;
        encoded.push(0x11); // one length byte, one offset byte
        encoded.push(extent.cluster_count as u8);
        encoded.push(delta as i8 as u8);
    }
    encoded.push(0x00);

    let (decoded, complete) = runs::decode(&encoded);
    assert!(complete);
    assert_eq!(decoded.len(), from_driver.fragment_count());

    let from_runs = MftExtents::from_data_runs(&decoded, CLUSTER, RECORD).unwrap();
    assert_eq!(
        from_runs, from_driver,
        "both routes to the MFT must agree exactly"
    );

    // And the reconstructed map still finds real records.
    let src = open(&image("runs-read").0);
    let mut buf = vec![0u8; RECORD as usize];
    for record in [0u64, 7, 8, 19, 20, 23] {
        let loc = from_runs.locate(record).unwrap();
        src.read_exact_at(loc.byte_offset, &mut buf).unwrap();
        let expected = format!("RECORD-{record:06}");
        assert_eq!(&buf[..expected.len()], expected.as_bytes());
    }
}

#[test]
fn a_sparse_data_run_list_would_be_rejected() {
    // Not reachable from a healthy $MFT, but the failure must be an error
    // rather than a map that shifts every record after the hole.
    let sparse = vec![
        DataRun {
            lcn: Some(MFT_LCN),
            cluster_count: 2,
        },
        DataRun {
            lcn: None,
            cluster_count: 2,
        },
        DataRun {
            lcn: Some(20),
            cluster_count: 2,
        },
    ];
    assert!(MftExtents::from_data_runs(&sparse, CLUSTER, RECORD).is_err());
}
