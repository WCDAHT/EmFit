//! `$MFT`'s allocation bitmap: one bit per record, set when the record is in
//! use.
//!
//! Reading it costs a single ~130 KB I/O and answers a question the sweep
//! otherwise cannot answer until it finishes: **how many files are actually
//! here**. `$MFT`'s data size only says how many record slots have ever been
//! written — the table never shrinks, so on a volume that has had many files
//! deleted it overstates the count, sometimes badly.
//!
//! Knowing the real figure up front buys two things:
//!
//! - [`IndexBuilder::reserve`] can size its arrays once instead of growing a
//!   several-hundred-megabyte `Vec` by repeated doubling and copying.
//! - Progress can be reported against the true total rather than against
//!   capacity, so the bar does not stall at 70% and then finish.
//!
//! [`IndexBuilder::reserve`]: crate::model::builder::IndexBuilder::reserve

use crate::error::{Error, Result};
use crate::parser::block::{AlignedBuf, BlockSource};
use crate::parser::ntfs::runs::DataRun;

/// Refuse to allocate more than this for one non-resident attribute. The MFT
/// bitmap of a 100-million-file volume is 12 MB; anything past this is a
/// corrupt run list rather than a real attribute.
const MAX_ATTRIBUTE_BYTES: u64 = 256 * 1024 * 1024;

/// Which MFT records are in use.
#[derive(Debug, Clone)]
pub struct MftBitmap {
    bits: Vec<u8>,
}

impl MftBitmap {
    pub fn from_bytes(bits: Vec<u8>) -> Self {
        Self { bits }
    }

    /// Whether the record is allocated to a file.
    ///
    /// Records past the end of the bitmap read as free — a bitmap shorter than
    /// the table means the tail has never been allocated.
    pub fn is_in_use(&self, record_number: u64) -> bool {
        let byte = (record_number / 8) as usize;
        let bit = (record_number % 8) as u8;
        self.bits.get(byte).is_some_and(|b| b & (1 << bit) != 0)
    }

    /// How many records are allocated. One pass over ~130 KB.
    pub fn count_in_use(&self) -> u64 {
        self.bits.iter().map(|b| u64::from(b.count_ones())).sum()
    }

    /// How many records the bitmap covers.
    pub fn capacity(&self) -> u64 {
        self.bits.len() as u64 * 8
    }

    pub fn is_empty(&self) -> bool {
        self.bits.is_empty()
    }
}

/// Read a non-resident attribute's contents by following its data runs.
///
/// Reads each run whole — runs are cluster-aligned, and clusters are a
/// multiple of the sector size, so every read stays aligned for an unbuffered
/// source. Sparse runs contribute zeroes without any I/O.
pub fn read_non_resident(
    source: &dyn BlockSource,
    runs: &[DataRun],
    bytes_per_cluster: u32,
    data_size: u64,
) -> Result<Vec<u8>> {
    if data_size > MAX_ATTRIBUTE_BYTES {
        return Err(Error::Parse {
            format: "non-resident attribute".to_string(),
            message: format!("claims {data_size} bytes; refusing to allocate that much"),
        });
    }

    let cluster = u64::from(bytes_per_cluster);
    let total_clusters: u64 = runs.iter().map(|r| r.cluster_count).sum();
    let total_bytes = total_clusters * cluster;

    if total_bytes < data_size {
        return Err(Error::Parse {
            format: "non-resident attribute".to_string(),
            message: format!(
                "run list covers {total_bytes} bytes but the attribute claims {data_size}"
            ),
        });
    }
    if total_bytes > MAX_ATTRIBUTE_BYTES {
        return Err(Error::Parse {
            format: "non-resident attribute".to_string(),
            message: format!("run list covers {total_bytes} bytes; refusing to allocate that much"),
        });
    }

    let mut buf = AlignedBuf::for_source(source, total_bytes as usize);
    let mut written = 0usize;

    for run in runs {
        let len = (run.cluster_count * cluster) as usize;
        let end = written + len;

        match run.lcn {
            // A hole: the buffer is already zeroed, so just skip past it.
            None => {}
            Some(lcn) => {
                let at = lcn * cluster;
                source.read_exact_at(at, &mut buf.as_mut_slice()[written..end])?;
            }
        }
        written = end;
    }

    let mut out = buf.as_slice()[..data_size as usize].to_vec();
    out.shrink_to_fit();
    Ok(out)
}

/// Read `$MFT`'s allocation bitmap.
pub fn read(
    source: &dyn BlockSource,
    runs: &[DataRun],
    bytes_per_cluster: u32,
    data_size: u64,
) -> Result<MftBitmap> {
    let bits = read_non_resident(source, runs, bytes_per_cluster, data_size)?;
    Ok(MftBitmap::from_bytes(bits))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_map_to_records_least_significant_first() {
        // 0b0000_0101 → records 0 and 2; 0b1000_0000 → record 15.
        let bitmap = MftBitmap::from_bytes(vec![0b0000_0101, 0b1000_0000]);

        assert!(bitmap.is_in_use(0));
        assert!(!bitmap.is_in_use(1));
        assert!(bitmap.is_in_use(2));
        assert!(!bitmap.is_in_use(7));
        assert!(!bitmap.is_in_use(8));
        assert!(bitmap.is_in_use(15));
    }

    #[test]
    fn records_past_the_bitmap_are_free() {
        let bitmap = MftBitmap::from_bytes(vec![0xFF]);
        assert!(bitmap.is_in_use(7));
        assert!(!bitmap.is_in_use(8), "never allocated, so not in use");
        assert!(!bitmap.is_in_use(u64::MAX));
    }

    #[test]
    fn counts_and_capacity_are_consistent() {
        let bitmap = MftBitmap::from_bytes(vec![0xFF, 0x00, 0x0F]);
        assert_eq!(bitmap.count_in_use(), 12);
        assert_eq!(bitmap.capacity(), 24);

        let empty = MftBitmap::from_bytes(Vec::new());
        assert_eq!(empty.count_in_use(), 0);
        assert!(empty.is_empty());
        assert!(!empty.is_in_use(0));
    }

    #[test]
    fn a_full_bitmap_counts_every_record() {
        let bitmap = MftBitmap::from_bytes(vec![0xFF; 1000]);
        assert_eq!(bitmap.count_in_use(), 8000);
        assert_eq!(bitmap.count_in_use(), bitmap.capacity());
    }
}
