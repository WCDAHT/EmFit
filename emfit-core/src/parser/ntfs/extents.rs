//! Where the MFT physically lives, and how much of it can be read at once.
//!
//! The MFT is a file, and like any file on a used volume it is fragmented. Its
//! extent map turns a record number into a byte offset — and, just as
//! importantly, says **how many records follow it contiguously**.
//!
//! That second number is what makes bulk reading correct. v1 computed the
//! offset of a batch's first record and then read `count × 1024` bytes
//! straight through; any batch spanning a fragment boundary silently ingested
//! whatever happened to be on disk next. Returning the run length from the
//! same lookup makes the clamp impossible to forget rather than a separate
//! guard someone has to remember.
//!
//! Pure arithmetic; no I/O, testable anywhere.

use crate::error::{Error, Result};
use crate::parser::ntfs::runs::DataRun;

/// One contiguous piece of the MFT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    /// First virtual cluster — the offset *within the MFT*.
    pub vcn: u64,
    /// First logical cluster — where it actually sits on the volume.
    pub lcn: u64,
    /// Length in clusters.
    pub cluster_count: u64,
}

impl Extent {
    /// One past the last virtual cluster in this extent.
    pub fn end_vcn(&self) -> u64 {
        self.vcn + self.cluster_count
    }

    /// True when `vcn` falls inside this extent.
    pub fn contains(&self, vcn: u64) -> bool {
        vcn >= self.vcn && vcn < self.end_vcn()
    }
}

/// Where one MFT record sits, and how much can be read from there in one go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordLocation {
    /// Byte offset of the record, relative to the start of the volume.
    pub byte_offset: u64,
    /// How many whole records — this one included — are contiguous on disk
    /// from here. A bulk read must not exceed this.
    ///
    /// Zero means the record itself straddles a fragment boundary, which can
    /// only happen when records are larger than clusters. Such a record needs
    /// two reads; it cannot be read as part of a batch.
    pub contiguous_records: u64,
}

/// The MFT's fragment map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MftExtents {
    extents: Vec<Extent>,
    bytes_per_cluster: u64,
    bytes_per_record: u64,
}

impl MftExtents {
    /// Build from extents already in virtual-cluster order.
    ///
    /// Rejects a map with gaps or overlaps in VCN space: the MFT is one
    /// unbroken file, so a hole means the map is wrong, and silently accepting
    /// it would put every record after the hole at the wrong offset.
    pub fn new(
        extents: Vec<Extent>,
        bytes_per_cluster: u32,
        bytes_per_record: u32,
    ) -> Result<Self> {
        if bytes_per_cluster == 0 || bytes_per_record == 0 {
            return Err(Error::Parse {
                format: "MFT extents".to_string(),
                message: "zero cluster or record size".to_string(),
            });
        }

        let mut expected_vcn = 0u64;
        for (i, extent) in extents.iter().enumerate() {
            if extent.cluster_count == 0 {
                return Err(Error::Parse {
                    format: "MFT extents".to_string(),
                    message: format!("extent {i} is empty"),
                });
            }
            if extent.vcn != expected_vcn {
                return Err(Error::Parse {
                    format: "MFT extents".to_string(),
                    message: format!(
                        "extent {i} starts at VCN {} but VCN {expected_vcn} was expected — \
                         the map has a gap or an overlap",
                        extent.vcn
                    ),
                });
            }
            expected_vcn = extent.end_vcn();
        }

        Ok(Self {
            extents,
            bytes_per_cluster: u64::from(bytes_per_cluster),
            bytes_per_record: u64::from(bytes_per_record),
        })
    }

    /// The map for an MFT assumed to be one unbroken run.
    ///
    /// Used before the real map is known — reading record 0 requires locating
    /// record 0. Correct for the first fragment of any MFT, which is all the
    /// bootstrap needs.
    pub fn contiguous(
        start_lcn: u64,
        cluster_count: u64,
        bytes_per_cluster: u32,
        bytes_per_record: u32,
    ) -> Result<Self> {
        Self::new(
            vec![Extent {
                vcn: 0,
                lcn: start_lcn,
                cluster_count,
            }],
            bytes_per_cluster,
            bytes_per_record,
        )
    }

