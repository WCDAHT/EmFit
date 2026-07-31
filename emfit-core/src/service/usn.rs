//! USN change-journal reader — deletion watching (roadmap M5, rescoped
//! 2026-07-30).
//!
//! The watcher observes exactly one thing: **file deletions** — in both
//! forms. Shift+Del (and emptying the bin) is a true `FILE_DELETE`; plain
//! Del is NOT a delete at the filesystem level: the shell *renames* the
//! file into `$Recycle.Bin\<SID>\`, which the journal reports as
//! `RENAME_NEW_NAME`. The watcher therefore subscribes to both bits and
//! reports raw events with the post-event parent; the caller classifies a
//! rename as "recycled" when the new parent is a bin directory, and
//! ignores every other rename. A deleted node is *marked*, never removed —
//! no sizes re-roll, creations are invisible, and a path reappearing (or a
//! restore from the bin) does not clear its mark; only a rescan does.
//!
//! [`UsnWatcher::open`] snapshots the journal's current position, so only
//! deletions from that moment forward are reported (anything between scan
//! completion and watcher start is missed — the next rescan reconciles).
//! [`UsnWatcher::poll`] is non-blocking and meant for a ~1 s cadence from a
//! caller-owned thread; it returns the deleted file reference numbers,
//! which map onto index nodes via [`Index::native_id`].
//!
//! [`Index::native_id`]: crate::model::index::Index::native_id
//!
//! # `unsafe`
//!
//! Windows-only ioctls, same pattern as [`crate::service::volume`]: the
//! module overrides the workspace `unsafe_code` deny, every block wraps one
//! call and documents why it is sound.

use crate::error::Result;

/// One journal record the watcher cares about. `reason` carries the raw
/// bits; `parent_frn` is where the file lives *after* the event — which is
/// what lets the caller tell a Recycle-Bin move (parent = a bin directory)
/// from an ordinary rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsnEvent {
    pub frn: u64,
    pub parent_frn: u64,
    pub reason: u32,
}

/// What one poll of the journal produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsnPoll {
    /// Records since the last poll matching the watch mask: true deletes
    /// and renames (the caller classifies recycles). Often empty.
    Events(Vec<UsnEvent>),
    /// The journal wrapped, was deleted, or changed identity: events were
    /// lost and the marks can no longer be trusted complete. The watcher is
    /// done; only a rescan re-establishes truth.
    Gap,
}

/// `USN_REASON_FILE_DELETE`: the file truly ceased to exist (Shift+Del,
/// emptied bin, direct deletion).
pub const REASON_FILE_DELETE: u32 = 0x0000_0200;
/// `USN_REASON_RENAME_NEW_NAME`: the file got a new name/location. Plain
/// Del is THIS, not a delete — the shell "deletes" to the bin by renaming
/// into `$Recycle.Bin\<SID>\`, so recycle detection lives on this bit.
pub const REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;

/// The bits the watcher subscribes to.
const REASON_MASK: u32 = REASON_FILE_DELETE | REASON_RENAME_NEW_NAME;

/// One volume's deletion watcher. Construct with [`UsnWatcher::open`], then
/// [`UsnWatcher::poll`] on a cadence until `Gap` or cancellation.
pub struct UsnWatcher {
    #[cfg(windows)]
    inner: windows_impl::Watcher,
}

impl UsnWatcher {
    /// Open the change journal of `X:` and position at its current tail.
    /// Needs the same volume-handle access as a raw scan (elevation).
    pub fn open(drive_letter: char) -> Result<Self> {
        #[cfg(windows)]
        {
            Ok(Self {
                inner: windows_impl::Watcher::open(drive_letter)?,
            })
        }
        #[cfg(not(windows))]
        {
            let _ = drive_letter;
            Err(crate::error::Error::UnsupportedPlatform {
                operation: "USN journal watching".to_string(),
            })
        }
    }

    /// Drain everything new since the last poll. Non-blocking.
    pub fn poll(&mut self) -> Result<UsnPoll> {
        #[cfg(windows)]
        {
            self.inner.poll()
        }
        #[cfg(not(windows))]
        {
            unreachable!("open() cannot succeed off Windows")
        }
    }
}

