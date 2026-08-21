//! Asking the filesystem driver where `$MFT` is.
//!
//! `FSCTL_GET_RETRIEVAL_POINTERS` returns a file's extent map. It is the fast
//! path for locating the MFT: no parsing, no bootstrap, one ioctl. When it is
//! unavailable - a raw image, an unmounted volume, no rights to open `$MFT` -
//! the caller falls back to decoding record 0's `$DATA` run list, which
//! reaches the same answer the slow way.
//!
//! # The loop that matters
//!
//! The output buffer holds a fixed number of extents. On a heavily fragmented
//! MFT the driver fills it, returns `ERROR_MORE_DATA`, and **expects to be
//! asked again** starting from the last extent's ending VCN. A single call
//! returns a *prefix* of the map with no indication it is short. v1 made one
//! 64 KB call and took what it got; every record past roughly the four
//! thousandth fragment would then be read from the wrong offset.
//!
//! # Platforms
//!
//! Windows-only. [`mft_extents`] exists everywhere and returns
//! [`Error::UnsupportedPlatform`] elsewhere.

use crate::error::Result;
use crate::parser::ntfs::extents::MftExtents;

/// Read `$MFT`'s extent map from the filesystem driver.
///
/// `drive_letter` is the volume to interrogate; the geometry comes from the
/// caller because the boot sector has already been read by this point.
pub fn mft_extents(
    drive_letter: char,
    bytes_per_cluster: u32,
    bytes_per_record: u32,
) -> Result<MftExtents> {
    #[cfg(windows)]
    {
        windows_impl::mft_extents(drive_letter, bytes_per_cluster, bytes_per_record)
    }
    #[cfg(not(windows))]
    {
        let _ = (drive_letter, bytes_per_cluster, bytes_per_record);
        Err(crate::error::Error::UnsupportedPlatform {
            operation: "FSCTL_GET_RETRIEVAL_POINTERS".to_string(),
        })
    }
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "FSCTL_GET_RETRIEVAL_POINTERS has no safe std equivalent; the one \
              block wraps a single DeviceIoControl and documents its preconditions"
)]
mod windows_impl {
    use std::os::windows::io::AsRawHandle;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::DeviceIoControl;

    use crate::error::{Error, Result};
    use crate::parser::ntfs::extents::{Extent, MftExtents};

    /// `CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 28, METHOD_NEITHER, FILE_ANY_ACCESS)`.
    const FSCTL_GET_RETRIEVAL_POINTERS: u32 = 0x0009_0073;

    /// The driver filled the buffer and has more to give.
    const ERROR_MORE_DATA: i32 = 234;
    /// No extents at all past this point.
    const ERROR_HANDLE_EOF: i32 = 38;

    /// 64 KiB holds about four thousand extents per call. Larger buffers buy
    /// little: the loop handles the remainder either way.
    const BUFFER_LEN: usize = 64 * 1024;

    /// `RETRIEVAL_POINTERS_BUFFER` header: `DWORD ExtentCount` (padded to 8)
    /// then `LARGE_INTEGER StartingVcn`.
    const HEADER_LEN: usize = 16;
    /// Each extent is `LARGE_INTEGER NextVcn` followed by `LARGE_INTEGER Lcn`.
    const EXTENT_LEN: usize = 16;

    /// A fragment count past which the volume is not merely fragmented but
    /// broken, and we would rather stop than loop forever on a driver that
    /// keeps saying "more".
    const MAX_EXTENTS: usize = 1_000_000;

