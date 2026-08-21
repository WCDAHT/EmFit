//! One MFT record: validate it, repair it, walk its attributes.
//!
//! A record is a 48-byte header followed by a chain of self-describing
//! attributes, each carrying its own length. Everything about a file is in
//! there - name, parent, timestamps, and either its contents or a map of where
//! its contents are.
//!
//! # The fixup array, and why skipping it corrupts names
//!
//! Before writing a record, NTFS overwrites **the last two bytes of every
//! sector** with a check value, and stashes the displaced bytes in a side
//! array. On read they must be put back. The point is torn-write detection: if
//! the machine lost power mid-write, some sectors will carry the previous
//! check value and the damage is detectable instead of silent.
//!
//! Two bytes per 512 lands roughly in the middle of a 1024-byte record, which
//! is exactly where filenames live. Skip the repair and names come out with
//! two bytes of garbage in the middle - plausible enough to survive parsing
//! and wrong enough to be useless.
//!
//! Records obtained through `FSCTL_GET_NTFS_FILE_RECORD` are repaired by the
//! driver already. We always read raw, so we always repair.

use crate::error::{Error, Result};
use crate::parser::ntfs::attr::AttributeIter;

/// Every MFT record starts with this.
pub const RECORD_SIGNATURE: &[u8; 4] = b"FILE";

/// A record NTFS gave up on and marked, rather than a record we misread.
const BAAD_SIGNATURE: &[u8; 4] = b"BAAD";

/// Length of the fixed part of the header.
const HEADER_LEN: usize = 48;

/// Record is allocated to a file. Without it the record is free space that
/// still holds a previous file's bytes.
const FLAG_IN_USE: u16 = 0x0001;
/// Record describes a directory.
const FLAG_DIRECTORY: u16 = 0x0002;

/// The fixed header at the start of every MFT record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordHeader {
    /// Byte offset of the update sequence array.
    pub update_sequence_offset: u16,
    /// Entries in that array: the check value plus one per sector.
    pub update_sequence_count: u16,
    /// Bumped every time this record is reused for a different file.
    /// Together with the record number it forms a file reference that cannot
    /// be confused with a stale one.
    pub sequence_number: u16,
    /// How many names point at this file. Above one means hard links.
    pub hard_link_count: u16,
    /// Byte offset of the first attribute.
    pub first_attribute_offset: u16,
    pub flags: u16,
    /// Bytes of this record actually used.
    pub used_size: u32,
    /// Bytes allocated to it - the record size.
    pub allocated_size: u32,
    /// Reference to the base record, when this is an extension record holding
    /// attributes that overflowed. Zero when this *is* a base record.
    pub base_record_reference: u64,
    /// The record's own number. Present since Windows XP; older volumes leave
    /// it zero, so it is a cross-check rather than a source of truth.
    pub record_number: u32,
}

impl RecordHeader {
    /// Parse and validate the header. Cheap: reads a few fields, mutates
    /// nothing, allocates nothing.
    ///
    /// The sweep calls this on every record and skips most of them without
    /// paying for fixup or attribute walking - on a used volume a large share
    /// of the MFT is free records holding stale bytes.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < HEADER_LEN {
            return Err(Error::Parse {
                format: "MFT record".to_string(),
                message: format!("need {HEADER_LEN} bytes, got {}", buf.len()),
            });
        }

        let signature = &buf[0..4];
        if signature != RECORD_SIGNATURE {
            let message = if signature == BAAD_SIGNATURE {
                "record marked BAAD; NTFS detected corruption here".to_string()
            } else {
                format!("bad signature {:02X?}", signature)
            };
            return Err(Error::Parse {
                format: "MFT record".to_string(),
                message,
            });
        }

        Ok(Self {
            update_sequence_offset: read_u16(buf, 0x04),
            update_sequence_count: read_u16(buf, 0x06),
            sequence_number: read_u16(buf, 0x10),
            hard_link_count: read_u16(buf, 0x12),
            first_attribute_offset: read_u16(buf, 0x14),
            flags: read_u16(buf, 0x16),
            used_size: read_u32(buf, 0x18),
            allocated_size: read_u32(buf, 0x1C),
            base_record_reference: read_u64(buf, 0x20),
            record_number: read_u32(buf, 0x2C),
        })
    }

    /// Whether this record currently describes a file. Free records keep their
    /// old contents until reused, so their attributes parse fine and describe
    /// a file that no longer exists.
    pub fn is_in_use(&self) -> bool {
        self.flags & FLAG_IN_USE != 0
    }

    pub fn is_directory(&self) -> bool {
        self.flags & FLAG_DIRECTORY != 0
    }

    /// The base record this one extends, or `None` when this is itself a base
    /// record.
    ///
    /// A file with more attributes than fit in 1024 bytes - many hard links,
    /// a heavily fragmented `$DATA` - spills into extension records. Those
    /// carry a full header and appear in the sweep like any other record, but
    /// belong to the file named here.
    pub fn base_record(&self) -> Option<u64> {
        let record = self.base_record_reference & 0x0000_FFFF_FFFF_FFFF;
        (record != 0).then_some(record)
    }

    /// The 48-bit record number plus 16-bit sequence, as Windows file APIs
    /// want it.
    pub fn file_reference(&self, record_number: u64) -> u64 {
        (record_number & 0x0000_FFFF_FFFF_FFFF) | (u64::from(self.sequence_number) << 48)
    }
}