    /// Build from a decoded `$DATA` run list — the path used when the
    /// filesystem driver will not supply retrieval pointers, such as a raw
    /// image or an unmounted volume.
    ///
    /// Sparse runs advance the virtual cluster counter without contributing an
    /// extent. The MFT is never sparse in practice, but a run list that says
    /// otherwise must not silently shift every following extent.
    pub fn from_data_runs(
        runs: &[DataRun],
        bytes_per_cluster: u32,
        bytes_per_record: u32,
    ) -> Result<Self> {
        let mut extents = Vec::with_capacity(runs.len());
        let mut vcn = 0u64;

        for run in runs {
            if let Some(lcn) = run.lcn {
                extents.push(Extent {
                    vcn,
                    lcn,
                    cluster_count: run.cluster_count,
                });
            }
            vcn += run.cluster_count;
        }

        if extents.is_empty() {
            return Err(Error::Parse {
                format: "MFT extents".to_string(),
                message: "run list contains no allocated clusters".to_string(),
            });
        }

        // A sparse run leaves a VCN gap, which `new` would reject. The MFT
        // being sparse is pathological, so say so plainly instead.
        let covered: u64 = extents.iter().map(|e| e.cluster_count).sum();
        if covered != vcn {
            return Err(Error::Parse {
                format: "MFT extents".to_string(),
                message: "MFT run list is sparse; it should be fully allocated".to_string(),
            });
        }

        Self::new(extents, bytes_per_cluster, bytes_per_record)
    }

    /// Locate an MFT record.
    ///
    /// Returns `None` when the record lies past the end of the mapped extents.
    pub fn locate(&self, record_number: u64) -> Option<RecordLocation> {
        let record_byte = record_number.checked_mul(self.bytes_per_record)?;
        let target_vcn = record_byte / self.bytes_per_cluster;
        let offset_in_cluster = record_byte % self.bytes_per_cluster;

        let extent = self.extents.iter().find(|e| e.contains(target_vcn))?;

        let cluster_in_extent = target_vcn - extent.vcn;
        let byte_offset =
            (extent.lcn + cluster_in_extent) * self.bytes_per_cluster + offset_in_cluster;

        // Bytes left in this fragment, starting at this record.
        let remaining_clusters = extent.end_vcn() - target_vcn;
        let remaining_bytes = remaining_clusters * self.bytes_per_cluster - offset_in_cluster;

        Some(RecordLocation {
            byte_offset,
            contiguous_records: remaining_bytes / self.bytes_per_record,
        })
    }

    /// How many records the mapped extents can hold.
    ///
    /// An upper bound on the record count, not the number in use — the tail of
    /// the MFT is allocated but unwritten. `$MFT`'s own data size is the real
    /// figure, and the scanner uses that when it has it.
    pub fn capacity_records(&self) -> u64 {
        self.total_clusters() * self.bytes_per_cluster / self.bytes_per_record
    }

    /// Total clusters across every fragment.
    pub fn total_clusters(&self) -> u64 {
        self.extents.iter().map(|e| e.cluster_count).sum()
    }

    /// The fragments, in virtual-cluster order.
    pub fn extents(&self) -> &[Extent] {
        &self.extents
    }

    /// How many fragments the MFT is in. One means contiguous.
    pub fn fragment_count(&self) -> usize {
        self.extents.len()
    }

    /// True when the MFT is in more than one piece — which it will be on any
    /// volume that has seen use.
    pub fn is_fragmented(&self) -> bool {
        self.extents.len() > 1
    }

    pub fn bytes_per_record(&self) -> u64 {
        self.bytes_per_record
    }