    pub(super) fn mft_extents(
        drive_letter: char,
        bytes_per_cluster: u32,
        bytes_per_record: u32,
    ) -> Result<MftExtents> {
        let file = open_mft(drive_letter)?;
        let mut buf = vec![0u8; BUFFER_LEN];
        let mut extents: Vec<Extent> = Vec::new();

        // Where the next call should resume, and where the extent we are about
        // to read begins. The driver reports each extent's *ending* VCN, so the
        // start is the previous extent's end.
        let mut next_vcn: u64 = 0;
        let mut run_start_vcn: u64 = 0;

        loop {
            let input = next_vcn.to_le_bytes();
            let mut returned = 0u32;

            // SAFETY: the handle is open and owned by `file` for the call; both
            // buffers are live locals whose true sizes are passed.
            let outcome = unsafe {
                DeviceIoControl(
                    HANDLE(file.as_raw_handle()),
                    FSCTL_GET_RETRIEVAL_POINTERS,
                    Some(input.as_ptr().cast()),
                    input.len() as u32,
                    Some(buf.as_mut_ptr().cast()),
                    buf.len() as u32,
                    Some(&mut returned),
                    None,
                )
            };

            // A partial fill is reported as an error *and* fills the buffer, so
            // the returned data must be parsed before the status is judged.
            let more = match &outcome {
                Ok(()) => false,
                Err(e) => match e.code().0 & 0xFFFF {
                    ERROR_MORE_DATA => true,
                    ERROR_HANDLE_EOF => break,
                    _ => {
                        if extents.is_empty() {
                            return Err(Error::WindowsApi {
                                api: "DeviceIoControl(FSCTL_GET_RETRIEVAL_POINTERS)".to_string(),
                                source: std::io::Error::from_raw_os_error(e.code().0),
                            });
                        }
                        // Partway through with a usable prefix: stop and let
                        // validation in `MftExtents::new` judge it.
                        tracing::warn!(error = %e, "retrieval pointers ended early");
                        break;
                    }
                },
            };

            let parsed = parse_buffer(&buf, returned as usize, &mut run_start_vcn, &mut extents);
            if parsed == 0 {
                // No progress; a further identical call would spin.
                break;
            }
            if extents.len() > MAX_EXTENTS {
                return Err(Error::Parse {
                    format: "MFT extents".to_string(),
                    message: format!("more than {MAX_EXTENTS} fragments; volume looks corrupt"),
                });
            }
            if !more {
                break;
            }
            next_vcn = run_start_vcn;
        }

        tracing::info!(
            fragments = extents.len(),
            "read $MFT extent map from the filesystem"
        );
        MftExtents::new(extents, bytes_per_cluster, bytes_per_record)
    }

