//! Finding the volumes on this machine.
//!
//! Produces [`VolumeInfo`] for every volume Windows knows about, including
//! those with no drive letter and those mounted into a directory. The physical
//! location - which disk, which offset - comes from
//! `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`, and is what lets a scanner open
//! `\\.\PhysicalDriveN` and read past the filesystem driver entirely.
//!
//! # Platforms
//!
//! Windows-only. [`enumerate`] exists on every platform and returns
//! [`Error::UnsupportedPlatform`] elsewhere, so callers need no `cfg` of their
//! own.
//!
//! # `unsafe`
//!
//! The workspace denies `unsafe_code`; this module overrides it, because these
//! ioctls have no `std` equivalent. Every block is small, wraps exactly one
//! call, and is preceded by why it is sound. Nothing else in the engine needs
//! this - raw device *reads* go through `std`'s positional file APIs (see
//! [`crate::parser::block`]).

use crate::error::Result;
use crate::model::volume::VolumeInfo;

/// Every volume on this machine, in the order the OS reports them.
///
/// Volumes that cannot be interrogated - a card reader with no card, a
/// disconnected network drive - are skipped rather than failing the whole
/// enumeration, so one bad device does not hide the rest.
pub fn enumerate() -> Result<Vec<VolumeInfo>> {
    #[cfg(windows)]
    {
        windows_impl::enumerate()
    }
    #[cfg(not(windows))]
    {
        Err(crate::error::Error::UnsupportedPlatform {
            operation: "volume enumeration".to_string(),
        })
    }
}

