//! Attributes: everything a file record actually says.
//!
//! A record's contents are a chain of typed, self-describing chunks. Each
//! carries its own length, so walking them is a matter of adding lengths until
//! the end marker — which is also why one bad length turns the rest of the
//! record into nonsense, and why the iterator here refuses to move backwards
//! or off the end.
//!
//! # Resident and non-resident
//!
//! A small attribute stores its value inline. A large one stores a **data run
//! list** instead, mapping virtual clusters to positions on the volume. A
//! 200-byte text file therefore occupies no clusters at all — its bytes live
//! in the MFT record — which is why "size on disk" for such a file is zero
//! rather than one cluster.

use crate::parser::ntfs::runs;

/// What an attribute holds. Only the four EmFit reads are named; the rest
/// carry their raw type code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeType {
    /// Timestamps and the DOS attribute bits.
    StandardInformation,
    /// Points at extension records when a file's attributes outgrow one record.
    AttributeList,
    /// A name and the parent directory's record number. A file has one per
    /// name: long, 8.3 short, and one per hard link.
    FileName,
    /// The file's contents, or a map of where they are.
    Data,
    /// Anything else — security descriptors, index roots, reparse data.
    Other(u32),
    /// The terminator.
    End,
}

impl AttributeType {
    pub fn from_code(code: u32) -> Self {
        match code {
            0x10 => Self::StandardInformation,
            0x20 => Self::AttributeList,
            0x30 => Self::FileName,
            0x80 => Self::Data,
            0xFFFF_FFFF => Self::End,
            other => Self::Other(other),
        }
    }
}

/// Smallest possible attribute header — type, length, and the resident flag.
const MIN_HEADER: usize = 16;
/// Resident attributes add a value length and offset.
const RESIDENT_HEADER: usize = 24;
/// Non-resident attributes add VCN range, run-list offset, and sizes.
const NON_RESIDENT_HEADER: usize = 64;

/// One attribute, borrowed from the record it lives in.
#[derive(Debug, Clone, Copy)]
pub struct Attribute<'a> {
    data: &'a [u8],
}

impl<'a> Attribute<'a> {
    pub fn type_code(&self) -> u32 {
        read_u32(self.data, 0x00)
    }

    pub fn kind(&self) -> AttributeType {
        AttributeType::from_code(self.type_code())
    }

    /// Total length including the header — what the iterator advances by.
    pub fn len(&self) -> usize {
        read_u32(self.data, 0x04) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_non_resident(&self) -> bool {
        self.data[0x08] != 0
    }

    /// Name length in UTF-16 code units. Zero means the default, unnamed
    /// stream.
    pub fn name_len(&self) -> usize {
        usize::from(self.data[0x09])
    }

    /// True for the default stream. A named `$DATA` is an alternate data
    /// stream, whose bytes belong to the file but are not its main contents.
    pub fn is_unnamed(&self) -> bool {
        self.name_len() == 0
    }

    /// Attribute-level flags — compression, encryption, sparseness.
    ///
    /// More reliable than the DOS attribute bits in `$STANDARD_INFORMATION`,
    /// which describe the *file* while these describe *this stream*.
    pub fn flags(&self) -> u16 {
        read_u16(self.data, 0x0C)
    }

    /// The stream is compressed, so its logical size exceeds what it occupies.
    pub fn is_compressed(&self) -> bool {
        self.flags() & 0x00FF != 0
    }

    /// The stream has holes that occupy nothing.
    pub fn is_sparse(&self) -> bool {
        self.flags() & 0x8000 != 0
    }

    pub fn is_encrypted(&self) -> bool {
        self.flags() & 0x4000 != 0
    }

    /// The name as raw UTF-16LE bytes, for the rare caller that wants it.
    pub fn name_bytes(&self) -> &'a [u8] {
        let offset = read_u16(self.data, 0x0A) as usize;
        let bytes = self.name_len() * 2;
        if offset + bytes > self.data.len() {
            return &[];
        }
        &self.data[offset..offset + bytes]
    }