    /// Open `$MFT` for its metadata only.
    ///
    /// `FILE_READ_ATTRIBUTES` alone is enough for this ioctl - the file's
    /// *contents* are never read through this handle, only its extent map, so
    /// no read access is requested.
    fn open_mft(drive_letter: char) -> Result<std::fs::File> {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;

        let path = format!(r"{drive_letter}:\$MFT");
        OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&path)
            .map_err(|source| Error::Device { path, source })
    }

    /// Pull extents out of one filled buffer. Returns how many were added.
    ///
    /// `run_start_vcn` carries across calls: the driver gives each extent's
    /// ending VCN, so an extent's start is wherever the previous one finished.
    fn parse_buffer(
        buf: &[u8],
        returned: usize,
        run_start_vcn: &mut u64,
        extents: &mut Vec<Extent>,
    ) -> usize {
        let usable = returned.min(buf.len());
        if usable < HEADER_LEN {
            return 0;
        }

        let count = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        // The header's StartingVcn is where this batch begins; trust it over
        // our own bookkeeping on the first extent of each call.
        *run_start_vcn = read_u64(buf, 8);

        let available = (usable - HEADER_LEN) / EXTENT_LEN;
        let count = count.min(available);

        for i in 0..count {
            let at = HEADER_LEN + i * EXTENT_LEN;
            let next_vcn = read_u64(buf, at);
            let lcn = read_u64(buf, at + 8);

            // LCN -1 marks a virtual (unallocated) range. The MFT should never
            // have one, but skipping keeps a bad map from becoming a bad read.
            if lcn == u64::MAX {
                *run_start_vcn = next_vcn;
                continue;
            }
            if next_vcn <= *run_start_vcn {
                continue; // empty or backwards; ignore
            }

            extents.push(Extent {
                vcn: *run_start_vcn,
                lcn,
                cluster_count: next_vcn - *run_start_vcn,
            });
            *run_start_vcn = next_vcn;
        }

        count
    }

    fn read_u64(buf: &[u8], at: usize) -> u64 {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&buf[at..at + 8]);
        u64::from_le_bytes(bytes)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Build a `RETRIEVAL_POINTERS_BUFFER` as the driver would.
        fn buffer(starting_vcn: u64, extents: &[(u64, u64)]) -> Vec<u8> {
            let mut buf = vec![0u8; HEADER_LEN + extents.len() * EXTENT_LEN];
            buf[0..4].copy_from_slice(&(extents.len() as u32).to_le_bytes());
            buf[8..16].copy_from_slice(&starting_vcn.to_le_bytes());
            for (i, &(next_vcn, lcn)) in extents.iter().enumerate() {
                let at = HEADER_LEN + i * EXTENT_LEN;
                buf[at..at + 8].copy_from_slice(&next_vcn.to_le_bytes());
                buf[at + 8..at + 16].copy_from_slice(&lcn.to_le_bytes());
            }
            buf
        }

        #[test]
        fn extent_starts_come_from_the_previous_extents_end() {
            let buf = buffer(0, &[(10, 1000), (25, 5000), (30, 9000)]);
            let mut start = 0;
            let mut extents = Vec::new();

            assert_eq!(parse_buffer(&buf, buf.len(), &mut start, &mut extents), 3);
            assert_eq!(
                extents,
                vec![
                    Extent {
                        vcn: 0,
                        lcn: 1000,
                        cluster_count: 10
                    },
                    Extent {
                        vcn: 10,
                        lcn: 5000,
                        cluster_count: 15
                    },
                    Extent {
                        vcn: 25,
                        lcn: 9000,
                        cluster_count: 5
                    },
                ]
            );
            assert_eq!(start, 30, "the next call resumes here");
        }

        #[test]
        fn a_second_call_continues_where_the_first_stopped() {
            // What ERROR_MORE_DATA looks like: two buffers, one map.
            let mut extents = Vec::new();
            let mut start = 0;

            let first = buffer(0, &[(10, 1000)]);
            parse_buffer(&first, first.len(), &mut start, &mut extents);
            assert_eq!(start, 10);

            let second = buffer(10, &[(20, 7000)]);
            parse_buffer(&second, second.len(), &mut start, &mut extents);

            assert_eq!(extents.len(), 2);
            assert_eq!(extents[1].vcn, 10, "no gap between the two calls");
            assert_eq!(extents[1].cluster_count, 10);
        }

        #[test]
        fn a_truncated_buffer_yields_only_whole_extents() {
            // The header claims three but only two fit in what was returned.
            let buf = buffer(0, &[(10, 1000), (20, 2000), (30, 3000)]);
            let mut start = 0;
            let mut extents = Vec::new();

            let short = HEADER_LEN + 2 * EXTENT_LEN;
            assert_eq!(parse_buffer(&buf, short, &mut start, &mut extents), 2);
            assert_eq!(extents.len(), 2);
        }

        #[test]
        fn virtual_and_backwards_extents_are_skipped() {
            let buf = buffer(0, &[(10, 1000), (20, u64::MAX), (30, 3000)]);
            let mut start = 0;
            let mut extents = Vec::new();
            parse_buffer(&buf, buf.len(), &mut start, &mut extents);

            assert_eq!(extents.len(), 2, "the LCN -1 range is not a real extent");
            assert_eq!(extents[1].vcn, 20, "but it still advances the VCN");
            assert_eq!(extents[1].lcn, 3000);
        }

        #[test]
        fn an_undersized_buffer_reports_no_progress() {
            let mut start = 0;
            let mut extents = Vec::new();
            assert_eq!(parse_buffer(&[0u8; 4], 4, &mut start, &mut extents), 0);
            assert!(extents.is_empty());
        }
    }
}
