//! Where bytes come from, independent of what they mean.
//!
//! One of the two axes in `architecture.md` sec 4. A [`BlockSource`] answers
//! "give me the bytes at this offset" and nothing else; deciding whether those
//! bytes are an MFT record or an ext4 inode is the scanner's job. Keeping the
//! two apart is what makes NTFS-in-an-image and ext4-in-an-image fall out for
//! free - v1 fused them, embedding `NtfsVolumeData` inside its I/O enum, so
//! opening an image implied NTFS.
//!
//! # Positional reads
//!
//! [`BlockSource::read_at`] takes `&self`, not `&mut self`. Underneath it uses
//! the platform's positional read - `seek_read` on Windows, `read_at` on Unix -
//! which carries the offset in the call rather than mutating a shared file
//! pointer. Several threads can therefore read one source concurrently with no
//! lock. v1 could not: it used `SetFilePointerEx` followed by `ReadFile`, a
//! seek-then-read pair that races, and so had to put the whole handle behind a
//! `Mutex`.
//!
//! # No `unsafe`
//!
//! The workspace denies `unsafe_code`, and nothing here needs it. Raw volumes
//! open through `OpenOptions` with Windows-specific flags, reads go through
//! `std`, and sector alignment is obtained by over-allocating and slicing
//! rather than by calling the allocator directly.

use std::fs::File;
use std::path::Path;

use crate::error::{Error, Result};

/// Sector size assumed when nothing better is known. Correct for the great
/// majority of drives; native 4Kn disks report 4096 and must be told so, since
/// unbuffered reads fail outright at the wrong granularity.
pub const DEFAULT_SECTOR_SIZE: u32 = 512;

/// A readable span of bytes: a mounted volume, a physical drive, or a disk
/// image.
///
/// Implementations must be `Send + Sync`; scanners read them from several
/// threads at once.
pub trait BlockSource: Send + Sync {
    /// Read into `buf` starting at `offset`, measured from the start of this
    /// source (so a partition-bounded source is addressed partition-relative,
    /// not disk-relative).
    ///
    /// Returns the number of bytes read, which may be less than `buf.len()` at
    /// the end of the source. Use [`BlockSource::read_exact_at`] when a short
    /// read is an error.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize>;

    /// Alignment required of offsets, lengths, and buffer addresses when
    /// [`BlockSource::requires_alignment`] is true.
    fn sector_size(&self) -> u32;

    /// Whether reads must be sector-aligned. True for unbuffered device
    /// handles, false for ordinary files.
    fn requires_alignment(&self) -> bool;

    /// Total readable length, when known. Devices often cannot say without an
    /// ioctl the caller has not made yet.
    fn span(&self) -> Option<u64>;

    /// Short human-readable name, used in error messages.
    fn label(&self) -> &str;

    /// Fill `buf` completely, or fail.
    ///
    /// Retries on partial reads, which are legal and do occur on large device
    /// reads even when the bytes are there.
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let wanted = buf.len();
        let mut done = 0usize;

        while done < wanted {
            let got = self.read_at(offset + done as u64, &mut buf[done..])?;
            if got == 0 {
                return Err(Error::ShortRead {
                    device: self.label().to_string(),
                    offset,
                    wanted,
                    got: done,
                });
            }
            done += got;
        }
        Ok(())
    }
}

/// A [`BlockSource`] backed by a file handle.
///
/// Covers all three cases, because on Windows a raw volume and a physical
/// drive are both openable as files: `\\.\C:` with no base offset,
/// `\\.\PhysicalDrive0` with the partition's offset as base, and a `.dd` image
/// likewise. One implementation, three uses.
#[derive(Debug)]
pub struct FileBlockSource {
    file: File,
    /// Added to every requested offset. The partition's start, for sources
    /// that address a partition within a larger device.
    base: u64,
    /// Length of the readable window beginning at `base`.
    span: Option<u64>,
    sector_size: u32,
    requires_alignment: bool,
    label: String,
}

impl FileBlockSource {
    /// Open a disk image, or any ordinary file. Buffered, so reads need no
    /// alignment.
    pub fn open_image(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| Error::Device {
            path: path.display().to_string(),
            source,
        })?;
        let span = file.metadata().ok().map(|m| m.len());

        Ok(Self {
            file,
            base: 0,
            span,
            sector_size: DEFAULT_SECTOR_SIZE,
            requires_alignment: false,
            label: path.display().to_string(),
        })
    }

    /// Open a raw Windows device - `\\.\C:` for a volume, `\\.\PhysicalDrive0`
    /// for a whole disk.
    ///
    /// Opened unbuffered, which is what makes bulk metadata reads fast: the
    /// cache is pure overhead when sweeping millions of records once. The cost
    /// is that every read must be sector-aligned in offset, length, and buffer
    /// address - see [`AlignedBuf`].
    ///
    /// Requires Administrator. Sharing is permissive because the volume is
    /// live and in use by everything else on the system.
    #[cfg(windows)]
    pub fn open_device(path: &str) -> Result<Self> {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;

        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_NO_BUFFERING)
            .open(path)
            .map_err(|source| Error::Device {
                path: path.to_string(),
                source,
            })?;

        Ok(Self {
            file,
            base: 0,
            span: None,
            sector_size: DEFAULT_SECTOR_SIZE,
            requires_alignment: true,
            label: path.to_string(),
        })
    }

    /// Restrict this source to a window of `span` bytes starting at `base`.
    ///
    /// Callers then address the partition from zero, and reads cannot report
    /// bytes belonging to a neighbouring partition.
    #[must_use]
    pub fn partition(mut self, base: u64, span: Option<u64>) -> Self {
        self.base = base;
        self.span = span;
        self
    }

    /// Set the sector size. Native 4Kn drives need this; without it every
    /// unbuffered read fails.
    #[must_use]
    pub fn with_sector_size(mut self, sector_size: u32) -> Self {
        debug_assert!(
            sector_size.is_power_of_two() && sector_size > 0,
            "sector size must be a power of two"
        );
        self.sector_size = sector_size;
        self
    }

    /// Force alignment checking on or off. Set automatically by the
    /// constructors; exposed for tests and for callers that reopen a device
    /// buffered.
    #[must_use]
    pub fn with_alignment_required(mut self, required: bool) -> Self {
        self.requires_alignment = required;
        self
    }

    /// Replace the label used in error messages.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    fn check_alignment(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let sector = u64::from(self.sector_size);
        let buffer_align = buf.as_ptr() as usize % self.sector_size as usize;

        if offset.is_multiple_of(sector)
            && (buf.len() as u64).is_multiple_of(sector)
            && buffer_align == 0
        {
            return Ok(());
        }
        Err(Error::Misaligned {
            device: self.label.clone(),
            offset,
            len: buf.len(),
            buffer_align,
            sector_size: self.sector_size,
        })
    }
}

