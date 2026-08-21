//! What a scanned volume can and cannot tell us.
//!
//! Filesystems differ in ways the shared layers above `model/` must not
//! hardcode: ext4 is case-sensitive and NTFS is not, FAT32 has no stable file
//! ids, copy-on-write filesystems report sizes that do not sum. Rather than
//! branching on a filesystem enum, each scanner declares its capabilities and
//! the rest of the app reads them as data.
//!
//! See `architecture.md` sec 6.

/// Capabilities of one scanned volume, declared by the scanner that read it.
// Serialized into a scan snapshot's manifest (`service::cache`), which is why
// a capability that changes meaning needs a new field rather than a reused one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct VolumeCaps {
    /// Name comparison is case-sensitive. ext4 yes, NTFS no. Search uses this
    /// as its default; the user may override per query.
    pub case_sensitive: bool,

    /// Files have a stable, meaningful identity (MFT record number, inode
    /// number). False on FAT32, where directory entries carry no id - which
    /// disables open-by-id and incremental diffing.
    pub has_stable_ids: bool,

    /// One file can appear under several names/parents.
    pub has_hard_links: bool,

    /// "Size on disk" is real. When false, `allocated` mirrors `size` and the
    /// column is hidden rather than showing a fabricated number.
    pub has_allocated_size: bool,

    /// A change journal exists, so the index can be kept current without a
    /// rescan. NTFS's USN journal; no equivalent on ext4 or FAT.
    pub live_updates: bool,

    /// Sizes add up. False on copy-on-write filesystems, where snapshots and
    /// clones share blocks and two files can each truthfully report 1 GB while
    /// together occupying 1 GB. The UI must say so rather than presenting a
    /// confident wrong total.
    pub sizes_are_exact: bool,

    /// Separator used when assembling display paths.
    pub path_separator: char,

    /// What the root is called: `"C:"`, `"/"`, `"evidence.dd :: part2"`.
    pub root_label: String,
}

impl Default for VolumeCaps {
    /// A neutral baseline, useful in tests. Real scanners fill every field
    /// explicitly rather than relying on these values.
    fn default() -> Self {
        Self {
            case_sensitive: false,
            has_stable_ids: true,
            has_hard_links: false,
            has_allocated_size: true,
            live_updates: false,
            sizes_are_exact: true,
            path_separator: '\\',
            root_label: String::new(),
        }
    }
}
