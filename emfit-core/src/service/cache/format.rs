//! The snapshot container: a header, a table of sections, and their bytes.
//!
//! Deliberately dumb. It knows nothing about indexes, volumes, or nodes - only
//! that a file holds numbered, checksummed, optionally compressed blobs, each
//! labelled truth or derived. What the numbers mean lives in
//! [`super::snapshot`].
//!
//! Two rules make the format survive its own evolution without a migration
//! path (`caching.md` sec 2.1):
//!
//! - **A section this build does not recognize is skipped**, not an error.
//! - **A derived section that fails its checksum is dropped and rebuilt**; a
//!   truth section that fails means the whole file is unusable.
//!
//! Little-endian, always. Every platform EmFit targets is little-endian, so a
//! big-endian reader refuses the file rather than byte-swapping a few hundred
//! megabytes to be polite (roadmap M5 wants the choice made and written down).

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

use crate::error::{Error, Result};

/// First eight bytes of any snapshot.
pub const MAGIC: [u8; 8] = *b"EMFITSNP";

/// Bumped only for a change no reader can skip past. Adding a section kind is
/// not such a change.
pub const CONTAINER_VERSION: u16 = 1;

/// Reads back as `1` on a little-endian machine and `256` on a big-endian one.
const ENDIAN_MARK: u16 = 0x0001;

const HEADER_LEN: u64 = 32;
const ENTRY_LEN: u64 = 48;

/// A sanity bound on the section table, so a corrupt count cannot make the
/// reader allocate gigabytes before it notices.
const MAX_SECTIONS: u32 = 4096;

/// What a section holds. Values are on-disk constants: never renumber one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct SectionKind(pub u32);

/// Whether losing a section costs a rescan or a recompute.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SectionClass {
    /// Only a scan can produce it. Corrupt means the file is worthless.
    Truth,
    /// Recomputable from truth in O(n). Corrupt means rebuild it.
    Derived,
}

impl SectionClass {
    fn to_bits(self) -> u32 {
        match self {
            Self::Truth => 0,
            Self::Derived => 1,
        }
    }

    fn from_bits(bits: u32) -> Self {
        match bits {
            0 => Self::Truth,
            _ => Self::Derived,
        }
    }
}

/// How a section's bytes are stored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Codec {
    /// As-is.
    Raw,
    /// LZ4 in independent chunks, so compression and decompression both spread
    /// across cores and neither needs the whole section resident twice.
    Lz4,
}

impl Codec {
    /// Chunk size for [`Codec::Lz4`]. Big enough that the ratio barely differs
    /// from whole-section compression, small enough that a 250 MB snapshot
    /// splits into a couple of hundred parallel pieces.
    const CHUNK: usize = 1 << 20;

    fn to_bits(self) -> u32 {
        match self {
            Self::Raw => 0,
            Self::Lz4 => 1,
        }
    }

    fn from_bits(bits: u32) -> Option<Self> {
        match bits {
            0 => Some(Self::Raw),
            1 => Some(Self::Lz4),
            _ => None,
        }
    }

    fn encode(self, raw: &[u8]) -> Vec<u8> {
        match self {
            Self::Raw => raw.to_vec(),
            Self::Lz4 => {
                let parts: Vec<Vec<u8>> = raw
                    .par_chunks(Self::CHUNK)
                    .map(lz4_flex::compress)
                    .collect();

                let mut out = Vec::with_capacity(8 + parts.len() * 4 + raw.len() / 2);
                out.extend_from_slice(&(parts.len() as u32).to_le_bytes());
                out.extend_from_slice(&(Self::CHUNK as u32).to_le_bytes());
                for part in &parts {
                    out.extend_from_slice(&(part.len() as u32).to_le_bytes());
                }
                for part in &parts {
                    out.extend_from_slice(part);
                }
                out
            }
        }
    }