    /// The inline value, for a resident attribute.
    pub fn resident_value(&self) -> Option<&'a [u8]> {
        if self.is_non_resident() || self.data.len() < RESIDENT_HEADER {
            return None;
        }
        let length = read_u32(self.data, 0x10) as usize;
        let offset = read_u16(self.data, 0x14) as usize;
        if offset < MIN_HEADER || offset.checked_add(length)? > self.data.len() {
            return None;
        }
        Some(&self.data[offset..offset + length])
    }

    /// The cluster map and sizes, for a non-resident attribute.
    pub fn non_resident(&self) -> Option<NonResident<'a>> {
        if !self.is_non_resident() || self.data.len() < NON_RESIDENT_HEADER {
            return None;
        }
        Some(NonResident { data: self.data })
    }

    /// The attribute's own bytes.
    pub fn bytes(&self) -> &'a [u8] {
        self.data
    }
}

/// The non-resident half of an attribute header.
#[derive(Debug, Clone, Copy)]
pub struct NonResident<'a> {
    data: &'a [u8],
}

impl<'a> NonResident<'a> {
    /// First virtual cluster this fragment of the attribute covers.
    ///
    /// Non-zero when the run list itself was too big for one record and
    /// continues in an extension record. Only the fragment starting at zero
    /// carries the real sizes; the others repeat them as zero or stale values,
    /// which is why a caller must check this before trusting [`Self::data_size`].
    pub fn starting_vcn(&self) -> u64 {
        read_u64(self.data, 0x10)
    }

    pub fn last_vcn(&self) -> u64 {
        read_u64(self.data, 0x18)
    }

    /// Size of the **virtual** cluster range this attribute spans.
    ///
    /// Despite the name this is not always what the file occupies. For a
    /// compressed or sparse stream it is the size the data *would* take if it
    /// were laid out plainly — holes and compressed runs included. The figure
    /// a user means by "size on disk" is [`Self::physical_size`].
    pub fn allocated_size(&self) -> u64 {
        read_u64(self.data, 0x28)
    }

    /// Compression unit, as a power-of-two count of clusters. Zero means the
    /// stream is stored plainly, and is also what says whether
    /// [`Self::compressed_size`] is present at all.
    pub fn compression_unit(&self) -> u16 {
        read_u16(self.data, 0x22)
    }

    /// Bytes genuinely occupied, for a compressed or sparse stream.
    ///
    /// This field only exists when [`Self::compression_unit`] is non-zero —
    /// the header is 64 bytes without it and 72 with it — so reading it
    /// unconditionally would pick up whatever follows.
    pub fn compressed_size(&self) -> Option<u64> {
        (self.compression_unit() != 0 && self.data.len() >= 0x48).then(|| read_u64(self.data, 0x40))
    }

    /// What this stream actually costs on disk.
    ///
    /// The compressed size when there is one, the allocated range otherwise.
    /// Using [`Self::allocated_size`] directly overstates every compressed and
    /// sparse file, which on a Windows volume is enough to push a whole-disk
    /// total several gigabytes past what the filesystem reports as used.
    pub fn physical_size(&self) -> u64 {
        self.compressed_size()
            .unwrap_or_else(|| self.allocated_size())
    }

    /// The file's logical size: what a user calls "size".
    pub fn data_size(&self) -> u64 {
        read_u64(self.data, 0x30)
    }

    /// How much of the file has actually been written. The tail between this
    /// and [`Self::data_size`] reads as zeroes.
    pub fn initialized_size(&self) -> u64 {
        read_u64(self.data, 0x38)
    }

    /// Decode the cluster map.
    pub fn data_runs(&self) -> Vec<runs::DataRun> {
        let offset = read_u16(self.data, 0x20) as usize;
        if offset >= self.data.len() {
            return Vec::new();
        }
        runs::decode(&self.data[offset..]).0
    }
}

/// Walks a record's attribute chain.
///
/// Stops at the end marker, at a length that would not advance, or at anything
/// that would read past the record. A corrupt length must end the walk, not
/// send it wandering through stale bytes producing plausible-looking files.
#[derive(Debug, Clone)]
pub struct AttributeIter<'a> {
    buf: &'a [u8],
    offset: usize,
    done: bool,
}

