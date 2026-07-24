//! Byte-source reads, through the public API only.
//!
//! Everything here runs against a disk image on any platform, so the block
//! layer is covered on Linux and macOS CI where no raw volume exists.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use emfit_core::error::Error;
use emfit_core::parser::block::{AlignedBuf, BlockSource, DEFAULT_SECTOR_SIZE, FileBlockSource};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

const SECTOR: usize = DEFAULT_SECTOR_SIZE as usize;

/// A test image whose every byte is a function of its offset, so a read at any
/// position can be checked without a copy of the expected data.
fn pattern(offset: usize) -> u8 {
    (offset % 251) as u8 // prime, so the cycle never aligns with sector size
}

fn image(tag: &str, len: usize) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("emfit-block-test-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("{tag}.img"));

    let bytes: Vec<u8> = (0..len).map(pattern).collect();
    fs::write(&path, &bytes).expect("write test image");
    path
}

fn expect_pattern(buf: &[u8], from: usize) {
    for (i, &got) in buf.iter().enumerate() {
        assert_eq!(got, pattern(from + i), "byte {} of the read", i);
    }
}

// ---------------------------------------------------------------------------
// basic reads
// ---------------------------------------------------------------------------

#[test]
fn reads_at_an_offset_without_a_cursor() {
    let path = image("basic", 8 * SECTOR);
    let src = FileBlockSource::open_image(&path).unwrap();

    let mut buf = vec![0u8; SECTOR];

    // Out of order, to prove no cursor is involved.
    assert_eq!(src.read_at(4 * SECTOR as u64, &mut buf).unwrap(), SECTOR);
    expect_pattern(&buf, 4 * SECTOR);

    assert_eq!(src.read_at(0, &mut buf).unwrap(), SECTOR);
    expect_pattern(&buf, 0);

    // Repeating a read gives the same bytes.
    assert_eq!(src.read_at(4 * SECTOR as u64, &mut buf).unwrap(), SECTOR);
    expect_pattern(&buf, 4 * SECTOR);
}

#[test]
fn unaligned_offsets_and_lengths_are_fine_on_a_buffered_source() {
    let path = image("unaligned", 4 * SECTOR);
    let src = FileBlockSource::open_image(&path).unwrap();

    let mut buf = vec![0u8; 37];
    assert_eq!(src.read_at(13, &mut buf).unwrap(), 37);
    expect_pattern(&buf, 13);
}

#[test]
fn span_is_reported_for_an_image() {
    let path = image("span", 3 * SECTOR);
    let src = FileBlockSource::open_image(&path).unwrap();
    assert_eq!(src.span(), Some(3 * SECTOR as u64));
    assert_eq!(src.sector_size(), DEFAULT_SECTOR_SIZE);
    assert!(!src.requires_alignment());
}

#[test]
fn missing_file_reports_the_path() {
    let mut path = std::env::temp_dir();
    path.push("emfit-does-not-exist-9d3f.img");
    let err = FileBlockSource::open_image(&path).unwrap_err();

    assert!(matches!(err, Error::Device { .. }), "got {err:?}");
    assert!(
        err.to_string().contains("emfit-does-not-exist"),
        "error should name the path: {err}"
    );
}

// ---------------------------------------------------------------------------
// partitions
// ---------------------------------------------------------------------------

#[test]
fn a_partition_is_addressed_from_zero() {
    let path = image("partition", 8 * SECTOR);
    let base = 2 * SECTOR;
    let src = FileBlockSource::open_image(&path)
        .unwrap()
        .partition(base as u64, Some(4 * SECTOR as u64));

    let mut buf = vec![0u8; SECTOR];
    // Offset 0 of the partition is `base` bytes into the image.
    assert_eq!(src.read_at(0, &mut buf).unwrap(), SECTOR);
    expect_pattern(&buf, base);

    assert_eq!(src.read_at(SECTOR as u64, &mut buf).unwrap(), SECTOR);
    expect_pattern(&buf, base + SECTOR);
}

#[test]
fn reads_never_report_bytes_past_the_partition() {
    // A four-sector partition inside an eight-sector image. A sector-aligned
    // tail read physically overlaps the next partition; those bytes must not
    // be counted as ours.
    let path = image("clamp", 8 * SECTOR);
    let span = 4 * SECTOR;
    let src = FileBlockSource::open_image(&path)
        .unwrap()
        .partition(0, Some(span as u64 - 100));

    let mut buf = vec![0u8; SECTOR];
    let last_sector = (3 * SECTOR) as u64;
    let got = src.read_at(last_sector, &mut buf).unwrap();

    assert_eq!(
        got,
        SECTOR - 100,
        "only the bytes inside the partition are reported"
    );
    expect_pattern(&buf[..got], 3 * SECTOR);
}

#[test]
fn reading_beyond_the_span_is_end_of_source_not_an_error() {
    let path = image("beyond", 8 * SECTOR);
    let src = FileBlockSource::open_image(&path)
        .unwrap()
        .partition(0, Some(2 * SECTOR as u64));

    let mut buf = vec![0u8; SECTOR];
    assert_eq!(src.read_at(2 * SECTOR as u64, &mut buf).unwrap(), 0);
    assert_eq!(src.read_at(99 * SECTOR as u64, &mut buf).unwrap(), 0);
}