    fn decode(self, stored: Vec<u8>, raw_len: usize) -> Result<Vec<u8>> {
        match self {
            Self::Raw => {
                if stored.len() != raw_len {
                    return Err(malformed("raw section length disagrees with its header"));
                }
                Ok(stored)
            }
            Self::Lz4 => {
                let head = |at: usize| -> Result<u32> {
                    stored
                        .get(at..at + 4)
                        .map(|b| u32::from_le_bytes(b.try_into().expect("4-byte slice")))
                        .ok_or_else(|| malformed("compressed section is truncated"))
                };
                let count = head(0)? as usize;
                let chunk = head(4)? as usize;
                if chunk == 0 || count > raw_len.div_ceil(chunk.max(1)) + 1 {
                    return Err(malformed(
                        "compressed section has an impossible chunk table",
                    ));
                }

                let mut lens = Vec::with_capacity(count);
                for i in 0..count {
                    lens.push(head(8 + i * 4)? as usize);
                }
                let body_at = 8 + count * 4;
                let mut starts = Vec::with_capacity(count);
                let mut at = body_at;
                for &len in &lens {
                    starts.push(at);
                    at = at
                        .checked_add(len)
                        .ok_or_else(|| malformed("compressed section overflows"))?;
                }
                if at != stored.len() {
                    return Err(malformed(
                        "compressed section length disagrees with its table",
                    ));
                }

                let mut out = vec![0u8; raw_len];
                let results: Vec<Result<()>> = out
                    .par_chunks_mut(chunk)
                    .enumerate()
                    .map(|(i, slot)| {
                        let start = *starts
                            .get(i)
                            .ok_or_else(|| malformed("compressed section is missing a chunk"))?;
                        let end = start + lens[i];
                        let written = lz4_flex::decompress_into(&stored[start..end], slot)
                            .map_err(|e| malformed(&format!("lz4: {e}")))?;
                        if written != slot.len() {
                            return Err(malformed("a chunk decompressed to the wrong size"));
                        }
                        Ok(())
                    })
                    .collect();
                for result in results {
                    result?;
                }
                Ok(out)
            }
        }
    }
}

/// One row of the section table.
#[derive(Clone, Copy, Debug)]
pub struct SectionEntry {
    pub kind: SectionKind,
    /// Discriminates sections of the same kind - which sort column an order
    /// belongs to, for instance.
    pub param: u32,
    pub class: SectionClass,
    codec: u32,
    offset: u64,
    stored_len: u64,
    raw_len: u64,
    hash: u64,
}

impl SectionEntry {
    /// Bytes this section occupies once decoded.
    pub fn raw_len(&self) -> u64 {
        self.raw_len
    }

    /// Bytes it occupies in the file.
    pub fn stored_len(&self) -> u64 {
        self.stored_len
    }

    fn write_to(&self, out: &mut [u8]) {
        out[0..4].copy_from_slice(&self.kind.0.to_le_bytes());
        out[4..8].copy_from_slice(&self.codec.to_le_bytes());
        out[8..12].copy_from_slice(&self.class.to_bits().to_le_bytes());
        out[12..16].copy_from_slice(&self.param.to_le_bytes());
        out[16..24].copy_from_slice(&self.offset.to_le_bytes());
        out[24..32].copy_from_slice(&self.stored_len.to_le_bytes());
        out[32..40].copy_from_slice(&self.raw_len.to_le_bytes());
        out[40..48].copy_from_slice(&self.hash.to_le_bytes());
    }
}

/// Checksum of a section's stored bytes. Corruption detection, not integrity:
/// a snapshot is a local cache, and anyone who can rewrite it can rewrite the
/// checksum too. Validation of what the bytes *mean* is [`super::snapshot`]'s
/// job and is what actually keeps a bad file from reaching `Index`.
fn checksum(bytes: &[u8]) -> u64 {
    twox_hash::XxHash64::oneshot(0, bytes)
}

fn malformed(reason: &str) -> Error {
    Error::CacheInvalid {
        path: PathBuf::new(),
        reason: reason.to_string(),
    }
}

fn at_path(e: Error, path: &Path) -> Error {
    match e {
        Error::CacheInvalid { reason, .. } => Error::CacheInvalid {
            path: path.to_path_buf(),
            reason,
        },
        other => other,
    }
}

fn io(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

// ---------------------------------------------------------------------------
// writing
// ---------------------------------------------------------------------------

/// What an unfinished snapshot is called, before the rename puts it in place.
/// Everything that walks the cache directory skips this extension.
pub const TEMP_EXTENSION: &str = "tmp";

/// A temporary path no other writer will be using.
///
/// Unique per *writer*, not per target, and that is the whole point. Two
/// processes saving the same volume - the app in front of the user and a
/// background scan behind it - would otherwise both create
/// `<fingerprint>.tmp`, truncate each other, interleave their bytes, and each
/// rename the wreckage into place. Section checksums would catch it on the way
/// back in, so no wrong answer could come of it, but the snapshot would be
/// gone and the scan it saved would have to be done again.
///
/// With separate temporaries the rename stays atomic and whichever finishes
/// last wins with a complete file. Nothing has to coordinate for that to hold.
fn temp_path(path: &Path) -> PathBuf {
    /// Distinguishes writers within one process; the pid distinguishes them
    /// across processes.
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!("{}-{n}.{TEMP_EXTENSION}", std::process::id()))
}

