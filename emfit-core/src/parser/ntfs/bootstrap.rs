//! Finding the MFT by reading the MFT.
//!
//! Record 0 is `$MFT` — the table describing itself. Its `$DATA` run list is
//! the extent map, and its data size is how much of the table is real rather
//! than merely allocated. Getting at it is a small bootstrap: the boot sector
//! says where the table *starts*, which is enough to read record 0, which then
//! says where the rest of it is.
//!
//! This is the route that works everywhere — a disk image, an unmounted
//! volume, a volume whose driver will not answer. Asking the driver
//! ([`retrieval`]) is faster when it is available, but it is the optimization,
//! not the foundation.
//!
//! # Why several records are read at once
//!
//! On a badly fragmented volume `$MFT`'s own run list outgrows record 0 and
//! spills into extension records, which live in the low-numbered records right
//! behind it. Reading only record 0 then yields a *prefix* of the map — the
//! same silent truncation as a single retrieval-pointers call. Reading the
//! first several records in one go costs one I/O and covers the case.
//!
//! [`retrieval`]: crate::parser::ntfs::retrieval

use crate::error::{Error, Result};
use crate::parser::block::{AlignedBuf, BlockSource};
use crate::parser::ntfs::attr::{self, AttributeType};
use crate::parser::ntfs::boot::{BOOT_SECTOR_LEN, BootSector};
use crate::parser::ntfs::extents::MftExtents;
use crate::parser::ntfs::record::{Record, RecordHeader};
use crate::parser::ntfs::runs::DataRun;

/// How many records to pull in while bootstrapping.
///
/// Enough to cover `$MFT`'s extension records, which sit among the reserved
/// low records. WizTree reads sixteen for the same reason.
const BOOTSTRAP_RECORDS: u64 = 16;

/// Everything needed to sweep the MFT, recovered from the volume itself.
#[derive(Debug, Clone)]
pub struct MftLayout {
    /// Volume geometry.
    pub boot: BootSector,
    /// Where every fragment of the table is.
    pub extents: MftExtents,
    /// Bytes of the table that hold real records.
    ///
    /// The MFT is allocated in advance and never shrinks, so its allocated
    /// size overstates the record count — often by a lot on a volume that has
    /// had many files deleted. This is the figure to sweep.
    pub data_size: u64,
}

impl MftLayout {
    /// Records worth reading: the ones backed by written data.
    pub fn record_count(&self) -> u64 {
        self.data_size / u64::from(self.boot.bytes_per_record)
    }

    /// Records the table could hold without growing.
    pub fn capacity_records(&self) -> u64 {
        self.extents.capacity_records()
    }
}

/// Read the boot sector from the start of a volume.
pub fn read_boot_sector(source: &dyn BlockSource) -> Result<BootSector> {
    let len = (source.sector_size() as usize).max(BOOT_SECTOR_LEN);
    let mut buf = AlignedBuf::for_source(source, len);
    source.read_exact_at(0, buf.as_mut_slice())?;
    BootSector::parse(buf.as_slice())
}

/// Recover the MFT's layout by reading its own first records.
pub fn probe(source: &dyn BlockSource) -> Result<MftLayout> {
    let boot = read_boot_sector(source)?;
    let layout = probe_with_boot(source, boot)?;
    tracing::info!(
        fragments = layout.extents.fragment_count(),
        records = layout.record_count(),
        capacity = layout.capacity_records(),
        "recovered the MFT layout from record 0"
    );
    Ok(layout)
}

/// As [`probe`], for a caller that already has the boot sector.
pub fn probe_with_boot(source: &dyn BlockSource, boot: BootSector) -> Result<MftLayout> {
    let record_size = u64::from(boot.bytes_per_record);
    let span = BOOTSTRAP_RECORDS * record_size;

    // Enough clusters to hold the bootstrap window. The MFT's first fragment
    // is always at least this long, so treating it as contiguous is safe here
    // even though the table as a whole is not.
    let clusters = span.div_ceil(u64::from(boot.bytes_per_cluster));
    let window = MftExtents::contiguous(
        boot.mft_start_lcn,
        clusters,
        boot.bytes_per_cluster,
        boot.bytes_per_record,
    )?;

    let start = window.locate(0).ok_or_else(|| Error::Parse {
        format: "MFT bootstrap".to_string(),
        message: "record 0 is not locatable from the boot sector".to_string(),
    })?;

    let mut buf = AlignedBuf::for_source(
        source,
        (clusters * u64::from(boot.bytes_per_cluster)) as usize,
    );
    source.read_exact_at(start.byte_offset, buf.as_mut_slice())?;

    let (runs, data_size) = collect_mft_data_runs(buf.as_mut_slice(), &boot)?;
    let extents = MftExtents::from_data_runs(&runs, boot.bytes_per_cluster, boot.bytes_per_record)?;

    // The map must reach every record the data size claims exists, or the tail
    // of the sweep would read past the end of the table.
    let mapped = extents.capacity_records() * record_size;
    if data_size > mapped {
        return Err(Error::Parse {
            format: "MFT bootstrap".to_string(),
            message: format!(
                "$MFT says {data_size} bytes of records but the recovered map covers only \
                 {mapped}; the run list is probably truncated"
            ),
        });
    }

    Ok(MftLayout {
        boot,
        extents,
        data_size,
    })
}

