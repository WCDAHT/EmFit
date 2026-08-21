//! Volume discovery against the machine the tests run on.
//!
//! Necessarily environment-dependent, so the assertions are about invariants
//! that hold on any Windows host - well-formed paths, self-consistent flags -
//! rather than about a particular drive layout. The pure logic (path parsing,
//! filesystem classification) is unit-tested in `model::volume`.

use emfit_core::service::volume;

#[cfg(windows)]
mod windows {
    use super::*;
    use emfit_core::model::volume::VolumeInfo;

    fn volumes() -> Vec<VolumeInfo> {
        volume::enumerate().expect("enumeration must succeed on Windows")
    }

    #[test]
    fn finds_at_least_one_volume() {
        let found = volumes();
        assert!(
            !found.is_empty(),
            "a Windows machine always has at least a system volume"
        );
    }

    #[test]
    fn every_volume_has_a_well_formed_guid_path() {
        for v in volumes() {
            assert!(
                v.guid_path.starts_with(r"\\?\Volume{"),
                "unexpected guid path: {}",
                v.guid_path
            );
            assert!(
                v.guid_path.ends_with('\\'),
                "the API returns a trailing separator: {}",
                v.guid_path
            );
        }
    }

    #[test]
    fn device_paths_are_openable_in_form() {
        // `CreateFile` rejects a volume path that keeps its trailing
        // separator, so `device_path` must never produce one.
        for v in volumes() {
            let path = v.device_path();
            assert!(
                !path.ends_with('\\'),
                "device path must not end in a separator: {path}"
            );
            assert!(
                path.starts_with(r"\\.\") || path.starts_with(r"\\?\"),
                "device path must be a device namespace path: {path}"
            );
        }
    }

    #[test]
    fn a_drive_letter_implies_a_short_device_path() {
        for v in volumes() {
            match v.drive_letter() {
                Some(letter) => {
                    assert_eq!(v.device_path(), format!(r"\\.\{letter}:"));
                    assert_eq!(v.display_name(), format!("{letter}:"));
                }
                // Letterless volumes are normal - recovery and EFI partitions
                // are the common case, and are exactly what a drive-letter
                // enumeration would miss.
                None => assert!(v.device_path().starts_with(r"\\?\Volume{")),
            }
        }
    }

    #[test]
    fn geometry_is_self_consistent() {
        for v in volumes() {
            if v.bytes_per_sector == 0 {
                continue; // geometry unavailable for this device
            }
            assert!(
                v.bytes_per_sector.is_power_of_two(),
                "{}: sector size {} is not a power of two",
                v.display_name(),
                v.bytes_per_sector
            );
            assert!(
                v.bytes_per_cluster.is_multiple_of(v.bytes_per_sector),
                "{}: cluster {} is not a whole number of {}-byte sectors",
                v.display_name(),
                v.bytes_per_cluster,
                v.bytes_per_sector
            );
            assert!(v.free_bytes <= v.total_bytes, "{}", v.display_name());
            assert_eq!(
                v.used_bytes(),
                v.total_bytes - v.free_bytes,
                "{}",
                v.display_name()
            );
        }
    }

    #[test]
    fn partition_locations_are_plausible() {
        for v in volumes() {
            let Some(loc) = v.location else {
                continue; // spanned, striped, or otherwise not one partition
            };
            assert!(loc.length > 0, "{}: empty partition", v.display_name());
            assert!(
                loc.length >= v.total_bytes,
                "{}: partition ({}) smaller than the filesystem in it ({})",
                v.display_name(),
                loc.length,
                v.total_bytes
            );
            assert_eq!(
                loc.physical_drive_path(),
                format!(r"\\.\PhysicalDrive{}", loc.disk_number)
            );
        }
    }

    #[test]
    fn scannable_volumes_are_ntfs_on_a_single_partition() {
        let scannable = volume::enumerate_scannable().expect("enumeration must succeed");

        for v in &scannable {
            assert!(
                v.filesystem.has_native_scanner(),
                "{} has no native scanner but was listed as scannable",
                v.display_name()
            );
            assert!(v.location.is_some(), "{}", v.display_name());
        }

        // The scannable set is exactly the subset that says it is.
        let expected = volumes().iter().filter(|v| v.supports_raw_scan()).count();
        assert_eq!(scannable.len(), expected);
    }

    #[test]
    fn discovery_needs_no_elevation() {
        // Partition offsets come from an ioctl that works with zero desired
        // access, so the drive list is complete before the user elevates. If
        // this ever regresses, an unelevated run silently loses every
        // location and nothing looks scannable.
        if emfit_core::service::elevation::is_elevated() {
            return; // can't prove the unelevated path from an elevated run
        }
        let located = volumes().iter().filter(|v| v.location.is_some()).count();
        assert!(
            located > 0,
            "no partition locations resolved without elevation"
        );
    }
}

#[cfg(not(windows))]
mod other_platforms {
    use super::*;
    use emfit_core::error::Error;

    #[test]
    fn enumeration_is_unsupported_but_callable() {
        // Callers get an error they can report, not a compile error - no
        // `cfg` needed at the call site.
        assert!(matches!(
            volume::enumerate(),
            Err(Error::UnsupportedPlatform { .. })
        ));
        assert!(matches!(
            volume::enumerate_scannable(),
            Err(Error::UnsupportedPlatform { .. })
        ));
    }
}