// ---------------------------------------------------------------------------
// read_exact_at
// ---------------------------------------------------------------------------

#[test]
fn read_exact_at_fills_the_whole_buffer() {
    let path = image("exact", 8 * SECTOR);
    let src = FileBlockSource::open_image(&path).unwrap();

    let mut buf = vec![0u8; 3 * SECTOR];
    src.read_exact_at(SECTOR as u64, &mut buf).unwrap();
    expect_pattern(&buf, SECTOR);
}

#[test]
fn read_exact_at_fails_rather_than_returning_short() {
    let path = image("exact-eof", 2 * SECTOR);
    let src = FileBlockSource::open_image(&path).unwrap();

    let mut buf = vec![0u8; 4 * SECTOR];
    let err = src.read_exact_at(0, &mut buf).unwrap_err();

    match err {
        Error::ShortRead { wanted, got, .. } => {
            assert_eq!(wanted, 4 * SECTOR);
            assert_eq!(got, 2 * SECTOR);
        }
        other => panic!("expected ShortRead, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// alignment
// ---------------------------------------------------------------------------

#[test]
fn an_aligned_source_rejects_a_misaligned_offset() {
    let path = image("align-offset", 8 * SECTOR);
    let src = FileBlockSource::open_image(&path)
        .unwrap()
        .with_alignment_required(true);

    let mut buf = AlignedBuf::for_source(&src, SECTOR);
    let err = src.read_at(100, buf.as_mut_slice()).unwrap_err();

    assert!(
        matches!(err, Error::Misaligned { offset: 100, .. }),
        "got {err:?}"
    );
    assert!(
        err.to_string().contains("512-byte sector"),
        "message should name the sector size: {err}"
    );
}

#[test]
fn an_aligned_source_rejects_a_misaligned_length() {
    let path = image("align-len", 8 * SECTOR);
    let src = FileBlockSource::open_image(&path)
        .unwrap()
        .with_alignment_required(true);

    let mut buf = AlignedBuf::new(SECTOR, SECTOR);
    let err = src.read_at(0, &mut buf.as_mut_slice()[..300]).unwrap_err();
    assert!(
        matches!(err, Error::Misaligned { len: 300, .. }),
        "got {err:?}"
    );
}

#[test]
fn an_aligned_source_accepts_an_aligned_buffer() {
    let path = image("align-ok", 8 * SECTOR);
    let src = FileBlockSource::open_image(&path)
        .unwrap()
        .with_alignment_required(true);

    let mut buf = AlignedBuf::for_source(&src, 2 * SECTOR);
    let got = src.read_at(SECTOR as u64, buf.as_mut_slice()).unwrap();

    assert_eq!(got, 2 * SECTOR);
    expect_pattern(buf.as_slice(), SECTOR);
}

#[test]
fn a_4kn_sector_size_changes_what_counts_as_aligned() {
    let path = image("4kn", 16 * SECTOR);
    let src = FileBlockSource::open_image(&path)
        .unwrap()
        .with_sector_size(4096)
        .with_alignment_required(true);

    assert_eq!(src.sector_size(), 4096);

    // 512-aligned is no longer good enough.
    let mut small = AlignedBuf::new(4096, 4096);
    assert!(src.read_at(512, small.as_mut_slice()).is_err());

    // 4096-aligned is.
    assert_eq!(src.read_at(4096, small.as_mut_slice()).unwrap(), 4096);
    expect_pattern(small.as_slice(), 4096);
}

// ---------------------------------------------------------------------------
// concurrency
// ---------------------------------------------------------------------------

#[test]
fn many_threads_read_one_source_without_a_lock() {
    // The property the whole design rests on: positional reads take &self, so
    // parallel scanners need no mutex around the handle. v1 could not do this.
    const THREADS: usize = 8;
    const SECTORS: usize = 64;

    let path = image("threads", SECTORS * SECTOR);
    let src = Arc::new(FileBlockSource::open_image(&path).unwrap());

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let src = Arc::clone(&src);
            std::thread::spawn(move || {
                let mut buf = vec![0u8; SECTOR];
                // Each thread sweeps the whole image on its own schedule.
                for i in 0..SECTORS {
                    let offset = ((i + t * 7) % SECTORS) * SECTOR;
                    let got = src.read_at(offset as u64, &mut buf).unwrap();
                    assert_eq!(got, SECTOR);
                    for (j, &b) in buf.iter().enumerate() {
                        assert_eq!(b, pattern(offset + j), "thread {t}, sector {i}");
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("no thread saw torn or misplaced bytes");
    }
}

#[test]
fn a_source_can_be_used_as_a_trait_object() {
    // Scanners receive `&dyn BlockSource`; the trait must stay object-safe.
    let path = image("dyn", 4 * SECTOR);
    let src = FileBlockSource::open_image(&path).unwrap();
    let erased: &dyn BlockSource = &src;

    let mut buf = AlignedBuf::for_source(erased, SECTOR);
    erased.read_exact_at(0, buf.as_mut_slice()).unwrap();
    expect_pattern(buf.as_slice(), 0);
    assert_eq!(erased.label(), path.display().to_string());
}