/// A validated, repaired MFT record.
#[derive(Debug)]
pub struct Record<'a> {
    header: RecordHeader,
    buf: &'a [u8],
}

impl<'a> Record<'a> {
    /// Repair `buf` in place and wrap it.
    ///
    /// `bytes_per_sector` comes from the boot sector; the fixup stride is a
    /// sector, and assuming 512 silently mangles every record on a native 4Kn
    /// drive.
    pub fn parse(buf: &'a mut [u8], bytes_per_sector: u32) -> Result<Self> {
        let header = RecordHeader::parse(buf)?;
        apply_fixup(buf, &header, bytes_per_sector)?;
        Self::from_repaired(buf, header)
    }

    /// Wrap a record whose fixup has already been applied.
    pub fn from_repaired(buf: &'a [u8], header: RecordHeader) -> Result<Self> {
        let first = usize::from(header.first_attribute_offset);
        if first < HEADER_LEN || first >= buf.len() {
            return Err(Error::Parse {
                format: "MFT record".to_string(),
                message: format!("first attribute at {first} is outside the record"),
            });
        }
        Ok(Self { header, buf })
    }

    pub fn header(&self) -> &RecordHeader {
        &self.header
    }

    pub fn is_in_use(&self) -> bool {
        self.header.is_in_use()
    }

    pub fn is_directory(&self) -> bool {
        self.header.is_directory()
    }

    /// Walk the attributes.
    ///
    /// Bounded by `used_size` rather than the whole record: the space past it
    /// is stale bytes from whatever lived here before, and following a length
    /// field out of it walks into nonsense.
    pub fn attributes(&self) -> AttributeIter<'a> {
        let used = (self.header.used_size as usize).min(self.buf.len());
        AttributeIter::new(
            &self.buf[..used],
            usize::from(self.header.first_attribute_offset),
        )
    }

    /// The record's bytes, after repair.
    pub fn bytes(&self) -> &'a [u8] {
        self.buf
    }
}

/// Put back the bytes NTFS displaced with its check value.
///
/// Verifies each sector's check value before restoring. A mismatch means a
/// torn write - the record is a mixture of two generations and cannot be
/// trusted, which is exactly what the mechanism exists to detect. WizTree
/// restores without verifying, trading that detection for a couple of
/// comparisons per record; we keep it, because a wrong filename that looks
/// right is worse than a skipped file.
pub fn apply_fixup(buf: &mut [u8], header: &RecordHeader, bytes_per_sector: u32) -> Result<()> {
    let usa_offset = usize::from(header.update_sequence_offset);
    let usa_count = usize::from(header.update_sequence_count);
    let sector = bytes_per_sector as usize;

    if sector < 4 {
        return Err(Error::Parse {
            format: "MFT record fixup".to_string(),
            message: format!("implausible sector size {sector}"),
        });
    }
    // One check value plus one entry per sector.
    if usa_count < 2 {
        return Err(Error::Parse {
            format: "MFT record fixup".to_string(),
            message: format!("update sequence array has {usa_count} entries"),
        });
    }
    if usa_offset < HEADER_LEN || usa_offset + usa_count * 2 > buf.len() {
        return Err(Error::Parse {
            format: "MFT record fixup".to_string(),
            message: format!("array at {usa_offset} x {usa_count} runs outside the record"),
        });
    }

    let expected_count = buf.len().div_ceil(sector) + 1;
    if usa_count != expected_count {
        return Err(Error::Parse {
            format: "MFT record fixup".to_string(),
            message: format!(
                "array has {usa_count} entries but a {}-byte record of {sector}-byte sectors \
                 needs {expected_count}",
                buf.len()
            ),
        });
    }

    let check = read_u16(buf, usa_offset);

    for i in 1..usa_count {
        let tail = i * sector - 2;
        if tail + 2 > buf.len() {
            break;
        }

        let found = read_u16(buf, tail);
        if found != check {
            return Err(Error::Parse {
                format: "MFT record fixup".to_string(),
                message: format!(
                    "sector {i} ends with {found:#06X}, expected {check:#06X} - torn write"
                ),
            });
        }

        let saved = read_u16(buf, usa_offset + i * 2);
        buf[tail..tail + 2].copy_from_slice(&saved.to_le_bytes());
    }

    Ok(())
}

