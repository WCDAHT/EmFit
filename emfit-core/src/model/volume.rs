//! What was found on this machine, before anything is read.
//!
//! Discovery answers questions the byte layer cannot: which volumes exist,
//! what filesystem each holds, and — for raw access — *where on which physical
//! disk* each one starts. A [`BlockSource`] can read `\\.\PhysicalDrive0`, but
//! only [`PartitionLocation`] says that `C:` begins 105 MB into it.
//!
//! These are plain descriptions. The code that produces them lives in
//! `service::volume`, because enumeration is an operation against the OS
//! rather than a data type.
//!
//! [`BlockSource`]: crate::parser::block::BlockSource

/// Which filesystem a volume holds, as reported by the OS.
///
/// Only [`FilesystemKind::Ntfs`] has a native scanner today. The rest are
/// recognized so the UI can say *why* a volume will be scanned the slow way,
/// rather than failing with something opaque.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesystemKind {
    Ntfs,
    Fat32,
    ExFat,
    ReFs,
    /// Recognized by name but with no scanner of its own — ext4 on a mounted
    /// image, a network redirector, anything unexpected.
    Other(String),
}

impl FilesystemKind {
    /// Classify the OS's filesystem name (`"NTFS"`, `"FAT32"`, …).
    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_uppercase().as_str() {
            "NTFS" => Self::Ntfs,
            "FAT32" => Self::Fat32,
            "EXFAT" => Self::ExFat,
            "REFS" => Self::ReFs,
            other => Self::Other(other.to_string()),
        }
    }

    /// The name to show a user.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Ntfs => "NTFS",
            Self::Fat32 => "FAT32",
            Self::ExFat => "exFAT",
            Self::ReFs => "ReFS",
            Self::Other(name) => name,
        }
    }

    /// Whether EmFit can read this filesystem's metadata directly, rather than
    /// walking directories through the OS.
    ///
    /// NTFS only, until the scanners in M7 land.
    pub fn has_native_scanner(&self) -> bool {
        matches!(self, Self::Ntfs)
    }
}

/// Where a volume physically sits, for raw access that bypasses the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartitionLocation {
    /// Index of the physical disk: `N` in `\\.\PhysicalDriveN`.
    pub disk_number: u32,
    /// Byte offset of the partition from the start of that disk.
    pub starting_offset: u64,
    /// Length of the partition in bytes.
    pub length: u64,
}

impl PartitionLocation {
    /// The device path of the disk this partition lives on.
    pub fn physical_drive_path(&self) -> String {
        format!(r"\\.\PhysicalDrive{}", self.disk_number)
    }
}

/// One volume found on this machine.
#[derive(Debug, Clone)]
pub struct VolumeInfo {
    /// Stable device path, `\\?\Volume{…}` form. Present even when the volume
    /// has no drive letter.
    pub guid_path: String,
    /// Every path the volume is reachable through: drive letters like `C:\`
    /// and directories it is mounted into. May be empty — a volume can exist
    /// with no mount point at all.
    pub mount_points: Vec<String>,
    /// User-assigned volume label, when set.
    pub label: Option<String>,
    pub filesystem: FilesystemKind,
    /// Volume serial number, as the OS reports it.
    pub serial: u32,
    pub bytes_per_sector: u32,
    pub bytes_per_cluster: u32,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// Physical position, when the volume maps to exactly one partition.
    ///
    /// `None` for spanned and striped volumes, which occupy several disk
    /// extents and so have no single starting offset. Raw access is
    /// unavailable for those; they fall back to the directory walker.
    pub location: Option<PartitionLocation>,
}

impl VolumeInfo {
    /// The drive letter, when the volume has one.
    pub fn drive_letter(&self) -> Option<char> {
        self.mount_points.iter().find_map(|m| {
            let mut chars = m.chars();
            let letter = chars.next()?;
            (letter.is_ascii_alphabetic() && chars.next() == Some(':'))
                .then(|| letter.to_ascii_uppercase())
        })
    }

    /// Device path for raw volume access: `\\.\C:` when there is a drive
    /// letter, otherwise the GUID path with its trailing separator removed
    /// (which `CreateFile` requires when opening a volume as a device).
    pub fn device_path(&self) -> String {
        match self.drive_letter() {
            Some(letter) => format!(r"\\.\{letter}:"),
            None => self.guid_path.trim_end_matches('\\').to_string(),
        }
    }

