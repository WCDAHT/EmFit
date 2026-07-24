//! The NTFS scanner: reads the Master File Table directly.
//!
//! NTFS keeps one record per file in a single table, and each record names its
//! own parent. Sweeping that table start to finish yields the whole tree with
//! no directory traversal at all — which is why a volume that takes minutes to
//! walk takes seconds to read. `architecture.md` §1.
//!
//! # Layout
//!
//! - [`boot`] — volume geometry from the first 512 bytes
//! - [`runs`] — decoding the `(length, offset)` lists NTFS stores extents in
//! - [`extents`] — where the MFT is, and how much can be read at once
//! - [`retrieval`] — asking the filesystem driver for the same map, faster
//!
//! # Finding the MFT
//!
//! Two routes to the same answer, and the fallback is not exotic — it is what
//! runs on a disk image, on an unmounted volume, and whenever `$MFT` cannot be
//! opened:
//!
//! 1. **Ask the driver.** [`retrieval::mft_extents`] issues
//!    `FSCTL_GET_RETRIEVAL_POINTERS`. One ioctl, no parsing.
//! 2. **Read record 0.** The MFT's first record describes the MFT itself; its
//!    `$DATA` run list is the extent map. Bootstrapping needs the boot
//!    sector's `mft_start_lcn` to find record 0 before the map is known, which
//!    is what [`extents::MftExtents::contiguous`] is for.
//!
//! Route 2 has a wrinkle worth knowing before it bites: on a badly fragmented
//! volume the MFT's own `$DATA` outgrows record 0 and spills into extension
//! records, so the full map is the concatenation of several records' run
//! lists. The bootstrap reads the first several records in one go for exactly
//! this reason.

pub mod attr;
pub mod boot;
pub mod bootstrap;
pub mod extents;
pub mod record;
pub mod retrieval;
pub mod runs;

pub use attr::{Attribute, AttributeType, FileName, Namespace, NonResident, StandardInfo};
pub use boot::BootSector;
pub use bootstrap::{MftLayout, probe, read_boot_sector};
pub use extents::{Extent, MftExtents, RecordLocation};
pub use record::{Record, RecordHeader};
pub use runs::DataRun;