/// Parse one `FSCTL_READ_USN_JOURNAL` output buffer: a leading next-USN
/// `i64`, then a run of `USN_RECORD_V2`. Returns the next USN (None when
/// the buffer held no header) and the events carrying any `reason_mask`
/// bit.
///
/// Pure, so the record walk is testable on every platform. V2 layout:
/// `RecordLength: u32 @0`, `MajorVersion: u16 @4`, `FileReferenceNumber:
/// u64 @8`, `ParentFileReferenceNumber: u64 @16`, `Reason: u32 @40`.
/// Records from NTFS via the V0 read struct are always V2; anything else
/// is skipped by length.
fn parse_events(buf: &[u8], reason_mask: u32) -> (Option<i64>, Vec<UsnEvent>) {
    const HEADER: usize = 8;
    const RECORD_MIN: usize = 60; // sizeof(USN_RECORD_V2) before the name

    if buf.len() < HEADER {
        return (None, Vec::new());
    }
    let next = i64::from_le_bytes(buf[0..8].try_into().expect("8-byte slice"));

    let mut out = Vec::new();
    let mut at = HEADER;
    while at + RECORD_MIN <= buf.len() {
        let len = u32::from_le_bytes(buf[at..at + 4].try_into().expect("4-byte slice")) as usize;
        if len < RECORD_MIN || at + len > buf.len() {
            break; // truncated or corrupt tail
        }
        let major = u16::from_le_bytes(buf[at + 4..at + 6].try_into().expect("2-byte slice"));
        if major == 2 {
            let frn =
                u64::from_le_bytes(buf[at + 8..at + 16].try_into().expect("8-byte slice"));
            let parent_frn =
                u64::from_le_bytes(buf[at + 16..at + 24].try_into().expect("8-byte slice"));
            let reason =
                u32::from_le_bytes(buf[at + 40..at + 44].try_into().expect("4-byte slice"));
            if reason & reason_mask != 0 {
                out.push(UsnEvent {
                    frn,
                    parent_frn,
                    reason,
                });
            }
        }
        at += len;
    }
    (Some(next), out)
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "FSCTL_QUERY/READ_USN_JOURNAL have no std equivalent; each \
              block wraps one call and documents its preconditions"
)]
mod windows_impl {
    use windows::Win32::Foundation::{CloseHandle, ERROR_HANDLE_EOF, HANDLE, WIN32_ERROR};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{
        FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0,
        USN_JOURNAL_DATA_V0,
    };
    use windows::core::PCWSTR;

    use super::{REASON_MASK, UsnPoll, parse_events};
    use crate::error::{Error, Result};

    /// Journal-loss codes that mean "gap", not "failure":
    /// entry deleted from under us (1181), journal being deleted (1178),
    /// journal not active (1179), and invalid-parameter (87) — what a
    /// changed journal id surfaces as.
    const GAP_CODES: [u32; 4] = [1181, 1178, 1179, 87];

    /// At most this many 64 KiB reads per poll, so one poll drains a mass
    /// delete quickly without ever spinning unbounded.
    const MAX_READS_PER_POLL: usize = 32;

    pub struct Watcher {
        handle: HANDLE,
        journal_id: u64,
        next_usn: i64,
        buf: Vec<u8>,
    }

    // SAFETY: the volume handle is used exclusively by the one watcher
    // thread that owns this struct; Windows handles are not thread-affine.
    unsafe impl Send for Watcher {}

    impl Drop for Watcher {
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

    impl Watcher {
        pub fn open(drive_letter: char) -> Result<Self> {
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

            let mut data = USN_JOURNAL_DATA_V0::default();
            let mut bytes = 0u32;
            // SAFETY: valid handle; out-struct and byte count are plain
            // out-params sized correctly.
            let query = unsafe {
                DeviceIoControl(
                    handle,
                    FSCTL_QUERY_USN_JOURNAL,
                    None,
                    0,
                    Some(&raw mut data as *mut _),
                    std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
                    Some(&mut bytes),
                    None,
                )
            };
            if query.is_err() {
                let e = api_error("FSCTL_QUERY_USN_JOURNAL");
                // SAFETY: close the handle we just opened; Drop won't run.
                let _ = unsafe { CloseHandle(handle) };
                return Err(e);
            }

            tracing::info!(
                drive = %drive_letter,
                journal_id = format_args!("{:#x}", data.UsnJournalID),
                first_usn = data.FirstUsn,
                next_usn = data.NextUsn,
                "usn: journal opened, starting at the current tail"
            );
            Ok(Self {
                handle,
                journal_id: data.UsnJournalID,
                next_usn: data.NextUsn,
                buf: vec![0u8; 64 * 1024],
            })
        }

