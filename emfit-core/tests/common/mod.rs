//! Builders for synthetic NTFS structures: records, attributes, whole images.
//!
//! Shared by the sweep integration tests and the fixture generator. Everything
//! here writes the *on-disk* format — real headers, real update-sequence
//! arrays, real run lists — so the code under test runs its production path,
//! fixups included. A shortcut here (skipping the USA, say) would quietly
//! exempt the parser from the very repair step the tests exist to prove.

// Each test binary compiles this module afresh and uses its own subset of the
// builders; what one suite leaves unused, another depends on.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use emfit_core::parser::block::FileBlockSource;

// ---------------------------------------------------------------------------
// geometry
// ---------------------------------------------------------------------------

/// The common 512n case; the 4Kn tests pass their own geometry.
pub const SECTOR: u32 = 512;
pub const CLUSTER: u32 = 4096;
pub const RECORD: u32 = 1024;

#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    pub bytes_per_sector: u32,
    pub bytes_per_cluster: u32,
    pub bytes_per_record: u32,
}

pub const GEOMETRY_512N: Geometry = Geometry {
    bytes_per_sector: SECTOR,
    bytes_per_cluster: CLUSTER,
    bytes_per_record: RECORD,
};

/// A native 4Kn disk: 4096-byte sectors, 4096-byte records, exactly one
/// fixup entry per record. The stride bug this exists to catch: applying
/// fixups every 512 bytes here corrupts the middle of every record.
pub const GEOMETRY_4KN: Geometry = Geometry {
    bytes_per_sector: 4096,
    bytes_per_cluster: 4096,
    bytes_per_record: 4096,
};

// ---------------------------------------------------------------------------
// attributes
// ---------------------------------------------------------------------------

const RESIDENT_HEADER: usize = 24;
const NON_RESIDENT_HEADER: usize = 64;

