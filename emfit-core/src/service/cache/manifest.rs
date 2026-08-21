//! What a snapshot claims about itself.
//!
//! JSON rather than a packed struct, and stored near the front of the file, so
//! that `cache list` and a human with a text editor can both read it without
//! decoding a single column - and so a file that is not a snapshot at all
//! fails to parse instead of being misread (`caching.md` sec 3.1).

use serde::{Deserialize, Serialize};

use crate::model::caps::VolumeCaps;
use crate::model::volume::VolumeInfo;

/// Bump when a field the loader relies on changes meaning. Adding an optional
/// field does not need it (STANDARDS sec 4.5).
pub const SCHEMA_VERSION: u32 = 1;

/// Everything except the columns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    /// Which build wrote it. Diagnostic only - a snapshot is not rejected for
    /// coming from another version, its sections are.
    pub app_version: String,
    pub volume: VolumeStamp,
    pub caps: VolumeCaps,
    pub scan: ScanStamp,
    /// Absent for a volume with no journal, and for disk images.
    pub journal: Option<JournalStamp>,
}

/// Which volume this is, in terms that survive a drive letter changing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeStamp {
    /// Hash of the identifying fields below. Also the file name.
    pub fingerprint: String,
    /// Filesystem serial number.
    pub serial: u32,
    /// `C:`, or an image file name.
    pub root_label: String,
    pub total_bytes: u64,
    pub bytes_per_cluster: u32,
    /// Free bytes when the scan ran. Refreshed on load - it changes by the
    /// minute and is not worth replaying.
    pub free_bytes: u64,
}

impl VolumeStamp {
    pub fn of(volume: &VolumeInfo) -> Self {
        let mut stamp = Self {
            fingerprint: String::new(),
            serial: volume.serial,
            root_label: volume.root_label(),
            total_bytes: volume.total_bytes,
            bytes_per_cluster: volume.bytes_per_cluster,
            free_bytes: volume.free_bytes,
        };
        stamp.fingerprint = stamp.compute_fingerprint();
        stamp
    }

    /// The identity a cache file is filed under.
    ///
    /// Deliberately not the drive letter: letters are reassigned, and a cache
    /// keyed to one would attach itself to whatever volume inherited it. Free
    /// space is excluded - it changes constantly and identifies nothing.
    fn compute_fingerprint(&self) -> String {
        let mut bytes = Vec::with_capacity(32);
        bytes.extend_from_slice(&self.serial.to_le_bytes());
        bytes.extend_from_slice(&self.total_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.bytes_per_cluster.to_le_bytes());
        format!("{:016x}", twox_hash::XxHash64::oneshot(0, &bytes))
    }

    /// Whether a snapshot filed under this stamp describes `other`.
    pub fn matches(&self, other: &Self) -> bool {
        self.serial == other.serial
            && self.total_bytes == other.total_bytes
            && self.bytes_per_cluster == other.bytes_per_cluster
    }
}

/// When the scan happened and how big it turned out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanStamp {
    /// Readable form, for the manifest's human readers.
    pub taken_at: String,
    /// Seconds since the Unix epoch, for the arithmetic.
    pub taken_at_unix: i64,
    pub duration_ms: u64,
    /// `physical`, `volume`, or `image`.
    pub access_mode: String,
    pub nodes: u64,
    pub arena_bytes: u64,
}

impl ScanStamp {
    pub fn now(duration_ms: u64, access_mode: &str, nodes: u64, arena_bytes: u64) -> Self {
        let at = chrono::Utc::now();
        Self {
            taken_at: at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            taken_at_unix: at.timestamp(),
            duration_ms,
            access_mode: access_mode.to_string(),
            nodes,
            arena_bytes,
        }
    }

    /// How long ago the scan ran. Zero if the clock has moved backwards.
    pub fn age(&self) -> std::time::Duration {
        let now = chrono::Utc::now().timestamp();
        std::time::Duration::from_secs((now - self.taken_at_unix).max(0) as u64)
    }
}

/// Where the volume's change journal stood when the scan **started**.
///
/// Started, not finished: everything that changed during the scan is then
/// replayed rather than lost, which costs nothing because replay re-reads the
/// records it is told about (`caching.md` sec 6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalStamp {
    pub id: u64,
    pub next_usn: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(serial: u32, total: u64) -> VolumeStamp {
        let mut s = VolumeStamp {
            fingerprint: String::new(),
            serial,
            root_label: "C:".to_string(),
            total_bytes: total,
            bytes_per_cluster: 4096,
            free_bytes: 1,
        };
        s.fingerprint = s.compute_fingerprint();
        s
    }

    #[test]
    fn the_fingerprint_ignores_what_changes_and_notices_what_does_not() {
        let a = stamp(7, 1 << 40);
        let mut b = stamp(7, 1 << 40);
        b.free_bytes = 999;
        b.root_label = "D:".to_string();
        assert_eq!(a.fingerprint, b.fingerprint, "a letter is not an identity");
        assert!(a.matches(&b));

        // A reformat changes the serial; a resize changes the size.
        assert_ne!(a.fingerprint, stamp(8, 1 << 40).fingerprint);
        assert_ne!(a.fingerprint, stamp(7, 1 << 41).fingerprint);
        assert!(!a.matches(&stamp(8, 1 << 40)));
    }

    #[test]
    fn a_manifest_survives_a_json_round_trip() {
        let manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            app_version: "0.1.0".to_string(),
            volume: stamp(7, 1 << 40),
            caps: crate::service::scan::ntfs_caps("C:".to_string()),
            scan: ScanStamp::now(1234, "physical", 42, 100),
            journal: Some(JournalStamp {
                id: 0xABCD,
                next_usn: 999,
            }),
        };
        let text = serde_json::to_string(&manifest).expect("serialize");
        let back: Manifest = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back.volume, manifest.volume);
        assert_eq!(back.journal, manifest.journal);
        assert_eq!(back.caps.root_label, "C:");
    }
}
