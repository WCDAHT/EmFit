//! MFT records read through the filesystem driver, not off the platter.
//!
//! A sweep reads the MFT as bytes on disk, which is right: it is one huge
//! sequential read and the table is as current as any snapshot can be. Journal
//! replay is the opposite case. It asks about records that changed *seconds
//! ago*, and NTFS keeps modified metadata in its own cache and writes it out
//! lazily - so a raw read of a just-created file's record shows the slot still
//! free, a just-deleted file's record still in use, and a just-written file's
//! old size. The journal knows immediately; the platter does not.
//!
//! `FSCTL_GET_NTFS_FILE_RECORD` asks NTFS itself for a record, which it serves
//! from the same cache it is about to write down. Same bytes, same parser,
//! current answer.
//!
//! Three details decide whether this is safe to build on:
//!
//! - **A free record does not fail.** The driver returns the nearest in-use
//!   record at or below the one asked for, so the answer has to be checked
//!   against the request. Skipping that check would attribute one file's
//!   record to another file's number - and during replay, that means writing
//!   the wrong file into the index.
//! - **The fixup may already be applied.** Windows serves these records
//!   repaired: the bytes NTFS displaced with its per-sector check value are
//!   already back in place. Applying the fixup again would corrupt two bytes
//!   per sector. That is not documented anywhere reliable and could differ by
//!   version, so [`LiveRecords::open`] *determines* the form from the record
//!   it fetches rather than assuming either answer - see [`probe_form`].
//! - **[`LiveRecords::open`] proves the ioctl works before anything relies on
//!   it**, by fetching the root directory and requiring a record that parses
//!   and names itself. Every systematic failure - unsupported filesystem,
//!   insufficient rights, a layout this parser does not expect - then surfaces
//!   as a refusal to open rather than as an empty result that would read as
//!   "everything was deleted".
//!
//! # `unsafe`
//!
//! Windows-only ioctls, same pattern as [`crate::service::usn`]: the module
//! overrides the workspace `unsafe_code` deny, every block wraps one call and
//! documents why it is sound.

use crate::error::Result;
use crate::parser::ntfs::attr::AttributeType;
use crate::parser::ntfs::record::{Record, RecordForm, RecordHeader};
use crate::parser::ntfs::scanner::ROOT_RECORD;

/// A handle on one volume's MFT, served by the filesystem.
pub struct LiveRecords {
    #[cfg(windows)]
    inner: windows_impl::Live,
}

impl LiveRecords {
    /// Open `X:` and confirm the filesystem will serve file records.
    ///
    /// The confirmation is not ceremony: everything downstream treats "no
    /// record" as "the file is gone", so an ioctl that fails wholesale must
    /// not look like a volume that emptied itself.
    pub fn open(drive_letter: char, bytes_per_sector: u32, bytes_per_record: u32) -> Result<Self> {
        #[cfg(windows)]
        {
            Ok(Self {
                inner: windows_impl::Live::open(drive_letter, bytes_per_sector, bytes_per_record)?,
            })
        }
        #[cfg(not(windows))]
        {
            let _ = (drive_letter, bytes_per_sector, bytes_per_record);
            Err(crate::error::Error::UnsupportedPlatform {
                operation: "reading MFT records through the filesystem".to_string(),
            })
        }
    }

    /// Which form [`LiveRecords::read`] returns records in, as determined when
    /// the volume was opened.
    pub fn form(&self) -> RecordForm {
        #[cfg(windows)]
        {
            self.inner.form
        }
        #[cfg(not(windows))]
        {
            RecordForm::OnDisk
        }
    }

    /// The bytes of one record, in the form [`LiveRecords::form`] reports.
    ///
    /// `None` means the record is not in use - which is exactly how a deleted
    /// file is observed.
    pub fn read(&mut self, record: u64) -> Result<Option<&mut [u8]>> {
        #[cfg(windows)]
        {
            self.inner.read(record)
        }
        #[cfg(not(windows))]
        {
            let _ = record;
            unreachable!("open() cannot succeed off Windows")
        }
    }
}

/// The record every NTFS volume has in use, used to prove the ioctl works.
const PROBE_RECORD: u64 = ROOT_RECORD;

/// Work out which form a served record is in, by parsing it both ways.
///
/// Not a guess: a record only counts as parsed when it comes out naming
/// itself, so the wrong reading is rejected on its content rather than on its
/// plausibility. On-disk is tried first because its check value has to match
/// exactly, which repaired bytes will not do by accident.
fn probe_form(bytes: &[u8], bytes_per_sector: u32) -> Option<RecordForm> {
    let mut scratch = bytes.to_vec();
    if let Ok(record) = Record::parse(&mut scratch, bytes_per_sector)
        && has_name(&record)
    {
        return Some(RecordForm::OnDisk);
    }
    let header = RecordHeader::parse(bytes).ok()?;
    let record = Record::from_repaired(bytes, header).ok()?;
    has_name(&record).then_some(RecordForm::Repaired)
}