/// A resident attribute with `value` as its payload.
pub fn resident_attr(type_code: u32, value: &[u8]) -> Vec<u8> {
    let total = (RESIDENT_HEADER + value.len()).next_multiple_of(8);
    let mut buf = vec![0u8; total];
    buf[0x00..0x04].copy_from_slice(&type_code.to_le_bytes());
    buf[0x04..0x08].copy_from_slice(&(total as u32).to_le_bytes());
    buf[0x08] = 0; // resident
    buf[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
    buf[0x14..0x16].copy_from_slice(&(RESIDENT_HEADER as u16).to_le_bytes());
    buf[RESIDENT_HEADER..RESIDENT_HEADER + value.len()].copy_from_slice(value);
    buf
}

/// A *named* resident attribute — for `$DATA`, an alternate data stream.
pub fn named_resident_attr(type_code: u32, name: &str, value: &[u8]) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let name_offset = RESIDENT_HEADER;
    let value_offset = (name_offset + units.len() * 2).next_multiple_of(8);
    let total = (value_offset + value.len()).next_multiple_of(8);

    let mut buf = vec![0u8; total];
    buf[0x00..0x04].copy_from_slice(&type_code.to_le_bytes());
    buf[0x04..0x08].copy_from_slice(&(total as u32).to_le_bytes());
    buf[0x08] = 0; // resident
    buf[0x09] = units.len() as u8;
    buf[0x0A..0x0C].copy_from_slice(&(name_offset as u16).to_le_bytes());
    buf[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
    buf[0x14..0x16].copy_from_slice(&(value_offset as u16).to_le_bytes());
    for (i, unit) in units.iter().enumerate() {
        buf[name_offset + i * 2..name_offset + i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
    }
    buf[value_offset..value_offset + value.len()].copy_from_slice(value);
    buf
}

/// A non-resident attribute carrying an encoded run list.
pub fn non_resident_attr(type_code: u32, data_size: u64, allocated: u64, runs: &[u8]) -> Vec<u8> {
    build_non_resident(type_code, None, data_size, allocated, runs)
}

/// A *named* non-resident attribute — a non-resident alternate data stream.
pub fn named_non_resident_attr(
    type_code: u32,
    name: &str,
    data_size: u64,
    allocated: u64,
    runs: &[u8],
) -> Vec<u8> {
    build_non_resident(type_code, Some(name), data_size, allocated, runs)
}

fn build_non_resident(
    type_code: u32,
    name: Option<&str>,
    data_size: u64,
    allocated: u64,
    runs: &[u8],
) -> Vec<u8> {
    let units: Vec<u16> = name.unwrap_or("").encode_utf16().collect();
    let name_offset = NON_RESIDENT_HEADER;
    let runs_offset = (name_offset + units.len() * 2).next_multiple_of(8);
    let total = (runs_offset + runs.len()).next_multiple_of(8);

    let mut buf = vec![0u8; total];
    buf[0x00..0x04].copy_from_slice(&type_code.to_le_bytes());
    buf[0x04..0x08].copy_from_slice(&(total as u32).to_le_bytes());
    buf[0x08] = 1; // non-resident
    if !units.is_empty() {
        buf[0x09] = units.len() as u8;
        buf[0x0A..0x0C].copy_from_slice(&(name_offset as u16).to_le_bytes());
        for (i, unit) in units.iter().enumerate() {
            buf[name_offset + i * 2..name_offset + i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
    }
    // starting VCN 0, last VCN from the allocation.
    let clusters = allocated.div_ceil(u64::from(CLUSTER)).max(1);
    buf[0x18..0x20].copy_from_slice(&(clusters - 1).to_le_bytes());
    buf[0x20..0x22].copy_from_slice(&(runs_offset as u16).to_le_bytes());
    buf[0x28..0x30].copy_from_slice(&allocated.to_le_bytes());
    buf[0x30..0x38].copy_from_slice(&data_size.to_le_bytes());
    buf[0x38..0x40].copy_from_slice(&data_size.to_le_bytes());
    buf[runs_offset..runs_offset + runs.len()].copy_from_slice(runs);
    buf
}

/// `$STANDARD_INFORMATION` with distinct timestamps and DOS attribute bits.
pub fn std_info_attr(created_filetime: u64, modified_filetime: u64, dos_attrs: u32) -> Vec<u8> {
    let mut value = vec![0u8; 0x48];
    value[0x00..0x08].copy_from_slice(&created_filetime.to_le_bytes());
    value[0x08..0x10].copy_from_slice(&modified_filetime.to_le_bytes());
    value[0x18..0x20].copy_from_slice(&modified_filetime.to_le_bytes());
    value[0x20..0x24].copy_from_slice(&dos_attrs.to_le_bytes());
    resident_attr(0x10, &value)
}

/// `$FILE_NAME` for `name` inside directory `parent`.
pub fn file_name_attr(parent: u64, namespace: u8, name: &str) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let mut value = vec![0u8; 0x42 + units.len() * 2];
    // Parent reference carries a sequence number in the top bits, as on disk.
    value[0x00..0x08].copy_from_slice(&(parent | (1u64 << 48)).to_le_bytes());
    value[0x40] = units.len() as u8;
    value[0x41] = namespace;
    for (i, unit) in units.iter().enumerate() {
        value[0x42 + i * 2..0x44 + i * 2].copy_from_slice(&unit.to_le_bytes());
    }
    resident_attr(0x30, &value)
}

/// `$ATTRIBUTE_LIST` from `(type_code, starting_vcn, record_number)` entries.
pub fn attribute_list_attr(entries: &[(u32, u64, u64)]) -> Vec<u8> {
    let mut value = Vec::new();
    for &(type_code, vcn, record) in entries {
        let mut entry = vec![0u8; 32];
        entry[0x00..0x04].copy_from_slice(&type_code.to_le_bytes());
        entry[0x04..0x06].copy_from_slice(&32u16.to_le_bytes());
        entry[0x08..0x10].copy_from_slice(&vcn.to_le_bytes());
        entry[0x10..0x18].copy_from_slice(&(record | (1u64 << 48)).to_le_bytes());
        value.extend_from_slice(&entry);
    }
    resident_attr(0x20, &value)
}

/// Encode a run list the way NTFS stores it: per run, a header nibble pair,
/// the length, and the LCN as a signed delta from the previous run's LCN.
pub fn encode_runs(runs: &[(u64, u64)]) -> Vec<u8> {
    fn unsigned_len(v: u64) -> usize {
        ((64 - v.leading_zeros() as usize).div_ceil(8)).max(1)
    }
    fn signed_len(v: i64) -> usize {
        for n in 1..8 {
            let shift = 64 - n * 8;
            if (v << shift) >> shift == v {
                return n;
            }
        }
        8
    }

    let mut out = Vec::new();
    let mut previous = 0i64;
    for &(lcn, clusters) in runs {
        let delta = lcn as i64 - previous;
        let len_len = unsigned_len(clusters);
        let off_len = signed_len(delta);
        out.push(((off_len << 4) | len_len) as u8);
        out.extend_from_slice(&clusters.to_le_bytes()[..len_len]);
        out.extend_from_slice(&delta.to_le_bytes()[..off_len]);
        previous = lcn as i64;
    }
    out.push(0); // terminator
    out
}

// ---------------------------------------------------------------------------
// records
// ---------------------------------------------------------------------------

/// One MFT record under construction.
#[derive(Debug, Clone)]
pub struct RecordSpec {
    pub number: u64,
    pub in_use: bool,
    pub directory: bool,
    /// Non-zero makes this an extension record of that base.
    pub base_record: u64,
    pub attributes: Vec<Vec<u8>>,
}

impl RecordSpec {
    pub fn new(number: u64) -> Self {
        Self {
            number,
            in_use: true,
            directory: false,
            base_record: 0,
            attributes: Vec::new(),
        }
    }

    #[must_use]
    pub fn directory(mut self) -> Self {
        self.directory = true;
        self
    }

    #[must_use]
    pub fn free(mut self) -> Self {
        self.in_use = false;
        self
    }

    #[must_use]
    pub fn extension_of(mut self, base: u64) -> Self {
        self.base_record = base;
        self
    }

    #[must_use]
    pub fn attr(mut self, attribute: Vec<u8>) -> Self {
        self.attributes.push(attribute);
        self
    }

    /// Serialize to on-disk form: header, attributes, end marker, and a
    /// **valid update sequence array** — the check value is written over the
    /// last two bytes of every sector and the displaced originals are stashed,
    /// exactly as NTFS does, so `Record::parse` must repair the record to
    /// read it.
    pub fn build(&self, geometry: Geometry) -> Vec<u8> {
        let record_size = geometry.bytes_per_record as usize;
        let sector = geometry.bytes_per_sector as usize;
        let usa_count = record_size / sector + 1;
        let usa_offset = 0x30;
        let first_attribute = (usa_offset + usa_count * 2).next_multiple_of(8);

        let mut buf = vec![0u8; record_size];
        buf[0..4].copy_from_slice(b"FILE");
        buf[0x04..0x06].copy_from_slice(&(usa_offset as u16).to_le_bytes());
        buf[0x06..0x08].copy_from_slice(&(usa_count as u16).to_le_bytes());
        buf[0x10..0x12].copy_from_slice(&1u16.to_le_bytes()); // sequence
        buf[0x12..0x14].copy_from_slice(&1u16.to_le_bytes()); // hard links
        buf[0x14..0x16].copy_from_slice(&(first_attribute as u16).to_le_bytes());
        let mut flags = 0u16;
        if self.in_use {
            flags |= 0x0001;
        }
        if self.directory {
            flags |= 0x0002;
        }
        buf[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        buf[0x1C..0x20].copy_from_slice(&(record_size as u32).to_le_bytes());
        if self.base_record != 0 {
            let reference = self.base_record | (1u64 << 48);
            buf[0x20..0x28].copy_from_slice(&reference.to_le_bytes());
        }
        buf[0x2C..0x30].copy_from_slice(&(self.number as u32).to_le_bytes());

        let mut at = first_attribute;
        for attribute in &self.attributes {
            assert!(
                at + attribute.len() + 8 <= record_size,
                "record {} overflows: attributes need {} of {} bytes",
                self.number,
                at + attribute.len() + 8,
                record_size
            );
            buf[at..at + attribute.len()].copy_from_slice(attribute);
            at += attribute.len();
        }
        buf[at..at + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let used = (at + 8) as u32;
        buf[0x18..0x1C].copy_from_slice(&used.to_le_bytes());

        // Apply the update sequence: stash each sector's real tail in the
        // array, then overwrite the tail with the check value.
        let check: u16 = 0x5100 | (self.number as u16 & 0xFF);
        buf[usa_offset..usa_offset + 2].copy_from_slice(&check.to_le_bytes());
        for i in 1..usa_count {
            let tail = i * sector - 2;
            let original = [buf[tail], buf[tail + 1]];
            buf[usa_offset + i * 2..usa_offset + i * 2 + 2].copy_from_slice(&original);
            buf[tail..tail + 2].copy_from_slice(&check.to_le_bytes());
        }
        buf
    }
}

// ---------------------------------------------------------------------------
// whole images
// ---------------------------------------------------------------------------

/// A synthetic NTFS volume image: boot sector, an MFT laid out over the given
/// fragments, and the caller's records placed at their proper offsets.
pub struct ImageSpec {
    pub geometry: Geometry,
    /// `(lcn, cluster_count)` in VCN order. Record 0's `$DATA` run list is
    /// generated from this, so the bootstrap recovers exactly this map.
    pub fragments: Vec<(u64, u64)>,
    pub records: Vec<RecordSpec>,
}

impl ImageSpec {
    /// Records the image's MFT claims to contain: the full fragment capacity,
    /// so the sweep walks every slot (unwritten ones read as never-used).
    pub fn capacity_records(&self) -> u64 {
        let clusters: u64 = self.fragments.iter().map(|&(_, c)| c).sum();
        clusters * u64::from(self.geometry.bytes_per_cluster)
            / u64::from(self.geometry.bytes_per_record)
    }

    /// Write the image to `dir/<tag>.img` and return its path.
    pub fn write(&self, tag: &str) -> PathBuf {
        let g = self.geometry;
        let record_size = u64::from(g.bytes_per_record);
        let data_size = self.capacity_records() * record_size;

        // Record 0: $MFT describing itself — its run list *is* the map.
        let mft_runs = encode_runs(&self.fragments);
        let allocated: u64 =
            self.fragments.iter().map(|&(_, c)| c).sum::<u64>() * u64::from(g.bytes_per_cluster);
        let record0 = RecordSpec::new(0)
            .attr(std_info_attr(0, 0, 0x06))
            .attr(file_name_attr(5, 3, "$MFT"))
            .attr(non_resident_attr(0x80, data_size, allocated, &mft_runs));

        let volume_clusters = self
            .fragments
            .iter()
            .map(|&(lcn, count)| lcn + count)
            .max()
            .unwrap_or(0)
            + 4;
        let mut volume = vec![0u8; (volume_clusters * u64::from(g.bytes_per_cluster)) as usize];

        volume[..g.bytes_per_sector as usize].copy_from_slice(&boot_sector(
            g,
            self.fragments[0].0,
            volume_clusters,
        ));

        for spec in std::iter::once(&record0).chain(self.records.iter()) {
            let offset = self.record_offset(spec.number) as usize;
            let bytes = spec.build(g);
            volume[offset..offset + bytes.len()].copy_from_slice(&bytes);
        }

        let mut dir = std::env::temp_dir();
        dir.push(format!("emfit-sweep-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{tag}.img"));
        std::fs::write(&path, &volume).expect("write image");
        path
    }

    /// Volume byte offset of a record, walking the fragment map.
    fn record_offset(&self, number: u64) -> u64 {
        let g = self.geometry;
        let byte = number * u64::from(g.bytes_per_record);
        let cluster = byte / u64::from(g.bytes_per_cluster);
        let within = byte % u64::from(g.bytes_per_cluster);

        let mut vcn = 0u64;
        for &(lcn, count) in &self.fragments {
            if cluster < vcn + count {
                return (lcn + cluster - vcn) * u64::from(g.bytes_per_cluster) + within;
            }
            vcn += count;
        }
        panic!("record {number} lies past the fragment map");
    }
}

/// A minimal boot sector for the geometry.
pub fn boot_sector(g: Geometry, mft_start_lcn: u64, volume_clusters: u64) -> Vec<u8> {
    let mut buf = vec![0u8; g.bytes_per_sector as usize];
    buf[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
    buf[3..11].copy_from_slice(b"NTFS    ");
    buf[0x0B..0x0D].copy_from_slice(&(g.bytes_per_sector as u16).to_le_bytes());
    buf[0x0D] = (g.bytes_per_cluster / g.bytes_per_sector) as u8;
    buf[0x15] = 0xF8;
    let sectors = volume_clusters * u64::from(g.bytes_per_cluster / g.bytes_per_sector);
    buf[0x28..0x30].copy_from_slice(&sectors.to_le_bytes());
    buf[0x30..0x38].copy_from_slice(&mft_start_lcn.to_le_bytes());
    buf[0x38..0x40].copy_from_slice(&2u64.to_le_bytes());
    // Record size: negative form, 2^n bytes.
    let log2 = g.bytes_per_record.trailing_zeros() as u8;
    buf[0x40] = 0u8.wrapping_sub(log2);
    buf[0x48..0x50].copy_from_slice(&0xC0FF_EE00_DEAD_0001u64.to_le_bytes());
    buf[g.bytes_per_sector as usize - 2] = 0x55;
    buf[g.bytes_per_sector as usize - 1] = 0xAA;
    buf
}

/// Open an image the way the tests read it.
pub fn open_image(path: &Path) -> FileBlockSource {
    FileBlockSource::open_image(path).expect("open image")
}
