//! The NTFS boot sector: the volume's geometry, in the first 512 bytes.
//!
//! Everything the scanner needs to find the MFT - cluster size, record size,
//! and where the MFT starts - is here. Windows will also report most of it
//! through `FSCTL_GET_NTFS_VOLUME_DATA`, but that requires the filesystem
//! driver to be mounted and cooperative. Reading the boot sector works on a
//! raw image and on a volume the driver refuses to talk about, so it is the
//! primary source and the ioctl is the fallback rather than the reverse.
//!
//! Pure parsing over a byte buffer; no I/O, testable anywhere.

use crate::error::{Error, Result};

/// Length of the boot sector, and the minimum buffer [`BootSector::parse`]
/// accepts.
pub const BOOT_SECTOR_LEN: usize = 512;

/// The eight bytes at offset 3 of every NTFS volume.
const NTFS_OEM_ID: &[u8; 8] = b"NTFS    ";

/// NTFS volume geometry, as recorded in the boot sector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootSector {
    /// Physical sector size. 512 on most drives, 4096 on native 4Kn.
    pub bytes_per_sector: u32,
    /// Allocation unit. Every file's on-disk size is a multiple of this.
    pub bytes_per_cluster: u32,
    /// Size of one MFT record. Effectively always 1024, but stated rather
    /// than assumed - v1 hardcoded it in places.
    pub bytes_per_record: u32,
    /// First cluster of the MFT.
    pub mft_start_lcn: u64,
    /// First cluster of the MFT mirror, a backup of the first few records.
    pub mft_mirror_lcn: u64,
    /// Total sectors in the volume.
    pub total_sectors: u64,
    /// Volume serial number.
    pub serial: u64,
}

impl BootSector {
    /// Parse the boot sector from the first [`BOOT_SECTOR_LEN`] bytes of a
    /// volume or partition.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < BOOT_SECTOR_LEN {
            return Err(Error::Parse {
                format: "NTFS boot sector".to_string(),
                message: format!("need {BOOT_SECTOR_LEN} bytes, got {}", buf.len()),
            });
        }

        if &buf[3..11] != NTFS_OEM_ID {
            return Err(Error::Parse {
                format: "NTFS boot sector".to_string(),
                message: format!(
                    "not NTFS: OEM id is {:?}",
                    String::from_utf8_lossy(&buf[3..11])
                ),
            });
        }

        let bytes_per_sector = u32::from(u16::from_le_bytes([buf[0x0B], buf[0x0C]]));
        if bytes_per_sector < 256 || !bytes_per_sector.is_power_of_two() {
            return Err(Error::Parse {
                format: "NTFS boot sector".to_string(),
                message: format!("implausible sector size {bytes_per_sector}"),
            });
        }

        let sectors_per_cluster = decode_cluster_factor(buf[0x0D]);
        let Some(bytes_per_cluster) = bytes_per_sector.checked_mul(sectors_per_cluster) else {
            return Err(Error::Parse {
                format: "NTFS boot sector".to_string(),
                message: format!(
                    "cluster size overflows: {bytes_per_sector} x {sectors_per_cluster}"
                ),
            });
        };
        if bytes_per_cluster == 0 {
            return Err(Error::Parse {
                format: "NTFS boot sector".to_string(),
                message: "zero cluster size".to_string(),
            });
        }

        let bytes_per_record = decode_record_size(buf[0x40], bytes_per_cluster)?;

        Ok(Self {
            bytes_per_sector,
            bytes_per_cluster,
            bytes_per_record,
            mft_start_lcn: read_u64(buf, 0x30),
            mft_mirror_lcn: read_u64(buf, 0x38),
            total_sectors: read_u64(buf, 0x28),
            serial: read_u64(buf, 0x48),
        })
    }

    /// Byte offset of the MFT from the start of the volume.
    pub fn mft_byte_offset(&self) -> u64 {
        self.mft_start_lcn * u64::from(self.bytes_per_cluster)
    }

    /// Volume size in bytes, as the boot sector reports it.
    pub fn volume_bytes(&self) -> u64 {
        self.total_sectors * u64::from(self.bytes_per_sector)
    }

    /// How many MFT records fit in one cluster. Below one when records are
    /// larger than clusters, which is why callers must not assume it.
    pub fn records_per_cluster(&self) -> f64 {
        f64::from(self.bytes_per_cluster) / f64::from(self.bytes_per_record)
    }
}

/// Sectors per cluster, which NTFS encodes two ways.
///
/// Values above 0x80 are a signed shift: `0xF4` (-12) means 2^12 sectors. Used
/// on volumes with clusters larger than 64 KB, which Windows 10 and later
/// support up to 2 MB.
fn decode_cluster_factor(raw: u8) -> u32 {
    if raw <= 0x80 {
        u32::from(raw)
    } else {
        1u32 << (256 - u32::from(raw))
    }
}