/// Pull `$MFT`'s `$DATA` run list out of the bootstrap window, following
/// extension records when the list did not fit in record 0.
///
/// Returns the runs in virtual-cluster order and the attribute's data size.
fn collect_mft_data_runs(window: &mut [u8], boot: &BootSector) -> Result<(Vec<DataRun>, u64)> {
    let record_size = boot.bytes_per_record as usize;
    let available = window.len() / record_size;

    // Which records to look in: record 0 always, plus whatever its
    // $ATTRIBUTE_LIST points at, if it has one.
    let mut fragments: Vec<(u64, Vec<DataRun>)> = Vec::new();
    let mut data_size = 0u64;
    let mut extension_records: Vec<u64> = Vec::new();

    for index in 0..available.min(BOOTSTRAP_RECORDS as usize) {
        let at = index * record_size;
        let record_bytes = &mut window[at..at + record_size];

        // Only record 0 and its extensions are of interest; the rest of the
        // window is $MFTMirr, $LogFile and friends.
        if index != 0 && !extension_records.contains(&(index as u64)) {
            continue;
        }

        let Ok(header) = RecordHeader::parse(record_bytes) else {
            if index == 0 {
                return Err(Error::Parse {
                    format: "MFT bootstrap".to_string(),
                    message: "record 0 is not a valid MFT record".to_string(),
                });
            }
            continue;
        };
        if !header.is_in_use() {
            continue;
        }

        let record = Record::parse(record_bytes, boot.bytes_per_sector)?;

        for attribute in record.attributes() {
            match attribute.kind() {
                // An attribute list means the run list is split across records.
                AttributeType::AttributeList if index == 0 => {
                    if let Some(value) = attribute.resident_value() {
                        for entry in attr::parse_attribute_list(value) {
                            if entry.type_code == 0x80
                                && entry.record_number != 0
                                && entry.record_number < BOOTSTRAP_RECORDS
                                && !extension_records.contains(&entry.record_number)
                            {
                                extension_records.push(entry.record_number);
                            }
                        }
                    }
                }
                // The unnamed $DATA is the table itself; a named one would be
                // an alternate stream, which $MFT does not have.
                AttributeType::Data if attribute.is_unnamed() => {
                    if let Some(nr) = attribute.non_resident() {
                        // Only the fragment starting at VCN 0 carries the real
                        // sizes; the others repeat stale values.
                        if nr.starting_vcn() == 0 {
                            data_size = nr.data_size();
                        }
                        fragments.push((nr.starting_vcn(), nr.data_runs()));
                    }
                }
                _ => {}
            }
        }
    }

    if fragments.is_empty() {
        return Err(Error::Parse {
            format: "MFT bootstrap".to_string(),
            message: "record 0 has no non-resident $DATA attribute".to_string(),
        });
    }

    // Fragments may be listed out of order; virtual-cluster order is what
    // makes the concatenated run list meaningful.
    fragments.sort_by_key(|(vcn, _)| *vcn);
    let runs: Vec<DataRun> = fragments.into_iter().flat_map(|(_, runs)| runs).collect();

    if data_size == 0 {
        return Err(Error::Parse {
            format: "MFT bootstrap".to_string(),
            message: "$MFT reports a zero data size".to_string(),
        });
    }

    Ok((runs, data_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_count_uses_data_size_not_capacity() {
        // The distinction that decides how much of the table gets swept: a
        // volume that once held many more files has a table far larger than
        // its current contents.
        let boot = BootSector {
            bytes_per_sector: 512,
            bytes_per_cluster: 4096,
            bytes_per_record: 1024,
            mft_start_lcn: 100,
            mft_mirror_lcn: 2,
            total_sectors: 1000,
            serial: 0,
        };
        let extents = MftExtents::contiguous(100, 1000, 4096, 1024).unwrap();
        let layout = MftLayout {
            boot,
            extents,
            // Half the allocated table holds real records.
            data_size: 1000 * 4096 / 2,
        };

        assert_eq!(layout.capacity_records(), 4000);
        assert_eq!(layout.record_count(), 2000);
    }
}