    pub fn bytes_per_cluster(&self) -> u64 {
        self.bytes_per_cluster
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLUSTER: u32 = 4096;
    const RECORD: u32 = 1024;
    /// Four 1 KB records per 4 KB cluster.
    const PER_CLUSTER: u64 = 4;

    fn extent(vcn: u64, lcn: u64, cluster_count: u64) -> Extent {
        Extent {
            vcn,
            lcn,
            cluster_count,
        }
    }

    fn map(extents: Vec<Extent>) -> MftExtents {
        MftExtents::new(extents, CLUSTER, RECORD).unwrap()
    }

    #[test]
    fn locates_records_in_a_contiguous_mft() {
        // 100 clusters at LCN 1000 → 400 records.
        let m = MftExtents::contiguous(1000, 100, CLUSTER, RECORD).unwrap();

        let first = m.locate(0).unwrap();
        assert_eq!(first.byte_offset, 1000 * 4096);
        assert_eq!(first.contiguous_records, 400);

        // Record 5 is in cluster 1, at byte 1024 within it.
        let sixth = m.locate(5).unwrap();
        assert_eq!(sixth.byte_offset, 1001 * 4096 + 1024);
        assert_eq!(sixth.contiguous_records, 395);

        assert!(!m.is_fragmented());
        assert_eq!(m.capacity_records(), 400);
    }

    #[test]
    fn a_record_past_the_end_is_not_located() {
        let m = MftExtents::contiguous(1000, 10, CLUSTER, RECORD).unwrap();
        assert_eq!(m.capacity_records(), 40);

        assert!(m.locate(39).is_some());
        assert!(m.locate(40).is_none());
        assert!(m.locate(u64::MAX).is_none(), "must not overflow");
    }

    #[test]
    fn the_run_length_stops_at_a_fragment_boundary() {
        // Two fragments: 10 clusters (40 records) then 10 more, far away.
        let m = map(vec![extent(0, 1000, 10), extent(10, 5000, 10)]);

        // The last record of the first fragment can be read alone, not as part
        // of a batch that would run into unrelated disk.
        let last = m.locate(39).unwrap();
        assert_eq!(last.byte_offset, 1009 * 4096 + 3 * 1024);
        assert_eq!(
            last.contiguous_records, 1,
            "a batch here must be one record, not forty"
        );

        // The first record of the second fragment jumps to the new location.
        let next = m.locate(40).unwrap();
        assert_eq!(next.byte_offset, 5000 * 4096);
        assert_eq!(next.contiguous_records, 40);
    }

    #[test]
    fn a_batch_clamped_to_the_run_length_never_crosses_a_fragment() {
        // The property the whole type exists for, checked exhaustively: a
        // reader that trusts `contiguous_records` always stays inside one
        // extent.
        let m = map(vec![
            extent(0, 1000, 3),
            extent(3, 8000, 5),
            extent(8, 2000, 2),
        ]);

        for record in 0..m.capacity_records() {
            let loc = m.locate(record).unwrap();
            assert!(loc.contiguous_records > 0);

            // Every record the batch would cover must be where the batch
            // expects it: consecutive in byte space.
            for i in 0..loc.contiguous_records {
                let inner = m.locate(record + i).unwrap();
                assert_eq!(
                    inner.byte_offset,
                    loc.byte_offset + i * u64::from(RECORD),
                    "record {} of the batch starting at {record}",
                    record + i
                );
            }

            // And the record just past the batch must not be.
            if let Some(after) = m.locate(record + loc.contiguous_records) {
                assert_ne!(
                    after.byte_offset,
                    loc.byte_offset + loc.contiguous_records * u64::from(RECORD),
                    "batch from {record} stopped short of the real boundary"
                );
            }
        }
    }

    #[test]
    fn fragments_are_walked_in_order() {
        let m = map(vec![
            extent(0, 1000, 2), // records 0..8
            extent(2, 3000, 2), // records 8..16
            extent(4, 9000, 2), // records 16..24
        ]);

        assert_eq!(m.fragment_count(), 3);
        assert_eq!(m.capacity_records(), 24);
        assert_eq!(m.locate(0).unwrap().byte_offset, 1000 * 4096);
        assert_eq!(m.locate(8).unwrap().byte_offset, 3000 * 4096);
        assert_eq!(m.locate(16).unwrap().byte_offset, 9000 * 4096);
        assert_eq!(m.locate(23).unwrap().byte_offset, 9001 * 4096 + 3 * 1024);
    }

    #[test]
    fn records_larger_than_clusters_are_handled_honestly() {
        // 4 KB records on 512-byte clusters: eight clusters per record. A
        // fragment of five clusters cannot hold a whole record.
        let m = MftExtents::new(vec![extent(0, 100, 5), extent(5, 900, 11)], 512, 4096).unwrap();

        let first = m.locate(0).unwrap();
        assert_eq!(
            first.contiguous_records, 0,
            "this record straddles the boundary and needs two reads"
        );
        assert_eq!(first.byte_offset, 100 * 512);
    }

    #[test]
    fn a_gap_in_the_map_is_rejected() {
        // VCN 5..10 is missing. Accepting this would place every record past
        // the gap at the wrong offset — silently, and for the whole scan.
        let err = MftExtents::new(
            vec![extent(0, 1000, 5), extent(10, 5000, 5)],
            CLUSTER,
            RECORD,
        )
        .unwrap_err();
        assert!(err.to_string().contains("gap or an overlap"), "{err}");
    }

    #[test]
    fn an_overlap_in_the_map_is_rejected() {
        let err = MftExtents::new(
            vec![extent(0, 1000, 5), extent(3, 5000, 5)],
            CLUSTER,
            RECORD,
        )
        .unwrap_err();
        assert!(err.to_string().contains("gap or an overlap"), "{err}");
    }

    #[test]
    fn empty_and_degenerate_maps_are_rejected() {
        assert!(MftExtents::new(vec![extent(0, 1000, 0)], CLUSTER, RECORD).is_err());
        assert!(MftExtents::new(vec![extent(0, 1000, 5)], 0, RECORD).is_err());
        assert!(MftExtents::new(vec![extent(0, 1000, 5)], CLUSTER, 0).is_err());

        // No extents at all is legal but locates nothing.
        let empty = MftExtents::new(vec![], CLUSTER, RECORD).unwrap();
        assert_eq!(empty.capacity_records(), 0);
        assert!(empty.locate(0).is_none());
    }

    #[test]
    fn builds_from_data_runs() {
        let runs = vec![
            DataRun {
                lcn: Some(1000),
                cluster_count: 4,
            },
            DataRun {
                lcn: Some(5000),
                cluster_count: 6,
            },
        ];
        let m = MftExtents::from_data_runs(&runs, CLUSTER, RECORD).unwrap();

        assert_eq!(m.fragment_count(), 2);
        assert_eq!(m.total_clusters(), 10);
        assert_eq!(m.capacity_records(), 10 * PER_CLUSTER);
        assert_eq!(m.locate(0).unwrap().byte_offset, 1000 * 4096);
        assert_eq!(m.locate(16).unwrap().byte_offset, 5000 * 4096);
    }

    #[test]
    fn a_sparse_mft_run_list_is_refused_rather_than_misread() {
        let runs = vec![
            DataRun {
                lcn: Some(1000),
                cluster_count: 4,
            },
            DataRun {
                lcn: None,
                cluster_count: 4,
            },
            DataRun {
                lcn: Some(5000),
                cluster_count: 4,
            },
        ];
        let err = MftExtents::from_data_runs(&runs, CLUSTER, RECORD).unwrap_err();
        assert!(err.to_string().contains("sparse"), "{err}");
    }

    #[test]
    fn a_run_list_with_no_allocated_clusters_is_rejected() {
        let runs = vec![DataRun {
            lcn: None,
            cluster_count: 8,
        }];
        assert!(MftExtents::from_data_runs(&runs, CLUSTER, RECORD).is_err());
    }
}