        pub fn poll(&mut self) -> Result<UsnPoll> {
            let mut events = Vec::new();
            for _ in 0..MAX_READS_PER_POLL {
                let read = READ_USN_JOURNAL_DATA_V0 {
                    StartUsn: self.next_usn,
                    ReasonMask: REASON_MASK,
                    ReturnOnlyOnClose: 0,
                    Timeout: 0,
                    BytesToWaitFor: 0,
                    UsnJournalID: self.journal_id,
                };
                let mut bytes = 0u32;
                // SAFETY: valid handle; in-struct and out-buffer are live
                // for the call, sizes match the allocations.
                let result = unsafe {
                    DeviceIoControl(
                        self.handle,
                        FSCTL_READ_USN_JOURNAL,
                        Some(&raw const read as *const _),
                        std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                        Some(self.buf.as_mut_ptr().cast()),
                        self.buf.len() as u32,
                        Some(&mut bytes),
                        None,
                    )
                };
                if let Err(e) = result {
                    let code = WIN32_ERROR::from_error(&e).unwrap_or_default().0;
                    if code == ERROR_HANDLE_EOF.0 {
                        break; // nothing new yet
                    }
                    if GAP_CODES.contains(&code) {
                        tracing::warn!(code, "usn: journal gap signalled by read");
                        return Ok(UsnPoll::Gap);
                    }
                    tracing::warn!(code, "usn: read failed");
                    return Err(api_error("FSCTL_READ_USN_JOURNAL"));
                }

                let (next, matched) = parse_events(&self.buf[..bytes as usize], REASON_MASK);
                if bytes > 8 {
                    tracing::debug!(
                        bytes,
                        matched = matched.len(),
                        start_usn = self.next_usn,
                        next_usn = next,
                        "usn: read returned records"
                    );
                }
                let Some(next) = next else {
                    break; // header-less response: no data
                };
                let drained = bytes as usize <= 8;
                let stalled = next == self.next_usn;
                self.next_usn = next;
                events.extend(matched);
                if drained || stalled {
                    break;
                }
            }
            Ok(UsnPoll::Events(events))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Append one USN_RECORD_V2 with the given FRN, parent, and reason.
    fn push_record(buf: &mut Vec<u8>, frn: u64, parent: u64, reason: u32, len: usize) {
        let start = buf.len();
        buf.resize(start + len, 0);
        buf[start..start + 4].copy_from_slice(&(len as u32).to_le_bytes());
        buf[start + 4..start + 6].copy_from_slice(&2u16.to_le_bytes()); // major
        buf[start + 8..start + 16].copy_from_slice(&frn.to_le_bytes());
        buf[start + 16..start + 24].copy_from_slice(&parent.to_le_bytes());
        buf[start + 40..start + 44].copy_from_slice(&reason.to_le_bytes());
    }

    #[test]
    fn parses_header_and_matching_records() {
        let mut buf = 42i64.to_le_bytes().to_vec();
        push_record(&mut buf, 100, 5, REASON_FILE_DELETE, 64);
        push_record(&mut buf, 200, 5, 0x0000_0001, 72); // data-overwrite: ignored
        push_record(&mut buf, 300, 7, REASON_FILE_DELETE | 0x8000_0000, 60);

        let (next, events) = parse_events(&buf, REASON_MASK);
        assert_eq!(next, Some(42));
        assert_eq!(
            events,
            vec![
                UsnEvent {
                    frn: 100,
                    parent_frn: 5,
                    reason: REASON_FILE_DELETE
                },
                UsnEvent {
                    frn: 300,
                    parent_frn: 7,
                    reason: REASON_FILE_DELETE | 0x8000_0000
                },
            ]
        );
    }

    #[test]
    fn renames_carry_their_new_parent() {
        // A recycle is a rename whose new parent is the bin — the parent is
        // exactly what the caller classifies on.
        let mut buf = 9i64.to_le_bytes().to_vec();
        push_record(&mut buf, 555, 0xBEEF, REASON_RENAME_NEW_NAME, 60);

        let (_, events) = parse_events(&buf, REASON_MASK);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].frn, 555);
        assert_eq!(events[0].parent_frn, 0xBEEF);
        assert_eq!(events[0].reason, REASON_RENAME_NEW_NAME);
    }

    #[test]
    fn empty_and_headerless_buffers_yield_nothing() {
        assert_eq!(parse_events(&[], REASON_MASK), (None, Vec::new()));
        let (next, events) = parse_events(&7i64.to_le_bytes(), REASON_MASK);
        assert_eq!(next, Some(7));
        assert!(events.is_empty());
    }

    #[test]
    fn corrupt_lengths_stop_the_walk_instead_of_panicking() {
        let mut buf = 1i64.to_le_bytes().to_vec();
        push_record(&mut buf, 100, 5, REASON_FILE_DELETE, 64);
        // A record claiming to run past the buffer.
        let start = buf.len();
        buf.resize(start + 60, 0);
        buf[start..start + 4].copy_from_slice(&10_000u32.to_le_bytes());
        buf[start + 4..start + 6].copy_from_slice(&2u16.to_le_bytes());

        let (_, events) = parse_events(&buf, REASON_MASK);
        assert_eq!(events.len(), 1, "the valid prefix still parses");
        assert_eq!(events[0].frn, 100);

        // Zero-length record: must not loop forever.
        let mut buf = 1i64.to_le_bytes().to_vec();
        buf.resize(8 + 60, 0);
        let (_, events) = parse_events(&buf, REASON_MASK);
        assert!(events.is_empty());
    }

    #[test]
    fn non_v2_records_are_skipped_by_length() {
        let mut buf = 5i64.to_le_bytes().to_vec();
        // A "V4" record that would match the mask if version were ignored.
        let start = buf.len();
        buf.resize(start + 64, 0);
        buf[start..start + 4].copy_from_slice(&64u32.to_le_bytes());
        buf[start + 4..start + 6].copy_from_slice(&4u16.to_le_bytes());
        buf[start + 8..start + 16].copy_from_slice(&900u64.to_le_bytes());
        buf[start + 40..start + 44].copy_from_slice(&REASON_FILE_DELETE.to_le_bytes());
        push_record(&mut buf, 901, 5, REASON_FILE_DELETE, 60);

        let (_, events) = parse_events(&buf, REASON_MASK);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].frn, 901);
    }
}