/// MFT record size, encoded the same two ways.
///
/// Positive: a count of clusters. Negative (as `i8`): a power of two in bytes,
/// so `0xF6` (-10) means 2^10 = 1024 - which is what essentially every volume
/// uses, because a 4 KB cluster would otherwise make records 4x larger than
/// they need to be.
fn decode_record_size(raw: u8, bytes_per_cluster: u32) -> Result<u32> {
    let size = if (raw as i8) < 0 {
        let shift = (raw as i8).unsigned_abs();
        if shift >= 32 {
            return Err(Error::Parse {
                format: "NTFS boot sector".to_string(),
                message: format!("implausible record-size shift {shift}"),
            });
        }
        1u32 << shift
    } else {
        match u32::from(raw).checked_mul(bytes_per_cluster) {
            Some(size) => size,
            None => {
                return Err(Error::Parse {
                    format: "NTFS boot sector".to_string(),
                    message: "record size overflows".to_string(),
                });
            }
        }
    };

    // A record holds a header plus attributes and is fixed up in sector-sized
    // chunks; anything outside this range is corrupt rather than exotic.
    if !(64..=65536).contains(&size) || !size.is_power_of_two() {
        return Err(Error::Parse {
            format: "NTFS boot sector".to_string(),
            message: format!("implausible MFT record size {size}"),
        });
    }
    Ok(size)
}

fn read_u64(buf: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[at..at + 8]);
    u64::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A boot sector for a typical volume: 512-byte sectors, 4 KB clusters,
    /// 1 KB records, MFT at cluster 786432.
    fn typical() -> [u8; BOOT_SECTOR_LEN] {
        let mut buf = [0u8; BOOT_SECTOR_LEN];
        buf[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
        buf[3..11].copy_from_slice(NTFS_OEM_ID);
        buf[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        buf[0x0D] = 8; // 8 x 512 = 4096-byte clusters
        buf[0x15] = 0xF8;
        buf[0x28..0x30].copy_from_slice(&976_773_167u64.to_le_bytes());
        buf[0x30..0x38].copy_from_slice(&786_432u64.to_le_bytes());
        buf[0x38..0x40].copy_from_slice(&2u64.to_le_bytes());
        buf[0x40] = 0xF6; // -10 -> 2^10 = 1024-byte records
        buf[0x48..0x50].copy_from_slice(&0x1234_5678_9ABC_DEF0u64.to_le_bytes());
        buf[510] = 0x55;
        buf[511] = 0xAA;
        buf
    }

    #[test]
    fn parses_a_typical_volume() {
        let boot = BootSector::parse(&typical()).unwrap();

        assert_eq!(boot.bytes_per_sector, 512);
        assert_eq!(boot.bytes_per_cluster, 4096);
        assert_eq!(boot.bytes_per_record, 1024);
        assert_eq!(boot.mft_start_lcn, 786_432);
        assert_eq!(boot.mft_mirror_lcn, 2);
        assert_eq!(boot.serial, 0x1234_5678_9ABC_DEF0);
        assert_eq!(boot.mft_byte_offset(), 786_432 * 4096);
        assert_eq!(boot.records_per_cluster(), 4.0);
    }

    #[test]
    fn accepts_a_4kn_drive() {
        let mut buf = typical();
        buf[0x0B..0x0D].copy_from_slice(&4096u16.to_le_bytes());
        buf[0x0D] = 1; // one 4096-byte sector per cluster

        let boot = BootSector::parse(&buf).unwrap();
        assert_eq!(boot.bytes_per_sector, 4096);
        assert_eq!(boot.bytes_per_cluster, 4096);
        assert_eq!(boot.bytes_per_record, 1024);
    }

    #[test]
    fn decodes_a_shifted_cluster_factor() {
        // 0xF4 as i8 is -12, meaning 2^12 = 4096 sectors per cluster: a 2 MB
        // cluster, which Windows 10 and later allow.
        let mut buf = typical();
        buf[0x0D] = 0xF4;

        let boot = BootSector::parse(&buf).unwrap();
        assert_eq!(boot.bytes_per_cluster, 4096 * 512);
    }

    #[test]
    fn decodes_a_record_size_given_in_clusters() {
        // Positive means clusters; 1 cluster of 4096 bytes.
        let mut buf = typical();
        buf[0x40] = 1;

        let boot = BootSector::parse(&buf).unwrap();
        assert_eq!(boot.bytes_per_record, 4096);
        assert_eq!(boot.records_per_cluster(), 1.0);
    }

    #[test]
    fn rejects_a_non_ntfs_volume() {
        let mut buf = typical();
        buf[3..11].copy_from_slice(b"MSDOS5.0");

        let err = BootSector::parse(&buf).unwrap_err();
        assert!(err.to_string().contains("not NTFS"), "{err}");
    }

    #[test]
    fn rejects_a_short_buffer() {
        assert!(BootSector::parse(&[0u8; 100]).is_err());
    }

    #[test]
    fn rejects_implausible_geometry() {
        let mut zero_sector = typical();
        zero_sector[0x0B..0x0D].copy_from_slice(&0u16.to_le_bytes());
        assert!(BootSector::parse(&zero_sector).is_err());

        let mut odd_sector = typical();
        odd_sector[0x0B..0x0D].copy_from_slice(&999u16.to_le_bytes());
        assert!(
            BootSector::parse(&odd_sector).is_err(),
            "not a power of two"
        );

        let mut zero_cluster = typical();
        zero_cluster[0x0D] = 0;
        assert!(BootSector::parse(&zero_cluster).is_err());

        let mut huge_record = typical();
        huge_record[0x40] = 0xE0; // -32 -> 2^32, nonsense
        assert!(BootSector::parse(&huge_record).is_err());
    }

    #[test]
    fn an_all_zero_sector_is_rejected_rather_than_parsed() {
        // What an unformatted or wrongly-offset read looks like. It must not
        // yield a BootSector with a zero cluster size that divides by zero
        // three layers up.
        assert!(BootSector::parse(&[0u8; BOOT_SECTOR_LEN]).is_err());
    }
}