impl<'a> AttributeIter<'a> {
    pub fn new(buf: &'a [u8], first_offset: usize) -> Self {
        Self {
            buf,
            offset: first_offset,
            done: false,
        }
    }
}

impl<'a> Iterator for AttributeIter<'a> {
    type Item = Attribute<'a>;

    fn next(&mut self) -> Option<Attribute<'a>> {
        if self.done || self.offset + MIN_HEADER > self.buf.len() {
            return None;
        }

        let type_code = read_u32(self.buf, self.offset);
        if type_code == 0xFFFF_FFFF || type_code == 0 {
            self.done = true;
            return None;
        }

        let length = read_u32(self.buf, self.offset + 4) as usize;
        // A zero or unaligned length would either loop forever or desynchronize
        // the walk from the real attribute boundaries.
        if length < MIN_HEADER || !length.is_multiple_of(8) || self.offset + length > self.buf.len()
        {
            self.done = true;
            return None;
        }

        let attribute = Attribute {
            data: &self.buf[self.offset..self.offset + length],
        };
        self.offset += length;
        Some(attribute)
    }
}

// ---------------------------------------------------------------------------
// $STANDARD_INFORMATION
// ---------------------------------------------------------------------------

/// Timestamps and attribute bits, present on every file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandardInfo {
    /// Creation, as nanoseconds since the Unix epoch.
    pub created: i64,
    /// Last content modification.
    pub modified: i64,
    /// Last access. Often stale: Windows disables access-time updates by
    /// default, so this can be years behind.
    pub accessed: i64,
    /// DOS attribute bits — hidden, system, compressed, and so on.
    pub attributes: u32,
}

impl StandardInfo {
    /// Fixed layout:
    ///
    /// | offset | field |
    /// |--------|-------|
    /// | `0x00` | creation time |
    /// | `0x08` | content modification time |
    /// | `0x10` | MFT record change time (not read) |
    /// | `0x18` | access time |
    /// | `0x20` | DOS attribute bits |
    pub fn parse(value: &[u8]) -> Option<Self> {
        if value.len() < 0x24 {
            return None;
        }
        Some(Self {
            created: filetime_to_unix_nanos(read_u64(value, 0x00)),
            modified: filetime_to_unix_nanos(read_u64(value, 0x08)),
            accessed: filetime_to_unix_nanos(read_u64(value, 0x18)),
            attributes: read_u32(value, 0x20),
        })
    }
}

// ---------------------------------------------------------------------------
// $FILE_NAME
// ---------------------------------------------------------------------------

/// Which naming convention a `$FILE_NAME` follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    /// Case-sensitive, anything but NUL and `/`.
    Posix,
    /// The ordinary long name.
    Win32,
    /// The 8.3 alias — `PROGRA~1`. An alternative spelling of a name the file
    /// already has, so counting it as a separate link double-counts the file.
    Dos,
    /// A name short enough to serve as both.
    Win32AndDos,
    Unknown(u8),
}

impl Namespace {
    pub fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Posix,
            1 => Self::Win32,
            2 => Self::Dos,
            3 => Self::Win32AndDos,
            other => Self::Unknown(other),
        }
    }

    /// Whether this name is a real directory entry rather than an alias.
    ///
    /// 8.3 names are skipped: the file already appears under its long name,
    /// and treating the alias as another link would list it twice and count
    /// its bytes twice.
    pub fn is_real_name(&self) -> bool {
        !matches!(self, Self::Dos)
    }
}

/// A name, and the directory it appears in.
#[derive(Debug, Clone, Copy)]
pub struct FileName<'a> {
    value: &'a [u8],
}

impl<'a> FileName<'a> {
    /// Header is 66 bytes, then the name.
    pub fn parse(value: &'a [u8]) -> Option<Self> {
        if value.len() < 0x42 {
            return None;
        }
        let this = Self { value };
        // The name must be inside the attribute.
        (0x42 + this.name_len() * 2 <= value.len()).then_some(this)
    }