    /// Short name for the UI: `C:`, else the label, else the GUID.
    pub fn display_name(&self) -> String {
        if let Some(letter) = self.drive_letter() {
            return format!("{letter}:");
        }
        if let Some(label) = &self.label
            && !label.is_empty()
        {
            return label.clone();
        }
        self.guid_path.clone()
    }

    /// What paths from this volume are prefixed with, for
    /// [`VolumeCaps::root_label`].
    ///
    /// [`VolumeCaps::root_label`]: crate::model::caps::VolumeCaps::root_label
    pub fn root_label(&self) -> String {
        self.display_name()
    }

    /// Bytes in use.
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }

    /// Whether this volume can be read through the raw MFT path: a supported
    /// filesystem, sitting on exactly one partition.
    pub fn supports_raw_scan(&self) -> bool {
        self.filesystem.has_native_scanner() && self.location.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(mount_points: &[&str]) -> VolumeInfo {
        VolumeInfo {
            guid_path: r"\\?\Volume{11111111-2222-3333-4444-555555555555}\".to_string(),
            mount_points: mount_points.iter().map(|s| s.to_string()).collect(),
            label: None,
            filesystem: FilesystemKind::Ntfs,
            serial: 0,
            bytes_per_sector: 512,
            bytes_per_cluster: 4096,
            total_bytes: 0,
            free_bytes: 0,
            location: None,
        }
    }

    #[test]
    fn filesystem_names_are_classified_case_insensitively() {
        assert_eq!(FilesystemKind::from_name("NTFS"), FilesystemKind::Ntfs);
        assert_eq!(FilesystemKind::from_name("ntfs"), FilesystemKind::Ntfs);
        assert_eq!(FilesystemKind::from_name("exFAT"), FilesystemKind::ExFat);
        assert_eq!(
            FilesystemKind::from_name("ext4"),
            FilesystemKind::Other("EXT4".to_string())
        );
    }

    #[test]
    fn only_ntfs_has_a_native_scanner_today() {
        assert!(FilesystemKind::Ntfs.has_native_scanner());
        assert!(!FilesystemKind::Fat32.has_native_scanner());
        assert!(!FilesystemKind::ReFs.has_native_scanner());
    }

    #[test]
    fn drive_letter_comes_from_the_mount_points() {
        assert_eq!(volume(&[r"C:\"]).drive_letter(), Some('C'));
        assert_eq!(volume(&[r"d:\"]).drive_letter(), Some('D'));
        assert_eq!(volume(&[]).drive_letter(), None);
        // A volume mounted only into a directory has no letter.
        assert_eq!(volume(&[r"C:\mnt\data\"]).drive_letter(), Some('C'));
    }

    #[test]
    fn letterless_volumes_still_have_a_device_path() {
        let v = volume(&[]);
        assert_eq!(
            v.device_path(),
            r"\\?\Volume{11111111-2222-3333-4444-555555555555}",
            "the trailing separator must go — CreateFile rejects it"
        );
        assert_eq!(v.device_path(), v.guid_path.trim_end_matches('\\'));
    }

    #[test]
    fn lettered_volumes_use_the_short_device_path() {
        assert_eq!(volume(&[r"E:\"]).device_path(), r"\\.\E:");
        assert_eq!(volume(&[r"E:\"]).display_name(), "E:");
    }

    #[test]
    fn display_name_falls_back_to_label_then_guid() {
        let mut v = volume(&[]);
        assert!(v.display_name().starts_with(r"\\?\Volume"));

        v.label = Some("Evidence".to_string());
        assert_eq!(v.display_name(), "Evidence");
    }

    #[test]
    fn raw_scan_needs_both_a_supported_filesystem_and_one_partition() {
        let mut v = volume(&[r"C:\"]);
        assert!(!v.supports_raw_scan(), "no location yet");

        v.location = Some(PartitionLocation {
            disk_number: 0,
            starting_offset: 105_906_176,
            length: 500_000_000_000,
        });
        assert!(v.supports_raw_scan());

        // A spanned volume has no single offset, so raw access is off.
        v.location = None;
        assert!(!v.supports_raw_scan());

        // Nor does a filesystem without a scanner.
        v.location = Some(PartitionLocation {
            disk_number: 0,
            starting_offset: 0,
            length: 1,
        });
        v.filesystem = FilesystemKind::ExFat;
        assert!(!v.supports_raw_scan());
    }

    #[test]
    fn physical_drive_path_is_built_from_the_disk_number() {
        let loc = PartitionLocation {
            disk_number: 2,
            starting_offset: 0,
            length: 0,
        };
        assert_eq!(loc.physical_drive_path(), r"\\.\PhysicalDrive2");
    }
}