impl BlockSource for FileBlockSource {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if self.requires_alignment {
            self.check_alignment(offset, buf)?;
        }

        // Past the end of our window: end-of-source, not an error.
        if let Some(span) = self.span
            && offset >= span
        {
            return Ok(0);
        }

        let absolute = self
            .base
            .checked_add(offset)
            .ok_or_else(|| Error::BlockRead {
                device: self.label.clone(),
                offset,
                len: buf.len(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "offset overflows the device address space",
                ),
            })?;

        let got =
            read_positional(&self.file, absolute, buf).map_err(|source| Error::BlockRead {
                device: self.label.clone(),
                offset,
                len: buf.len(),
                source,
            })?;

        // A tail read may be sector-padded past the end of the partition and
        // so pull in bytes belonging to whatever follows. The read itself is
        // fine - the buffer has to stay aligned - but we must not report those
        // bytes as ours.
        if let Some(span) = self.span {
            let remaining = (span - offset).min(got as u64);
            return Ok(remaining as usize);
        }
        Ok(got)
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn requires_alignment(&self) -> bool {
        self.requires_alignment
    }

    fn span(&self) -> Option<u64> {
        self.span
    }

    fn label(&self) -> &str {
        &self.label
    }
}

/// Read at an absolute offset without touching the file's cursor.
///
/// Both platform calls take `&File`, which is what lets one source serve many
/// threads at once.
#[cfg(windows)]
fn read_positional(file: &File, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

#[cfg(unix)]
fn read_positional(file: &File, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

#[cfg(not(any(windows, unix)))]
compile_error!("BlockSource needs a positional read; only Windows and Unix are implemented");

/// A byte buffer whose start address is sector-aligned.
///
/// Unbuffered device reads require the *buffer's address* to be a multiple of
/// the sector size, not merely the offset and length. `vec![0u8; n]` gives only
/// the allocator's alignment; it happens to satisfy large requests and happens
/// to fail small ones, which is how v1's one-kilobyte record buffers worked by
/// luck.
///
/// Obtained without `unsafe` by over-allocating and slicing to the first
/// aligned position. The waste is under one sector.
pub struct AlignedBuf {
    raw: Vec<u8>,
    offset: usize,
    len: usize,
}

impl AlignedBuf {
    /// A zeroed buffer of `len` bytes starting on an `align`-byte boundary.
    ///
    /// # Panics
    /// If `align` is zero or not a power of two.
    pub fn new(len: usize, align: usize) -> Self {
        assert!(
            align > 0 && align.is_power_of_two(),
            "alignment must be a power of two, got {align}"
        );

        // Never reallocated after this point, so the address stays put.
        let raw = vec![0u8; len + align - 1];
        let offset = align - 1 - ((raw.as_ptr() as usize).wrapping_add(align - 1) % align);

        debug_assert_eq!((raw.as_ptr() as usize + offset) % align, 0);
        debug_assert!(offset + len <= raw.len());

        Self { raw, offset, len }
    }

    /// A buffer sized and aligned for `source`, holding `len` bytes.
    pub fn for_source(source: &dyn BlockSource, len: usize) -> Self {
        Self::new(len, source.sector_size() as usize)
    }

    /// The aligned bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.raw[self.offset..self.offset + self.len]
    }

    /// The aligned bytes, mutably. Pass this to [`BlockSource::read_at`].
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.raw[self.offset..self.offset + self.len]
    }

    /// Usable length, excluding alignment padding.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when the buffer holds no bytes.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl std::fmt::Debug for AlignedBuf {
    /// Shows the shape, never the contents - these buffers hold megabytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlignedBuf")
            .field("len", &self.len)
            .field("address", &format_args!("{:p}", self.as_slice().as_ptr()))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_buffers_start_on_a_boundary() {
        for align in [512usize, 4096, 8192] {
            for len in [1usize, 511, 1024, 65536] {
                let buf = AlignedBuf::new(len, align);
                assert_eq!(buf.len(), len);
                assert_eq!(
                    buf.as_slice().as_ptr() as usize % align,
                    0,
                    "len {len} align {align}"
                );
            }
        }
    }

    #[test]
    fn aligned_buffers_are_zeroed_and_writable() {
        let mut buf = AlignedBuf::new(2048, 512);
        assert!(buf.as_slice().iter().all(|&b| b == 0));
        buf.as_mut_slice()[0] = 0xAB;
        buf.as_mut_slice()[2047] = 0xCD;
        assert_eq!(buf.as_slice()[0], 0xAB);
        assert_eq!(buf.as_slice()[2047], 0xCD);
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn alignment_must_be_a_power_of_two() {
        let _ = AlignedBuf::new(512, 3);
    }
}