fn read_u16(buf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([buf[at], buf[at + 1]])
}

fn read_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

fn read_u64(buf: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[at..at + 8]);
    u64::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECORD: usize = 1024;
    const SECTOR: u32 = 512;
    const USA_OFFSET: usize = 0x30;
    const CHECK: u16 = 0xBEEF;

    /// A record with a well-formed header, a valid fixup array, and no
    /// attributes. `tail_bytes` are the originals the fixup will restore.
    fn record(tail_bytes: [[u8; 2]; 2]) -> Vec<u8> {
        let mut buf = vec![0u8; RECORD];
        buf[0..4].copy_from_slice(RECORD_SIGNATURE);
        buf[0x04..0x06].copy_from_slice(&(USA_OFFSET as u16).to_le_bytes());
        buf[0x06..0x08].copy_from_slice(&3u16.to_le_bytes()); // check + 2 sectors
        buf[0x10..0x12].copy_from_slice(&7u16.to_le_bytes()); // sequence
        buf[0x12..0x14].copy_from_slice(&1u16.to_le_bytes()); // hard links
        buf[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes()); // first attribute
        buf[0x16..0x18].copy_from_slice(&FLAG_IN_USE.to_le_bytes());
        buf[0x18..0x1C].copy_from_slice(&64u32.to_le_bytes()); // used
        buf[0x1C..0x20].copy_from_slice(&(RECORD as u32).to_le_bytes());
        buf[0x2C..0x30].copy_from_slice(&42u32.to_le_bytes());

        // The array: check value, then the displaced originals.
        buf[USA_OFFSET..USA_OFFSET + 2].copy_from_slice(&CHECK.to_le_bytes());
        buf[USA_OFFSET + 2..USA_OFFSET + 4].copy_from_slice(&tail_bytes[0]);
        buf[USA_OFFSET + 4..USA_OFFSET + 6].copy_from_slice(&tail_bytes[1]);

        // Each sector currently ends with the check value.
        buf[510..512].copy_from_slice(&CHECK.to_le_bytes());
        buf[1022..1024].copy_from_slice(&CHECK.to_le_bytes());
        buf
    }

    #[test]
    fn parses_a_well_formed_header() {
        let buf = record([[0xAA, 0xBB], [0xCC, 0xDD]]);
        let h = RecordHeader::parse(&buf).unwrap();

        assert_eq!(h.sequence_number, 7);
        assert_eq!(h.hard_link_count, 1);
        assert_eq!(h.first_attribute_offset, 0x38);
        assert_eq!(h.used_size, 64);
        assert_eq!(h.allocated_size, RECORD as u32);
        assert_eq!(h.record_number, 42);
        assert!(h.is_in_use());
        assert!(!h.is_directory());
        assert_eq!(h.base_record(), None, "this is a base record");
    }

    #[test]
    fn rejects_anything_without_the_signature() {
        let mut buf = record([[0, 0], [0, 0]]);
        buf[0..4].copy_from_slice(b"HOLE");
        assert!(RecordHeader::parse(&buf).is_err());

        // A free record's slot is often all zeroes.
        assert!(RecordHeader::parse(&[0u8; RECORD]).is_err());
        assert!(RecordHeader::parse(&[]).is_err());
    }

    #[test]
    fn a_baad_record_says_so() {
        let mut buf = record([[0, 0], [0, 0]]);
        buf[0..4].copy_from_slice(BAAD_SIGNATURE);
        let err = RecordHeader::parse(&buf).unwrap_err();
        assert!(err.to_string().contains("BAAD"), "{err}");
    }

    #[test]
    fn fixup_restores_the_displaced_bytes() {
        let mut buf = record([[0xAA, 0xBB], [0xCC, 0xDD]]);
        let header = RecordHeader::parse(&buf).unwrap();

        // Before: both sectors end with the check value.
        assert_eq!(&buf[510..512], &CHECK.to_le_bytes());
        assert_eq!(&buf[1022..1024], &CHECK.to_le_bytes());

        apply_fixup(&mut buf, &header, SECTOR).unwrap();

        assert_eq!(&buf[510..512], &[0xAA, 0xBB], "sector 1 restored");
        assert_eq!(&buf[1022..1024], &[0xCC, 0xDD], "sector 2 restored");
    }

    #[test]
    fn fixup_detects_a_torn_write() {
        let mut buf = record([[0xAA, 0xBB], [0xCC, 0xDD]]);
        // The second sector was never written: it carries the old check value.
        buf[1022..1024].copy_from_slice(&0x1234u16.to_le_bytes());

        let header = RecordHeader::parse(&buf).unwrap();
        let err = apply_fixup(&mut buf, &header, SECTOR).unwrap_err();
        assert!(err.to_string().contains("torn write"), "{err}");
    }

    #[test]
    fn the_fixup_stride_follows_the_sector_size() {
        // A 4Kn drive: 4096-byte sectors and 4096-byte records, so exactly one
        // fixup at the end. Assuming 512 would write into the middle of the
        // record instead.
        let mut buf = vec![0u8; 4096];
        buf[0..4].copy_from_slice(RECORD_SIGNATURE);
        buf[0x04..0x06].copy_from_slice(&(USA_OFFSET as u16).to_le_bytes());
        buf[0x06..0x08].copy_from_slice(&2u16.to_le_bytes()); // check + 1 sector
        buf[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        buf[0x16..0x18].copy_from_slice(&FLAG_IN_USE.to_le_bytes());
        buf[0x18..0x1C].copy_from_slice(&64u32.to_le_bytes());
        buf[USA_OFFSET..USA_OFFSET + 2].copy_from_slice(&CHECK.to_le_bytes());
        buf[USA_OFFSET + 2..USA_OFFSET + 4].copy_from_slice(&[0x11, 0x22]);
        buf[4094..4096].copy_from_slice(&CHECK.to_le_bytes());

        let header = RecordHeader::parse(&buf).unwrap();
        apply_fixup(&mut buf, &header, 4096).unwrap();
        assert_eq!(&buf[4094..4096], &[0x11, 0x22]);

        // The same record read as if sectors were 512 bytes is rejected rather
        // than mangled.
        let mut wrong = buf.clone();
        assert!(apply_fixup(&mut wrong, &header, 512).is_err());
    }

    #[test]
    fn a_mismatched_array_length_is_rejected() {
        let mut buf = record([[0xAA, 0xBB], [0xCC, 0xDD]]);
        buf[0x06..0x08].copy_from_slice(&5u16.to_le_bytes()); // claims 4 sectors
        let header = RecordHeader::parse(&buf).unwrap();

        let err = apply_fixup(&mut buf, &header, SECTOR).unwrap_err();
        assert!(err.to_string().contains("needs 3"), "{err}");
    }

    #[test]
    fn an_out_of_range_array_is_rejected() {
        let mut buf = record([[0, 0], [0, 0]]);
        buf[0x04..0x06].copy_from_slice(&1000u16.to_le_bytes());
        let header = RecordHeader::parse(&buf).unwrap();
        assert!(apply_fixup(&mut buf, &header, SECTOR).is_err());

        let mut inside_header = record([[0, 0], [0, 0]]);
        inside_header[0x04..0x06].copy_from_slice(&4u16.to_le_bytes());
        let header = RecordHeader::parse(&inside_header).unwrap();
        assert!(apply_fixup(&mut inside_header, &header, SECTOR).is_err());
    }

    #[test]
    fn extension_records_name_their_base() {
        let mut buf = record([[0, 0], [0, 0]]);
        // Record 300, sequence 4.
        let reference = 300u64 | (4u64 << 48);
        buf[0x20..0x28].copy_from_slice(&reference.to_le_bytes());

        let h = RecordHeader::parse(&buf).unwrap();
        assert_eq!(h.base_record(), Some(300), "sequence must be masked off");
    }

    #[test]
    fn file_references_combine_number_and_sequence() {
        let buf = record([[0, 0], [0, 0]]);
        let h = RecordHeader::parse(&buf).unwrap();
        assert_eq!(h.file_reference(42), 42 | (7u64 << 48));
    }

    #[test]
    fn free_records_are_identifiable_without_repairing_them() {
        let mut buf = record([[0, 0], [0, 0]]);
        buf[0x16..0x18].copy_from_slice(&0u16.to_le_bytes());

        let h = RecordHeader::parse(&buf).unwrap();
        assert!(
            !h.is_in_use(),
            "the sweep skips these before doing any work"
        );
    }

    #[test]
    fn a_first_attribute_offset_outside_the_record_is_rejected() {
        let mut buf = record([[0, 0], [0, 0]]);
        buf[0x14..0x16].copy_from_slice(&5000u16.to_le_bytes());
        let header = RecordHeader::parse(&buf).unwrap();
        assert!(Record::from_repaired(&buf, header).is_err());

        let mut into_header = record([[0, 0], [0, 0]]);
        into_header[0x14..0x16].copy_from_slice(&8u16.to_le_bytes());
        let header = RecordHeader::parse(&into_header).unwrap();
        assert!(Record::from_repaired(&into_header, header).is_err());
    }
}