    /// The containing directory's MFT record number.
    ///
    /// The stored reference packs a 16-bit sequence number into the top bits;
    /// masking it off is what turns a file reference into a record number, and
    /// forgetting to gives a parent millions of records away.
    pub fn parent_record(&self) -> u64 {
        read_u64(self.value, 0x00) & 0x0000_FFFF_FFFF_FFFF
    }

    /// The full parent reference including its sequence number.
    pub fn parent_reference(&self) -> u64 {
        read_u64(self.value, 0x00)
    }

    /// Length in UTF-16 code units.
    pub fn name_len(&self) -> usize {
        usize::from(self.value[0x40])
    }

    pub fn namespace(&self) -> Namespace {
        Namespace::from_code(self.value[0x41])
    }

    /// Sizes as recorded *here*, which the directory index keeps for fast
    /// listings. They lag the real `$DATA` values, so they are a fallback and
    /// never a preference.
    pub fn allocated_size(&self) -> u64 {
        read_u64(self.value, 0x28)
    }

    pub fn data_size(&self) -> u64 {
        read_u64(self.value, 0x30)
    }

    /// Decode the name into `out`, replacing its contents.
    ///
    /// Takes a buffer rather than returning a `String` so the sweep can reuse
    /// one allocation across millions of records instead of making one per
    /// name.
    pub fn decode_name_into(&self, out: &mut String) {
        out.clear();
        let bytes = &self.value[0x42..0x42 + self.name_len() * 2];
        let units = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
        for unit in char::decode_utf16(units) {
            // Lone surrogates are legal in NTFS names and not in Rust strings.
            out.push(unit.unwrap_or(char::REPLACEMENT_CHARACTER));
        }
    }

    /// Convenience for tests and one-off lookups. The sweep uses
    /// [`Self::decode_name_into`].
    pub fn to_name(&self) -> String {
        let mut out = String::new();
        self.decode_name_into(&mut out);
        out
    }
}

// ---------------------------------------------------------------------------
// $ATTRIBUTE_LIST
// ---------------------------------------------------------------------------

/// One entry in an `$ATTRIBUTE_LIST`: an attribute that lives elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttributeListEntry {
    pub type_code: u32,
    /// First VCN this fragment covers, for a split run list.
    pub starting_vcn: u64,
    /// The record holding it. Equal to the base record when the attribute did
    /// not actually move.
    pub record_number: u64,
    pub name_len: u8,
}