/// The volumes EmFit can read the fast way: a supported filesystem on a single
/// partition. See [`VolumeInfo::supports_raw_scan`].
pub fn enumerate_scannable() -> Result<Vec<VolumeInfo>> {
    Ok(enumerate()?
        .into_iter()
        .filter(VolumeInfo::supports_raw_scan)
        .collect())
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "volume enumeration and IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS have \
              no safe std equivalent; each block wraps one call and documents \
              its preconditions"
)]
mod windows_impl {
    use windows::Win32::Foundation::{ERROR_NO_MORE_FILES, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetDiskFreeSpaceExW, GetDiskFreeSpaceW,
        GetVolumeInformationW, GetVolumePathNamesForVolumeNameW,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::core::PCWSTR;

    use std::os::windows::io::AsRawHandle;

    use crate::error::{Error, Result};
    use crate::model::volume::{FilesystemKind, PartitionLocation, VolumeInfo};

    /// `\\?\Volume{...}\` is 49 characters; the documented buffer size is 50.
    const VOLUME_NAME_LEN: usize = 64;

    /// `CTL_CODE(IOCTL_VOLUME_BASE, 0, METHOD_BUFFERED, FILE_ANY_ACCESS)`,
    /// where `IOCTL_VOLUME_BASE` is `'V'` - so `(0x56 << 16) | 0`.
    ///
    /// Spelled out because `windows-rs` does not generate the volume ioctl
    /// codes; they are macro expansions in the SDK headers, not constants.
    const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;

    pub(super) fn enumerate() -> Result<Vec<VolumeInfo>> {
        let mut volumes = Vec::new();
        let mut name = [0u16; VOLUME_NAME_LEN];

        // SAFETY: `name` is a live, correctly sized buffer for the duration of
        // the call. The returned handle is closed on every exit path below.
        let find = unsafe { FindFirstVolumeW(&mut name) }.map_err(|e| Error::WindowsApi {
            api: "FindFirstVolumeW".to_string(),
            source: std::io::Error::from_raw_os_error(e.code().0),
        })?;

        loop {
            if let Some(volume) = describe(&wide_to_string(&name)) {
                volumes.push(volume);
            }

            name = [0u16; VOLUME_NAME_LEN];
            // SAFETY: `find` is the live handle from FindFirstVolumeW, not yet
            // closed; `name` is a live buffer of the documented size.
            let more = unsafe { FindNextVolumeW(find, &mut name) };
            if let Err(e) = more {
                // Exhausting the list is the normal exit, not a failure.
                if e.code().0 as u32 & 0xFFFF != ERROR_NO_MORE_FILES.0 {
                    tracing::debug!(error = %e, "FindNextVolumeW ended early");
                }
                break;
            }
        }

        // SAFETY: `find` came from FindFirstVolumeW and has not been closed.
        let _ = unsafe { FindVolumeClose(find) };

        tracing::info!(count = volumes.len(), "enumerated volumes");
        Ok(volumes)
    }

    /// Gather everything about one volume. Returns `None` when the volume
    /// cannot be interrogated at all - an empty card reader, a disconnected
    /// network drive - so one dead device does not hide the others.
    fn describe(guid_path: &str) -> Option<VolumeInfo> {
        let wide = to_wide(guid_path);
        let root = PCWSTR(wide.as_ptr());

        let mut label_buf = [0u16; 256];
        let mut fs_buf = [0u16; 64];
        let mut serial = 0u32;

        // SAFETY: all four buffers outlive the call; `root` points at a
        // NUL-terminated string held in `wide`.
        let info = unsafe {
            GetVolumeInformationW(
                root,
                Some(&mut label_buf),
                Some(&mut serial),
                None,
                None,
                Some(&mut fs_buf),
            )
        };
        if let Err(e) = info {
            tracing::debug!(volume = guid_path, error = %e, "skipping unreadable volume");
            return None;
        }

        let mut sectors_per_cluster = 0u32;
        let mut bytes_per_sector = 0u32;
        // SAFETY: both out-params are live locals; `root` as above.
        let geometry = unsafe {
            GetDiskFreeSpaceW(
                root,
                Some(&mut sectors_per_cluster),
                Some(&mut bytes_per_sector),
                None,
                None,
            )
        };
        if geometry.is_err() {
            // Rare but survivable: sizes are still useful without geometry.
            tracing::debug!(volume = guid_path, "no cluster geometry");
        }

        let mut total = 0u64;
        let mut free = 0u64;
        // SAFETY: both out-params are live locals; `root` as above.
        let _ = unsafe { GetDiskFreeSpaceExW(root, None, Some(&mut total), Some(&mut free)) };

        let label = wide_to_string(&label_buf);
        let filesystem = FilesystemKind::from_name(&wide_to_string(&fs_buf));

        Some(VolumeInfo {
            mount_points: mount_points(root),
            label: (!label.is_empty()).then_some(label),
            filesystem,
            serial,
            bytes_per_sector,
            bytes_per_cluster: bytes_per_sector.saturating_mul(sectors_per_cluster),
            total_bytes: total,
            free_bytes: free,
            location: partition_location(guid_path),
            guid_path: guid_path.to_string(),
        })
    }

    /// Every path this volume is reachable through - drive letters and
    /// directories it is mounted into.
    fn mount_points(root: PCWSTR) -> Vec<String> {
        let mut buf = vec![0u16; 512];
        let mut needed = 0u32;

        // SAFETY: `buf` is live and its length is passed honestly; `needed` is
        // a live local.
        let ok = unsafe { GetVolumePathNamesForVolumeNameW(root, Some(&mut buf), &mut needed) };

        if ok.is_err() {
            // Grow once to the size the API asked for, then give up.
            if needed as usize > buf.len() {
                buf = vec![0u16; needed as usize];
                // SAFETY: as above, with the larger buffer.
                let retry =
                    unsafe { GetVolumePathNamesForVolumeNameW(root, Some(&mut buf), &mut needed) };
                if retry.is_err() {
                    return Vec::new();
                }
            } else {
                return Vec::new();
            }
        }

        // A NUL-separated list terminated by a second NUL.
        buf.split(|&c| c == 0)
            .take_while(|part| !part.is_empty())
            .map(String::from_utf16_lossy)
            .collect()
    }

    /// Which physical disk this volume sits on, and where.
    ///
    /// Returns `None` for spanned and striped volumes: those occupy several
    /// disk extents and have no single starting offset, so raw access cannot
    /// address them as one partition.
    fn partition_location(guid_path: &str) -> Option<PartitionLocation> {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;

        // `CreateFile` rejects a volume path with its trailing separator.
        let device = guid_path.trim_end_matches('\\');

        // Zero desired access is enough for this ioctl, and - unlike
        // GENERIC_READ - needs no Administrator rights. So partition offsets
        // are discoverable before the user elevates.
        let handle = OpenOptions::new()
            .access_mode(0)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(device)
            .ok()?;

        // VOLUME_DISK_EXTENTS: u32 count, 4 bytes padding, then 24-byte
        // DISK_EXTENTs. Room for eight, which is ample for anything that could
        // still qualify as a single partition.
        let mut buf = [0u8; 8 + 24 * 8];
        let mut returned = 0u32;

        // SAFETY: the handle is open and owned by `handle` for the whole call;
        // the output buffer is a live local and its true size is passed.
        let ok = unsafe {
            DeviceIoControl(
                HANDLE(handle.as_raw_handle()),
                IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
                None,
                0,
                Some(buf.as_mut_ptr().cast()),
                buf.len() as u32,
                Some(&mut returned),
                None,
            )
        };
        if ok.is_err() {
            tracing::debug!(
                volume = guid_path,
                "no disk extents; not a simple partition"
            );
            return None;
        }

        // Parsed from bytes rather than transmuted through the C struct: same
        // result, no layout assumptions, no second unsafe block.
        let count = u32::from_le_bytes(buf[0..4].try_into().ok()?);
        if count != 1 {
            tracing::debug!(
                volume = guid_path,
                extents = count,
                "spanned or striped volume; no single partition offset"
            );
            return None;
        }

        Some(PartitionLocation {
            disk_number: u32::from_le_bytes(buf[8..12].try_into().ok()?),
            starting_offset: u64::from_le_bytes(buf[16..24].try_into().ok()?),
            length: u64::from_le_bytes(buf[24..32].try_into().ok()?),
        })
    }

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Decode a NUL-terminated wide buffer, ignoring everything after the NUL.
    fn wide_to_string(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn wide_strings_stop_at_the_nul() {
            let buf: Vec<u16> = "C:\\\0garbage".encode_utf16().collect();
            assert_eq!(wide_to_string(&buf), "C:\\");
        }

        #[test]
        fn wide_round_trip() {
            let s = r"\\?\Volume{1234}\";
            assert_eq!(wide_to_string(&to_wide(s)), s);
        }

        #[test]
        fn empty_buffer_is_an_empty_string() {
            assert_eq!(wide_to_string(&[0u16; 32]), "");
        }
    }
}
