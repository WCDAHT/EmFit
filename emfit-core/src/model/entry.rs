//! What a scanner hands to the index builder, one file or directory at a time.
//!
//! `RawEntry` is the seam's currency: every filesystem, however it stores its
//! metadata, can produce this. Names are **borrowed** from the scanner's read
//! buffer — the sink copies what it needs before returning, which is what keeps
//! the hot path allocation-free. See `architecture.md` §5.

use std::ops::BitOr;

/// One file or directory, as produced by a scanner.
///
/// `fs_id` and `parent_id` are the *filesystem's* identifiers (MFT record
/// number, inode number, or a counter the scanner mints when the filesystem
/// has no stable id). The builder translates them to [`NodeId`]s after the
/// scan; entries may arrive in any order, and a parent may arrive after its
/// children.
///
/// [`NodeId`]: crate::model::index::NodeId
#[derive(Debug, Clone, Copy)]
pub struct RawEntry<'a> {
    /// This entry's filesystem identifier.
    pub fs_id: u64,
    /// The containing directory's filesystem identifier. Equal to `fs_id` for
    /// the volume root, which is how the builder recognizes it.
    pub parent_id: u64,
    /// File name only, not a path. Borrowed from the scanner's buffer.
    pub name: &'a str,
    /// Logical size in bytes. Zero for directories.
    pub size: u64,
    /// Bytes actually occupied on disk. Equal to `size` when the filesystem
    /// cannot report it (see [`VolumeCaps::has_allocated_size`]).
    ///
    /// [`VolumeCaps::has_allocated_size`]: crate::model::caps::VolumeCaps::has_allocated_size
    pub allocated: u64,
    /// Timestamps, already normalized by the scanner.
    pub times: Times,
    /// Attribute bits.
    pub flags: EntryFlags,
}

/// Timestamps in one representation, converted at the seam.
///
/// NTFS counts 100 ns ticks from 1601, ext4 counts seconds from 1970, FAT32
/// stores DOS time at two-second resolution. Scanners convert; nothing above
/// `model/` ever sees a native timestamp. Zero means "unknown".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Times {
    /// Last modification, nanoseconds since the Unix epoch.
    pub mtime: i64,
    /// Creation, nanoseconds since the Unix epoch.
    pub crtime: i64,
}

/// Attribute bits carried straight through from scanner to node.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct EntryFlags(u16);

impl EntryFlags {
    /// This entry is a directory.
    pub const DIRECTORY: Self = Self(1 << 0);
    /// Hidden from ordinary listings.
    pub const HIDDEN: Self = Self(1 << 1);
    /// Owned by the operating system.
    pub const SYSTEM: Self = Self(1 << 2);
    /// A reparse point, junction, or symbolic link. Never followed — doing so
    /// would count the target's bytes twice.
    pub const REPARSE: Self = Self(1 << 3);
    /// Sparse: allocated size is meaningfully below logical size.
    pub const SPARSE: Self = Self(1 << 4);
    /// Stored compressed.
    pub const COMPRESSED: Self = Self(1 << 5);
    /// **Not present on the filesystem.** The builder invents a small number
    /// of nodes — a root when the scan produced none, and the folder that
    /// collects unrooted entries. Flagged so the UI can render them
    /// distinctly and exports can disclose them rather than passing them off
    /// as real files.
    pub const SYNTHETIC: Self = Self(1 << 6);
    /// An additional name for a file counted elsewhere.
    ///
    /// A hard-linked file appears once per name, and each of those entries is
    /// a real path a user can open. But the bytes exist once, so exactly one
    /// entry carries the allocated size and the rest carry zero and this flag.
    ///
    /// Both halves matter: without the extra entries the file is missing from
    /// most of its locations, and without the zeroing a WinSxS-heavy volume
    /// reports several times the disk it actually uses. WizTree draws the same
    /// distinction by showing the non-owning links' size in parentheses.
    pub const ALIAS: Self = Self(1 << 7);

    /// No bits set.
    #[inline]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// True when every bit in `other` is set here.
    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Set union.
    #[inline]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Raw bits, for storage in a [`Node`].
    ///
    /// [`Node`]: crate::model::index::Node
    #[inline]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Rebuild from raw bits.
    #[inline]
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    /// Convenience for the check made on nearly every node.
    #[inline]
    pub const fn is_directory(self) -> bool {
        self.contains(Self::DIRECTORY)
    }

    /// True for nodes the builder invented. See [`EntryFlags::SYNTHETIC`].
    #[inline]
    pub const fn is_synthetic(self) -> bool {
        self.contains(Self::SYNTHETIC)
    }

    /// True for an extra name whose bytes are accounted under another entry.
    /// See [`EntryFlags::ALIAS`].
    #[inline]
    pub const fn is_alias(self) -> bool {
        self.contains(Self::ALIAS)
    }
}

impl BitOr for EntryFlags {
    type Output = Self;

    #[inline]
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_compose_and_test() {
        let f = EntryFlags::DIRECTORY | EntryFlags::HIDDEN;
        assert!(f.is_directory());
        assert!(f.contains(EntryFlags::HIDDEN));
        assert!(!f.contains(EntryFlags::SYSTEM));
        assert!(!f.is_synthetic());
    }

    #[test]
    fn flags_round_trip_through_bits() {
        let f = EntryFlags::SPARSE | EntryFlags::COMPRESSED | EntryFlags::SYNTHETIC;
        assert_eq!(EntryFlags::from_bits(f.bits()), f);
    }

    #[test]
    fn empty_contains_nothing() {
        let f = EntryFlags::empty();
        assert!(!f.is_directory());
        assert_eq!(f.bits(), 0);
    }
}