/// Walk an `$ATTRIBUTE_LIST` value.
///
/// A file whose attributes outgrow one record gets this: a directory of where
/// each attribute went. It is how a heavily hard-linked file in WinSxS, or one
/// fragmented into thousands of pieces, still describes itself.
pub fn parse_attribute_list(value: &[u8]) -> Vec<AttributeListEntry> {
    let mut entries = Vec::new();
    let mut offset = 0usize;

    while offset + 26 <= value.len() {
        let length = read_u16(value, offset + 4) as usize;
        if length < 26 || offset + length > value.len() {
            break;
        }
        entries.push(AttributeListEntry {
            type_code: read_u32(value, offset),
            starting_vcn: read_u64(value, offset + 8),
            record_number: read_u64(value, offset + 0x10) & 0x0000_FFFF_FFFF_FFFF,
            name_len: value[offset + 6],
        });
        offset += length;
    }

    entries
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Seconds between 1601-01-01 and 1970-01-01, in 100-nanosecond ticks.
const FILETIME_EPOCH_OFFSET: i64 = 116_444_736_000_000_000;

/// Convert a Windows `FILETIME` to nanoseconds since the Unix epoch.
///
/// NTFS counts 100-nanosecond ticks from 1601; the index stores nanoseconds
/// from 1970. Converting here rather than in the UI is the rule that keeps a
/// second filesystem from leaking its own epoch upward (`architecture.md` §7).
///
/// Zero in, zero out — NTFS uses it for "unknown", and shifting it by 369 years
/// would put unknown timestamps in 1601 rather than leaving them blank.
pub fn filetime_to_unix_nanos(filetime: u64) -> i64 {
    if filetime == 0 {
        return 0;
    }
    let ticks = filetime as i64 - FILETIME_EPOCH_OFFSET;
    ticks.saturating_mul(100)
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

    /// Build a resident attribute with `value` as its payload.
    fn resident(type_code: u32, value: &[u8]) -> Vec<u8> {
        let header = RESIDENT_HEADER;
        let total = (header + value.len()).next_multiple_of(8);
        let mut buf = vec![0u8; total];
        buf[0x00..0x04].copy_from_slice(&type_code.to_le_bytes());
        buf[0x04..0x08].copy_from_slice(&(total as u32).to_le_bytes());
        buf[0x08] = 0; // resident
        buf[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        buf[0x14..0x16].copy_from_slice(&(header as u16).to_le_bytes());
        buf[header..header + value.len()].copy_from_slice(value);
        buf
    }

    /// Build a non-resident attribute carrying `run_list`.
    fn non_resident(type_code: u32, data_size: u64, allocated: u64, run_list: &[u8]) -> Vec<u8> {
        let runs_at = NON_RESIDENT_HEADER;
        let total = (runs_at + run_list.len()).next_multiple_of(8);
        let mut buf = vec![0u8; total];
        buf[0x00..0x04].copy_from_slice(&type_code.to_le_bytes());
        buf[0x04..0x08].copy_from_slice(&(total as u32).to_le_bytes());
        buf[0x08] = 1; // non-resident
        buf[0x20..0x22].copy_from_slice(&(runs_at as u16).to_le_bytes());
        buf[0x28..0x30].copy_from_slice(&allocated.to_le_bytes());
        buf[0x30..0x38].copy_from_slice(&data_size.to_le_bytes());
        buf[0x38..0x40].copy_from_slice(&data_size.to_le_bytes());
        buf[runs_at..runs_at + run_list.len()].copy_from_slice(run_list);
        buf
    }

    fn end_marker() -> Vec<u8> {
        0xFFFF_FFFFu32.to_le_bytes().to_vec()
    }

    fn chain(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.iter().flatten().copied().collect()
    }

    #[test]
    fn walks_a_chain_and_stops_at_the_marker() {
        let buf = chain(&[
            resident(0x10, &[0xAA; 48]),
            resident(0x30, &[0xBB; 66]),
            non_resident(0x80, 4096, 4096, &[0x21, 0x01, 0x00, 0x01, 0x00]),
            end_marker(),
        ]);

        let kinds: Vec<_> = AttributeIter::new(&buf, 0).map(|a| a.kind()).collect();
        assert_eq!(
            kinds,
            vec![
                AttributeType::StandardInformation,
                AttributeType::FileName,
                AttributeType::Data,
            ]
        );
    }

    #[test]
    fn resident_values_come_back_intact() {
        let buf = resident(0x10, &[1, 2, 3, 4, 5]);
        let attr = AttributeIter::new(&buf, 0).next().unwrap();

        assert!(!attr.is_non_resident());
        assert_eq!(attr.resident_value(), Some(&[1u8, 2, 3, 4, 5][..]));
        assert!(attr.non_resident().is_none());
    }

    #[test]
    fn a_plain_stream_occupies_its_allocated_range() {
        let buf = non_resident(0x80, 30_000, 32_768, &[0x21, 0x08, 0x00, 0x01, 0x00]);
        let nr = AttributeIter::new(&buf, 0)
            .next()
            .unwrap()
            .non_resident()
            .unwrap();

        assert_eq!(nr.compression_unit(), 0);
        assert_eq!(
            nr.compressed_size(),
            None,
            "no such field on a plain stream"
        );
        assert_eq!(nr.physical_size(), 32_768);
    }

    #[test]
    fn a_compressed_stream_occupies_its_compressed_size() {
        // The distinction that keeps a whole-volume total honest: allocated
        // describes the virtual range, compressed_size what it really costs.
        let runs = [0x21u8, 0x08, 0x00, 0x01, 0x00];
        let total = 0x48 + runs.len();
        let mut buf = vec![0u8; total.next_multiple_of(8)];
        let len = buf.len() as u32;
        buf[0x00..0x04].copy_from_slice(&0x80u32.to_le_bytes());
        buf[0x04..0x08].copy_from_slice(&len.to_le_bytes());
        buf[0x08] = 1; // non-resident
        buf[0x0C..0x0E].copy_from_slice(&0x0001u16.to_le_bytes()); // compressed
        buf[0x20..0x22].copy_from_slice(&0x48u16.to_le_bytes()); // runs offset
        buf[0x22..0x24].copy_from_slice(&4u16.to_le_bytes()); // compression unit
        buf[0x28..0x30].copy_from_slice(&65_536u64.to_le_bytes()); // allocated
        buf[0x30..0x38].copy_from_slice(&60_000u64.to_le_bytes()); // logical
        buf[0x38..0x40].copy_from_slice(&60_000u64.to_le_bytes()); // initialized
        buf[0x40..0x48].copy_from_slice(&20_480u64.to_le_bytes()); // compressed
        buf[0x48..0x48 + runs.len()].copy_from_slice(&runs);

        let attr = AttributeIter::new(&buf, 0).next().unwrap();
        assert!(attr.is_compressed());
        assert!(!attr.is_sparse());

        let nr = attr.non_resident().unwrap();
        assert_eq!(nr.data_size(), 60_000, "what a user calls the size");
        assert_eq!(nr.allocated_size(), 65_536, "the virtual range");
        assert_eq!(nr.compressed_size(), Some(20_480));
        assert_eq!(
            nr.physical_size(),
            20_480,
            "counting 65_536 here would inflate every compressed file"
        );
    }

    #[test]
    fn a_truncated_compressed_header_falls_back_rather_than_reading_past_it() {
        // compression_unit says the field is there, but the attribute is too
        // short to hold it — reading anyway would pick up the run list.
        let mut buf = non_resident(0x80, 1000, 4096, &[0x00]);
        buf[0x22..0x24].copy_from_slice(&4u16.to_le_bytes());
        buf.truncate(0x44);
        let len = buf.len() as u32;
        buf[0x04..0x08].copy_from_slice(&len.to_le_bytes());

        let nr = NonResident { data: &buf };
        assert_eq!(nr.compressed_size(), None);
        assert_eq!(nr.physical_size(), 4096);
    }

    #[test]
    fn attribute_flags_are_readable() {
        let mut sparse = non_resident(0x80, 1000, 4096, &[0x00]);
        sparse[0x0C..0x0E].copy_from_slice(&0x8000u16.to_le_bytes());
        let attr = AttributeIter::new(&sparse, 0).next().unwrap();
        assert!(attr.is_sparse());
        assert!(!attr.is_compressed());

        let mut encrypted = non_resident(0x80, 1000, 4096, &[0x00]);
        encrypted[0x0C..0x0E].copy_from_slice(&0x4000u16.to_le_bytes());
        let attr = AttributeIter::new(&encrypted, 0).next().unwrap();
        assert!(attr.is_encrypted());
    }

    #[test]
    fn non_resident_attributes_expose_sizes_and_runs() {
        // 8 clusters at LCN 0x0100.
        let buf = non_resident(0x80, 30_000, 32_768, &[0x21, 0x08, 0x00, 0x01, 0x00]);
        let attr = AttributeIter::new(&buf, 0).next().unwrap();
        let nr = attr.non_resident().unwrap();

        assert_eq!(nr.data_size(), 30_000);
        assert_eq!(nr.allocated_size(), 32_768);
        assert_eq!(nr.starting_vcn(), 0);
        assert!(attr.resident_value().is_none());

        let runs = nr.data_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].lcn, Some(0x0100));
        assert_eq!(runs[0].cluster_count, 8);
    }

    #[test]
    fn named_streams_are_distinguishable_from_the_main_one() {
        let mut named = resident(0x80, &[0u8; 8]);
        named[0x09] = 4; // four UTF-16 chars
        named[0x0A..0x0C].copy_from_slice(&(MIN_HEADER as u16).to_le_bytes());

        let attr = AttributeIter::new(&named, 0).next().unwrap();
        assert!(!attr.is_unnamed(), "an alternate data stream");
        assert_eq!(attr.name_len(), 4);

        let plain = resident(0x80, &[0u8; 8]);
        let attr = AttributeIter::new(&plain, 0).next().unwrap();
        assert!(attr.is_unnamed(), "the file's actual contents");
    }

    #[test]
    fn a_corrupt_length_ends_the_walk_instead_of_wandering() {
        // Zero length would loop forever.
        let mut zero = resident(0x10, &[0u8; 8]);
        zero[0x04..0x08].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(AttributeIter::new(&zero, 0).count(), 0);

        // A length past the end would read stale bytes as an attribute.
        let mut over = resident(0x10, &[0u8; 8]);
        over[0x04..0x08].copy_from_slice(&9999u32.to_le_bytes());
        assert_eq!(AttributeIter::new(&over, 0).count(), 0);

        // An unaligned length desynchronizes every following attribute.
        let mut odd = resident(0x10, &[0u8; 8]);
        odd[0x04..0x08].copy_from_slice(&25u32.to_le_bytes());
        assert_eq!(AttributeIter::new(&odd, 0).count(), 0);
    }

    #[test]
    fn a_truncated_chain_yields_what_was_whole() {
        let full = chain(&[resident(0x10, &[0xAA; 48]), resident(0x30, &[0xBB; 66])]);
        let cut = &full[..full.len() - 20];
        assert_eq!(AttributeIter::new(cut, 0).count(), 1);
    }

    #[test]
    fn standard_information_reads_each_field_from_its_own_offset() {
        // Every field distinct, so a transposed offset cannot pass.
        let mut value = vec![0u8; 72];
        let created = 133_479_936_000_000_000u64; // 2024-01-01T00:00:00Z
        value[0x00..0x08].copy_from_slice(&created.to_le_bytes());
        value[0x08..0x10].copy_from_slice(&(created + 10_000_000).to_le_bytes()); // +1s
        value[0x10..0x18].copy_from_slice(&(created + 20_000_000).to_le_bytes()); // MFT change
        value[0x18..0x20].copy_from_slice(&(created + 30_000_000).to_le_bytes()); // +3s
        value[0x20..0x24].copy_from_slice(&0x0000_0022u32.to_le_bytes());

        let si = StandardInfo::parse(&value).unwrap();
        assert_eq!(si.attributes, 0x22);
        assert!(si.created > 1_700_000_000_000_000_000, "after 2023");
        assert_eq!(si.modified - si.created, 1_000_000_000, "one second later");
        assert_eq!(
            si.accessed - si.created,
            3_000_000_000,
            "access time comes from 0x18, not from the attribute bits at 0x20"
        );
    }

    #[test]
    fn standard_information_needs_the_whole_fixed_prefix() {
        assert!(StandardInfo::parse(&[0u8; 0x23]).is_none());
        assert!(StandardInfo::parse(&[0u8; 0x24]).is_some());
    }

    #[test]
    fn filetime_zero_stays_zero() {
        // Unknown must remain unknown, not become 1601.
        assert_eq!(filetime_to_unix_nanos(0), 0);
    }

    #[test]
    fn filetime_converts_a_known_instant() {
        // 1970-01-01T00:00:00Z is exactly the epoch offset.
        assert_eq!(filetime_to_unix_nanos(FILETIME_EPOCH_OFFSET as u64), 0);
        // One second later.
        assert_eq!(
            filetime_to_unix_nanos(FILETIME_EPOCH_OFFSET as u64 + 10_000_000),
            1_000_000_000
        );
    }

    /// Build a `$FILE_NAME` value for `name` in directory `parent`.
    fn file_name_value(parent: u64, namespace: u8, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut value = vec![0u8; 0x42 + units.len() * 2];
        // Parent reference: record number plus a sequence number in the top bits.
        value[0x00..0x08].copy_from_slice(&(parent | (3u64 << 48)).to_le_bytes());
        value[0x28..0x30].copy_from_slice(&8192u64.to_le_bytes());
        value[0x30..0x38].copy_from_slice(&5000u64.to_le_bytes());
        value[0x40] = units.len() as u8;
        value[0x41] = namespace;
        for (i, unit) in units.iter().enumerate() {
            value[0x42 + i * 2..0x44 + i * 2].copy_from_slice(&unit.to_le_bytes());
        }
        value
    }

    #[test]
    fn file_name_masks_the_sequence_off_the_parent() {
        let value = file_name_value(5, 1, "notes.txt");
        let fname = FileName::parse(&value).unwrap();

        assert_eq!(fname.parent_record(), 5, "sequence number must be masked");
        assert_ne!(fname.parent_reference(), 5, "but is still available whole");
        assert_eq!(fname.to_name(), "notes.txt");
        assert_eq!(fname.namespace(), Namespace::Win32);
        assert_eq!(fname.data_size(), 5000);
        assert_eq!(fname.allocated_size(), 8192);
    }

    #[test]
    fn dos_aliases_are_recognizable() {
        assert!(!Namespace::Dos.is_real_name(), "an alias, not a link");
        assert!(Namespace::Win32.is_real_name());
        assert!(Namespace::Win32AndDos.is_real_name());
        assert!(Namespace::Posix.is_real_name());
    }

    #[test]
    fn names_decode_beyond_ascii() {
        for name in ["日本語のファイル.txt", "café.txt", "emoji-🦀.rs"] {
            let value = file_name_value(5, 1, name);
            assert_eq!(FileName::parse(&value).unwrap().to_name(), name);
        }
    }

    #[test]
    fn decoding_reuses_the_callers_buffer() {
        let mut buf = String::from("stale contents");
        let value = file_name_value(5, 1, "fresh.txt");
        FileName::parse(&value).unwrap().decode_name_into(&mut buf);
        assert_eq!(buf, "fresh.txt");
    }

    #[test]
    fn a_lone_surrogate_becomes_a_replacement_char() {
        // Legal in an NTFS name, illegal in a Rust string.
        let mut value = vec![0u8; 0x42 + 4];
        value[0x40] = 2;
        value[0x41] = 1;
        value[0x42..0x44].copy_from_slice(&0xD800u16.to_le_bytes());
        value[0x44..0x46].copy_from_slice(&0x0041u16.to_le_bytes());

        let name = FileName::parse(&value).unwrap().to_name();
        assert_eq!(name, "\u{FFFD}A");
    }

    #[test]
    fn a_name_running_past_its_attribute_is_rejected() {
        let mut value = file_name_value(5, 1, "short");
        value[0x40] = 200; // claims 200 chars
        assert!(FileName::parse(&value).is_none());

        assert!(FileName::parse(&[0u8; 10]).is_none());
    }

    #[test]
    fn attribute_lists_name_the_records_holding_each_attribute() {
        let mut value = Vec::new();
        for (type_code, vcn, record) in [(0x10u32, 0u64, 100u64), (0x80, 0, 100), (0x80, 64, 350)] {
            let mut entry = vec![0u8; 32];
            entry[0x00..0x04].copy_from_slice(&type_code.to_le_bytes());
            entry[0x04..0x06].copy_from_slice(&32u16.to_le_bytes());
            entry[0x08..0x10].copy_from_slice(&vcn.to_le_bytes());
            // With a sequence number in the top bits, as on disk.
            entry[0x10..0x18].copy_from_slice(&(record | (2u64 << 48)).to_le_bytes());
            value.extend_from_slice(&entry);
        }

        let entries = parse_attribute_list(&value);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[2].type_code, 0x80);
        assert_eq!(entries[2].starting_vcn, 64);
        assert_eq!(
            entries[2].record_number, 350,
            "the run list continues in another record"
        );
    }

    #[test]
    fn a_corrupt_attribute_list_stops_cleanly() {
        let mut value = vec![0u8; 32];
        value[0x04..0x06].copy_from_slice(&0u16.to_le_bytes());
        assert!(parse_attribute_list(&value).is_empty());

        assert!(parse_attribute_list(&[0u8; 4]).is_empty());
    }
}