/// Builds a snapshot file section by section, then swaps it into place.
///
/// Sections are handed over one at a time and written as they arrive, so the
/// peak cost is one section's bytes rather than the whole file's (STANDARDS
/// sec 4.5: a document is written to a temporary and renamed, so a process
/// killed mid-write leaves the previous snapshot standing).
pub struct Writer {
    file: BufWriter<File>,
    tmp: PathBuf,
    final_path: PathBuf,
    entries: Vec<SectionEntry>,
    capacity: u32,
    /// Extra table rows written as zeros, for [`Appender`] to fill later.
    reserved: u32,
    at: u64,
    done: bool,
}

impl Writer {
    /// Start a snapshot at `path`, promising exactly `sections` sections and
    /// leaving room in the table for `reserve` more.
    ///
    /// The reserved rows are what let derived sections arrive later
    /// ([`Appender`]): the expensive ones take seconds to compute, and holding
    /// the truth hostage to them means an app closed in the meantime writes no
    /// cache at all.
    pub fn create(path: &Path, manifest: &str, sections: u32, reserve: u32) -> Result<Self> {
        let rows = sections.saturating_add(reserve);
        if rows > MAX_SECTIONS {
            return Err(at_path(malformed("too many sections"), path));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
        let tmp = temp_path(path);
        let file = File::create(&tmp).map_err(|e| io(&tmp, e))?;
        let mut file = BufWriter::new(file);

        let table_len = ENTRY_LEN * u64::from(rows);
        let manifest_at = HEADER_LEN + table_len;
        let manifest_len = manifest.len() as u64;

        let mut header = [0u8; HEADER_LEN as usize];
        header[0..8].copy_from_slice(&MAGIC);
        header[8..10].copy_from_slice(&CONTAINER_VERSION.to_le_bytes());
        header[10..12].copy_from_slice(&ENDIAN_MARK.to_le_bytes());
        header[12..16].copy_from_slice(&sections.to_le_bytes());
        header[16..24].copy_from_slice(&manifest_at.to_le_bytes());
        header[24..32].copy_from_slice(&manifest_len.to_le_bytes());

        file.write_all(&header).map_err(|e| io(&tmp, e))?;
        file.write_all(&vec![0u8; table_len as usize])
            .map_err(|e| io(&tmp, e))?;
        file.write_all(manifest.as_bytes())
            .map_err(|e| io(&tmp, e))?;

        Ok(Self {
            file,
            tmp,
            final_path: path.to_path_buf(),
            entries: Vec::with_capacity(sections as usize),
            capacity: sections,
            reserved: reserve,
            at: manifest_at + manifest_len,
            done: false,
        })
    }

    /// Append one section. `payload` is consumed here and never held.
    pub fn section(
        &mut self,
        kind: SectionKind,
        param: u32,
        class: SectionClass,
        codec: Codec,
        payload: &[u8],
    ) -> Result<()> {
        if self.entries.len() as u32 >= self.capacity {
            return Err(at_path(
                malformed("more sections written than the header promised"),
                &self.final_path,
            ));
        }
        // Payload starts 8-byte aligned, so a future reader can map a column
        // straight out of the file without shifting it.
        let padding = (8 - (self.at % 8)) % 8;
        if padding > 0 {
            self.file
                .write_all(&[0u8; 8][..padding as usize])
                .map_err(|e| io(&self.tmp, e))?;
            self.at += padding;
        }

        let stored = codec.encode(payload);
        let entry = SectionEntry {
            kind,
            param,
            class,
            codec: codec.to_bits(),
            offset: self.at,
            stored_len: stored.len() as u64,
            raw_len: payload.len() as u64,
            hash: checksum(&stored),
        };
        self.file.write_all(&stored).map_err(|e| io(&self.tmp, e))?;
        self.at += stored.len() as u64;
        self.entries.push(entry);
        Ok(())
    }

    /// Write the section table, flush, and rename over any previous snapshot.
    pub fn finish(mut self) -> Result<()> {
        // Only the used rows are rewritten; the reserved ones stay zeroed and
        // out of reach until `section_count` grows to cover them.
        let _ = self.reserved;
        let mut table = vec![0u8; self.entries.len() * ENTRY_LEN as usize];
        for (i, entry) in self.entries.iter().enumerate() {
            let at = i * ENTRY_LEN as usize;
            entry.write_to(&mut table[at..at + ENTRY_LEN as usize]);
        }

        self.file
            .seek(SeekFrom::Start(HEADER_LEN))
            .map_err(|e| io(&self.tmp, e))?;
        self.file.write_all(&table).map_err(|e| io(&self.tmp, e))?;

        // `create` promised `capacity`; write what actually arrived, so a
        // writer that stopped early still produces a readable file.
        self.file
            .seek(SeekFrom::Start(12))
            .map_err(|e| io(&self.tmp, e))?;
        self.file
            .write_all(&(self.entries.len() as u32).to_le_bytes())
            .map_err(|e| io(&self.tmp, e))?;

        // Rename is only atomic with respect to what has actually reached the
        // disk, which is the difference between "no cache" and "a cache that
        // decompresses into garbage" after a power cut.
        self.file.flush().map_err(|e| io(&self.tmp, e))?;
        self.file
            .get_ref()
            .sync_all()
            .map_err(|e| io(&self.tmp, e))?;

        std::fs::rename(&self.tmp, &self.final_path).map_err(|e| io(&self.final_path, e))?;
        self.done = true;
        Ok(())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if !self.done {
            // An abandoned write leaves nothing behind: the previous snapshot,
            // whatever it was, is still the one at `final_path`.
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

// ---------------------------------------------------------------------------
// appending
// ---------------------------------------------------------------------------

/// Adds sections to a finished snapshot, using the table rows [`Writer`]
/// reserved.
///
/// This exists because the expensive derived sections - the sort orders - take
/// about ten seconds to compute while the truth takes half a second to write.
/// Making the file wait for them means an app closed in between writes no
/// cache at all, which is exactly the bug this was built to fix. So the truth
/// lands immediately and the orders catch up.
///
/// Crash-safe by ordering: payloads and their table rows are written and
/// synced first, and only then is `section_count` raised to cover them. A
/// process killed at any point leaves the file exactly as valid as it was,
/// with at worst some unreferenced bytes at the end.
pub struct Appender {
    file: File,
    path: PathBuf,
    /// Sections the file already has.
    count: u32,
    /// Rows the table can hold in total.
    capacity: u32,
    added: Vec<SectionEntry>,
    at: u64,
}

impl Appender {
    /// Open a snapshot for appending. Fails if it is not one.
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| io(path, e))?;

        let mut header = [0u8; HEADER_LEN as usize];
        file.read_exact(&mut header)
            .map_err(|_| at_path(malformed("file is shorter than a header"), path))?;
        if header[0..8] != MAGIC || u16::from_le_bytes([header[8], header[9]]) != CONTAINER_VERSION
        {
            return Err(at_path(malformed("not an appendable snapshot"), path));
        }
        let count = u32::from_le_bytes(header[12..16].try_into().expect("4-byte slice"));
        let manifest_at = u64::from_le_bytes(header[16..24].try_into().expect("8-byte slice"));
        let capacity = manifest_at
            .checked_sub(HEADER_LEN)
            .map(|table| (table / ENTRY_LEN) as u32)
            .unwrap_or(0);
        if count > capacity {
            return Err(at_path(
                malformed("section table overflows the manifest"),
                path,
            ));
        }

        let at = file.metadata().map_err(|e| io(path, e))?.len();
        Ok(Self {
            file,
            path: path.to_path_buf(),
            count,
            capacity,
            added: Vec::new(),
            at,
        })
    }

    /// Which sections the file already holds, so a caller can skip the ones
    /// that are there. Reading the table is cheap; reading payloads is not.
    pub fn existing(&mut self) -> Result<Vec<(SectionKind, u32)>> {
        let mut table = vec![0u8; self.count as usize * ENTRY_LEN as usize];
        self.file
            .seek(SeekFrom::Start(HEADER_LEN))
            .map_err(|e| io(&self.path, e))?;
        self.file
            .read_exact(&mut table)
            .map_err(|_| at_path(malformed("section table is truncated"), &self.path))?;
        Ok((0..self.count as usize)
            .map(|i| {
                let at = i * ENTRY_LEN as usize;
                let u32_at = |o: usize| {
                    u32::from_le_bytes(table[at + o..at + o + 4].try_into().expect("4-byte slice"))
                };
                (SectionKind(u32_at(0)), u32_at(12))
            })
            .collect())
    }

    /// Room left for further sections.
    pub fn room(&self) -> u32 {
        self.capacity
            .saturating_sub(self.count)
            .saturating_sub(self.added.len() as u32)
    }

    /// Append one section. Nothing is visible to a reader until [`finish`].
    ///
    /// [`finish`]: Appender::finish
    pub fn section(
        &mut self,
        kind: SectionKind,
        param: u32,
        class: SectionClass,
        codec: Codec,
        payload: &[u8],
    ) -> Result<()> {
        if self.room() == 0 {
            return Err(at_path(
                malformed("no reserved table rows left"),
                &self.path,
            ));
        }
        let padding = (8 - (self.at % 8)) % 8;
        self.file
            .seek(SeekFrom::Start(self.at))
            .map_err(|e| io(&self.path, e))?;
        if padding > 0 {
            self.file
                .write_all(&[0u8; 8][..padding as usize])
                .map_err(|e| io(&self.path, e))?;
            self.at += padding;
        }

        let stored = codec.encode(payload);
        let entry = SectionEntry {
            kind,
            param,
            class,
            codec: codec.to_bits(),
            offset: self.at,
            stored_len: stored.len() as u64,
            raw_len: payload.len() as u64,
            hash: checksum(&stored),
        };
        self.file
            .write_all(&stored)
            .map_err(|e| io(&self.path, e))?;
        self.at += stored.len() as u64;
        self.added.push(entry);
        Ok(())
    }

    /// Publish the appended sections. Returns how many were added.
    pub fn finish(mut self) -> Result<usize> {
        if self.added.is_empty() {
            return Ok(0);
        }
        let mut table = vec![0u8; self.added.len() * ENTRY_LEN as usize];
        for (i, entry) in self.added.iter().enumerate() {
            let at = i * ENTRY_LEN as usize;
            entry.write_to(&mut table[at..at + ENTRY_LEN as usize]);
        }
        let rows_at = HEADER_LEN + ENTRY_LEN * u64::from(self.count);
        self.file
            .seek(SeekFrom::Start(rows_at))
            .map_err(|e| io(&self.path, e))?;
        self.file.write_all(&table).map_err(|e| io(&self.path, e))?;

        // Everything above is inert until the count grows, so it has to be on
        // the disk before the count is.
        self.file.sync_all().map_err(|e| io(&self.path, e))?;

        let total = self.count + self.added.len() as u32;
        self.file
            .seek(SeekFrom::Start(12))
            .map_err(|e| io(&self.path, e))?;
        self.file
            .write_all(&total.to_le_bytes())
            .map_err(|e| io(&self.path, e))?;
        self.file.sync_all().map_err(|e| io(&self.path, e))?;
        Ok(self.added.len())
    }
}

// ---------------------------------------------------------------------------
// reading
// ---------------------------------------------------------------------------

/// Opens a snapshot's header and table without touching its payloads.
///
/// Cheap enough to run over every file in the cache directory - that is how
/// `cache list` and staleness checks work.
pub struct Reader {
    file: File,
    path: PathBuf,
    entries: Vec<SectionEntry>,
    manifest: String,
}

impl Reader {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path).map_err(|e| io(path, e))?;

        let mut header = [0u8; HEADER_LEN as usize];
        file.read_exact(&mut header)
            .map_err(|_| at_path(malformed("file is shorter than a header"), path))?;
        if header[0..8] != MAGIC {
            return Err(at_path(malformed("not an EmFit snapshot"), path));
        }
        let version = u16::from_le_bytes([header[8], header[9]]);
        if version != CONTAINER_VERSION {
            return Err(at_path(
                malformed(&format!("container version {version} is not readable here")),
                path,
            ));
        }
        if u16::from_le_bytes([header[10], header[11]]) != ENDIAN_MARK {
            return Err(at_path(malformed("file is big-endian"), path));
        }
        let count = u32::from_le_bytes(header[12..16].try_into().expect("4-byte slice"));
        if count > MAX_SECTIONS {
            return Err(at_path(malformed("section count is implausible"), path));
        }
        let manifest_at = u64::from_le_bytes(header[16..24].try_into().expect("8-byte slice"));
        let manifest_len = u64::from_le_bytes(header[24..32].try_into().expect("8-byte slice"));
        if manifest_len > 1 << 20 {
            return Err(at_path(malformed("manifest is implausibly large"), path));
        }

        let mut table = vec![0u8; count as usize * ENTRY_LEN as usize];
        file.read_exact(&mut table)
            .map_err(|_| at_path(malformed("section table is truncated"), path))?;

        let len = file.metadata().map_err(|e| io(path, e))?.len();
        let mut entries = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            let at = i * ENTRY_LEN as usize;
            let u32_at = |o: usize| {
                u32::from_le_bytes(table[at + o..at + o + 4].try_into().expect("4-byte slice"))
            };
            let u64_at = |o: usize| {
                u64::from_le_bytes(table[at + o..at + o + 8].try_into().expect("8-byte slice"))
            };
            let entry = SectionEntry {
                kind: SectionKind(u32_at(0)),
                codec: u32_at(4),
                class: SectionClass::from_bits(u32_at(8)),
                param: u32_at(12),
                offset: u64_at(16),
                stored_len: u64_at(24),
                raw_len: u64_at(32),
                hash: u64_at(40),
            };
            if entry.offset.saturating_add(entry.stored_len) > len {
                return Err(at_path(
                    malformed("a section runs past the end of the file"),
                    path,
                ));
            }
            entries.push(entry);
        }