/// Whether a record carries a `$FILE_NAME` - cheap proof that its attributes
/// were read as attributes and not as noise.
fn has_name(record: &Record<'_>) -> bool {
    record
        .attributes()
        .any(|attribute| attribute.kind() == AttributeType::FileName)
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "FSCTL_GET_NTFS_FILE_RECORD has no std equivalent; each block \
              wraps one call and documents its preconditions"
)]
mod windows_impl {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WIN32_ERROR};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{
        FSCTL_GET_NTFS_FILE_RECORD, NTFS_FILE_RECORD_INPUT_BUFFER, NTFS_FILE_RECORD_OUTPUT_BUFFER,
    };
    use windows::core::PCWSTR;

    use super::{PROBE_RECORD, probe_form};
    use crate::error::{Error, Result};
    use crate::model::index::FS_OBJECT_MASK;
    use crate::parser::ntfs::record::{RecordForm, RecordHeader};

    /// Where the record bytes begin in the output buffer, after the reference
    /// number and the length.
    const RECORD_AT: usize = std::mem::offset_of!(NTFS_FILE_RECORD_OUTPUT_BUFFER, FileRecordBuffer);

    /// `ERROR_INVALID_PARAMETER`: what a record number past the end of the
    /// table comes back as. Not a failure - there is simply no such record.
    const OUT_OF_RANGE: u32 = 87;

    pub struct Live {
        handle: HANDLE,
        record_size: usize,
        /// Determined at open time from a record the filesystem served, never
        /// assumed. See `probe_form`.
        pub form: RecordForm,
        buf: Vec<u8>,
    }

    // SAFETY: the handle is used only by the thread owning this struct;
    // Windows handles are not thread-affine.
    unsafe impl Send for Live {}

    impl Drop for Live {
        fn drop(&mut self) {
            // SAFETY: closing the handle opened in `open`, exactly once.
            let _ = unsafe { CloseHandle(self.handle) };
        }
    }

    fn api_error(api: &str) -> Error {
        Error::WindowsApi {
            api: api.to_string(),
            source: std::io::Error::last_os_error(),
        }
    }

    impl Live {
        pub fn open(
            drive_letter: char,
            bytes_per_sector: u32,
            bytes_per_record: u32,
        ) -> Result<Self> {
            let path: Vec<u16> = format!("\\\\.\\{drive_letter}:")
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            // SAFETY: valid NUL-terminated path; the handle's lifetime is
            // managed by Drop.
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(path.as_ptr()),
                    FILE_GENERIC_READ.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                )
            }
            .map_err(|_| api_error("CreateFileW(\\\\.\\X:)"))?;

            let record_size = bytes_per_record.max(1024) as usize;
            let mut live = Self {
                handle,
                record_size,
                // Replaced below; nothing reads it before then.
                form: RecordForm::OnDisk,
                buf: vec![0u8; RECORD_AT + record_size + 64],
            };

            // Prove it before trusting it, and learn which form it serves. A
            // silent failure here would read downstream as "every changed file
            // is gone".
            let probe = match live.read(PROBE_RECORD)? {
                Some(bytes) => bytes.to_vec(),
                None => {
                    return Err(Error::Parse {
                        format: "live MFT record".to_string(),
                        message: format!("the filesystem reports record {PROBE_RECORD} unused"),
                    });
                }
            };
            live.form = probe_form(&probe, bytes_per_sector).ok_or_else(|| Error::Parse {
                format: "live MFT record".to_string(),
                message: format!(
                    "the filesystem served record {PROBE_RECORD} in a form this parser does                      not recognize"
                ),
            })?;

            tracing::debug!(
                drive = %drive_letter,
                record_size,
                form = ?live.form,
                "live: the filesystem is serving MFT records"
            );
            Ok(live)
        }

        pub fn read(&mut self, record: u64) -> Result<Option<&mut [u8]>> {
            let input = NTFS_FILE_RECORD_INPUT_BUFFER {
                FileReferenceNumber: (record & FS_OBJECT_MASK) as i64,
            };
            let mut bytes = 0u32;
            // SAFETY: valid handle; the in-struct and the out-buffer are live
            // for the call and their sizes match the allocations.
            let result = unsafe {
                DeviceIoControl(
                    self.handle,
                    FSCTL_GET_NTFS_FILE_RECORD,
                    Some(&raw const input as *const _),
                    std::mem::size_of::<NTFS_FILE_RECORD_INPUT_BUFFER>() as u32,
                    Some(self.buf.as_mut_ptr().cast()),
                    self.buf.len() as u32,
                    Some(&mut bytes),
                    None,
                )
            };
            if let Err(e) = result {
                let code = WIN32_ERROR::from_error(&e).unwrap_or_default().0;
                if code == OUT_OF_RANGE {
                    return Ok(None);
                }
                return Err(api_error("FSCTL_GET_NTFS_FILE_RECORD"));
            }
            if (bytes as usize) < RECORD_AT {
                return Ok(None);
            }

            let returned = u64::from_le_bytes(self.buf[0..8].try_into().expect("8-byte slice"));
            // A record that is not in use comes back as the nearest in-use one
            // below it. Believing that would put one file's metadata under
            // another file's number.
            if returned & FS_OBJECT_MASK != record & FS_OBJECT_MASK {
                return Ok(None);
            }

            let len =
                u32::from_le_bytes(self.buf[8..12].try_into().expect("4-byte slice")) as usize;
            let len = len.min(self.record_size).min(bytes as usize - RECORD_AT);
            if len == 0 {
                return Ok(None);
            }
            let bytes = &mut self.buf[RECORD_AT..RECORD_AT + len];

            // A record whose header does not parse is not this record.
            if RecordHeader::parse(bytes).is_err() {
                return Ok(None);
            }
            Ok(Some(bytes))
        }
    }
}