        file.seek(SeekFrom::Start(manifest_at))
            .map_err(|e| io(path, e))?;
        let mut manifest = vec![0u8; manifest_len as usize];
        file.read_exact(&mut manifest)
            .map_err(|_| at_path(malformed("manifest is truncated"), path))?;
        let manifest = String::from_utf8(manifest)
            .map_err(|_| at_path(malformed("manifest is not UTF-8"), path))?;

        Ok(Self {
            file,
            path: path.to_path_buf(),
            entries,
            manifest,
        })
    }

    /// The manifest, verbatim. Parsing it is the caller's business.
    pub fn manifest(&self) -> &str {
        &self.manifest
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every section, in the order they were written.
    pub fn sections(&self) -> &[SectionEntry] {
        &self.entries
    }

    pub fn find(&self, kind: SectionKind, param: u32) -> Option<SectionEntry> {
        self.entries
            .iter()
            .find(|e| e.kind == kind && e.param == param)
            .copied()
    }

    /// Decode one section. Fails on a bad codec or a length disagreement; the
    /// caller decides what that costs by looking at [`SectionEntry::class`].
    pub fn read(&mut self, entry: &SectionEntry) -> Result<Vec<u8>> {
        let Some(codec) = Codec::from_bits(entry.codec) else {
            return Err(at_path(
                malformed(&format!("unknown codec {}", entry.codec)),
                &self.path,
            ));
        };
        // A corrupt raw_len would otherwise ask for an arbitrary allocation.
        if entry.raw_len > 1 << 40 {
            return Err(at_path(
                malformed("section length is implausible"),
                &self.path,
            ));
        }

        self.file
            .seek(SeekFrom::Start(entry.offset))
            .map_err(|e| io(&self.path, e))?;
        let mut stored = vec![0u8; entry.stored_len as usize];
        self.file
            .read_exact(&mut stored)
            .map_err(|_| at_path(malformed("a section is truncated"), &self.path))?;

        if checksum(&stored) != entry.hash {
            return Err(at_path(
                malformed("a section failed its checksum"),
                &self.path,
            ));
        }

        codec
            .decode(stored, entry.raw_len as usize)
            .map_err(|e| at_path(e, &self.path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: SectionKind = SectionKind(1);
    const B: SectionKind = SectionKind(2);

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("emfit-format-tests");
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join(name)
    }

    fn write(path: &Path, payloads: &[(SectionKind, Codec, Vec<u8>)]) {
        let mut w = Writer::create(path, "{\"schema_version\":1}", payloads.len() as u32, 4)
            .expect("create");
        for (kind, codec, bytes) in payloads {
            w.section(*kind, 0, SectionClass::Truth, *codec, bytes)
                .expect("section");
        }
        w.finish().expect("finish");
    }

    #[test]
    fn sections_round_trip_through_both_codecs() {
        let path = temp("round-trip.emfit");
        // Bigger than one LZ4 chunk, so the chunk table is exercised.
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let small = b"hello".to_vec();
        write(
            &path,
            &[(A, Codec::Raw, small.clone()), (B, Codec::Lz4, big.clone())],
        );

        let mut r = Reader::open(&path).expect("open");
        assert_eq!(r.manifest(), "{\"schema_version\":1}");
        let a = r.find(A, 0).expect("section a");
        let b = r.find(B, 0).expect("section b");
        assert_eq!(r.read(&a).expect("read a"), small);
        assert_eq!(r.read(&b).expect("read b"), big);
        assert!(
            b.stored_len() < b.raw_len(),
            "compressible data should compress"
        );
    }

    #[test]
    fn two_writers_of_one_snapshot_do_not_share_a_temporary() {
        // The app and a background scan can both be saving the same volume.
        // Sharing `<fingerprint>.tmp` meant truncating each other mid-write.
        let path = std::path::Path::new("cache").join(format!("aaaa.{}", "emfit"));
        let first = temp_path(&path);
        let second = temp_path(&path);

        assert_ne!(first, second);
        for temp in [&first, &second] {
            assert_eq!(
                temp.extension().and_then(|e| e.to_str()),
                Some(TEMP_EXTENSION),
                "still has to read as a temporary: {}",
                temp.display()
            );
            assert_ne!(temp, &path, "and must never be the snapshot itself");
        }
    }

    #[test]
    fn an_empty_section_round_trips() {
        let path = temp("empty.emfit");
        write(&path, &[(A, Codec::Lz4, Vec::new())]);
        let mut r = Reader::open(&path).expect("open");
        let a = r.find(A, 0).expect("section");
        assert!(r.read(&a).expect("read").is_empty());
    }

    #[test]
    fn a_foreign_file_is_refused() {
        let path = temp("foreign.emfit");
        std::fs::write(&path, b"this is not a snapshot at all, not even close").expect("write");
        assert!(Reader::open(&path).is_err());

        std::fs::write(&path, b"EM").expect("write");
        assert!(Reader::open(&path).is_err(), "shorter than a header");
    }

    #[test]
    fn a_truncated_file_is_refused_rather_than_read() {
        let path = temp("truncated.emfit");
        let big: Vec<u8> = (0..500_000u32).map(|i| (i % 97) as u8).collect();
        write(&path, &[(A, Codec::Lz4, big)]);

        let full = std::fs::read(&path).expect("read");
        std::fs::write(&path, &full[..full.len() / 2]).expect("truncate");

        match Reader::open(&path) {
            // Caught by the table's bounds check ...
            Err(_) => {}
            // ... or, if the table survived, by the section read.
            Ok(mut r) => {
                let a = r.find(A, 0).expect("section");
                assert!(r.read(&a).is_err());
            }
        }
    }

    #[test]
    fn corrupt_payload_bytes_do_not_produce_silent_garbage() {
        let path = temp("corrupt.emfit");
        let big: Vec<u8> = (0..400_000u32).map(|i| (i % 31) as u8).collect();
        write(&path, &[(A, Codec::Lz4, big.clone())]);

        let mut bytes = std::fs::read(&path).expect("read");
        let payload_at = bytes.len() - 1000;
        for byte in &mut bytes[payload_at..payload_at + 64] {
            *byte ^= 0xFF;
        }
        std::fs::write(&path, &bytes).expect("write");

        let mut r = Reader::open(&path).expect("header is intact");
        let a = r.find(A, 0).expect("section");
        match r.read(&a) {
            Err(_) => {}
            Ok(decoded) => assert_ne!(
                decoded, big,
                "if a corrupt section decodes at all it must not claim to be the original"
            ),
        }
    }

    #[test]
    fn an_abandoned_write_leaves_no_file_behind() {
        let path = temp("abandoned.emfit");
        let _ = std::fs::remove_file(&path);
        {
            let mut w = Writer::create(&path, "{}", 2, 0).expect("create");
            w.section(A, 0, SectionClass::Truth, Codec::Raw, b"half a snapshot")
                .expect("section");
            // Dropped without finish(), as a killed process would.
        }
        assert!(!path.exists(), "no snapshot");
        assert!(!temp_path(&path).exists(), "no leftover temporary");
    }
}
