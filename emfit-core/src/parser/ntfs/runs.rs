//! Data runs: how NTFS records where a file's clusters actually are.
//!
//! A non-resident attribute stores its content as a *run list* - a compact
//! sequence of (length, offset) pairs, each offset a **signed delta** from the
//! previous run's start. Decoding it is how the MFT's own extent map is
//! recovered when the filesystem driver will not hand it over (a raw image, or
//! a volume Windows refuses to mount).
//!
//! Pure parsing over a byte buffer; no I/O, testable anywhere.

/// One run of clusters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataRun {
    /// First cluster on disk, or `None` for a sparse run - a hole that
    /// occupies no space and reads as zeroes.
    pub lcn: Option<u64>,
    /// Length of the run in clusters.
    pub cluster_count: u64,
}

impl DataRun {
    /// True for a hole: virtual clusters with no physical backing.
    pub fn is_sparse(&self) -> bool {
        self.lcn.is_none()
    }
}

/// Decode a run list.
///
/// Stops at the terminating zero byte, at the end of the buffer, or at the
/// first malformed header - returning what was decoded so far rather than
/// failing, because a partial run list still locates the beginning of the
/// file, and for `$MFT` that is enough to bootstrap.
///
/// Returns the runs and whether decoding ran to a proper terminator, so a
/// caller that cares can tell a complete list from a truncated one.
pub fn decode(buf: &[u8]) -> (Vec<DataRun>, bool) {
    let mut runs = Vec::new();
    let mut pos = 0usize;
    // Offsets are deltas from the previous run's start, so decoding is
    // stateful: LCN 0 is a legal cluster, and the delta may be negative.
    let mut lcn: i64 = 0;

    while pos < buf.len() {
        let header = buf[pos];
        if header == 0 {
            return (runs, true); // proper end of list
        }
        pos += 1;

        let len_size = usize::from(header & 0x0F);
        let off_size = usize::from(header >> 4);

        // A zero length field is meaningless, and either field can claim up to
        // 8 bytes; anything more is corruption.
        if len_size == 0 || len_size > 8 || off_size > 8 {
            return (runs, false);
        }
        if pos + len_size + off_size > buf.len() {
            return (runs, false);
        }

        let cluster_count = read_unsigned(&buf[pos..pos + len_size]);
        pos += len_size;

        if off_size == 0 {
            // Sparse: no offset field at all, and the running LCN does not
            // advance - the next run's delta is still relative to the last
            // *allocated* run.
            runs.push(DataRun {
                lcn: None,
                cluster_count,
            });
            continue;
        }

        let delta = read_signed(&buf[pos..pos + off_size]);
        pos += off_size;

        let Some(next) = lcn.checked_add(delta) else {
            return (runs, false);
        };
        if next < 0 {
            return (runs, false);
        }
        lcn = next;

        runs.push(DataRun {
            lcn: Some(lcn as u64),
            cluster_count,
        });
    }

    // Ran off the end without a terminator.
    (runs, false)
}

/// What a run list adds up to, without decoding it into a `Vec`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunSummary {
    /// Clusters backed by real disk locations.
    pub allocated_clusters: u64,
    /// Clusters in holes (sparse runs).
    pub hole_clusters: u64,
}

impl RunSummary {
    pub fn has_holes(&self) -> bool {
        self.hole_clusters > 0
    }
}

/// Sum a run list's allocated and hole clusters, allocation-free.
///
/// The sweep calls this for every non-resident stream, so unlike [`decode`]
/// it builds nothing - one pass over the bytes, two counters. Same
/// tolerance for truncation: a malformed tail yields what was summed.
pub fn summarize(buf: &[u8]) -> RunSummary {
    let mut summary = RunSummary::default();
    let mut pos = 0usize;

    while pos < buf.len() {
        let header = buf[pos];
        if header == 0 {
            break;
        }
        pos += 1;

        let len_size = usize::from(header & 0x0F);
        let off_size = usize::from(header >> 4);
        if len_size == 0 || len_size > 8 || off_size > 8 || pos + len_size + off_size > buf.len() {
            break;
        }

        let cluster_count = read_unsigned(&buf[pos..pos + len_size]);
        pos += len_size + off_size;

        if off_size == 0 {
            summary.hole_clusters = summary.hole_clusters.saturating_add(cluster_count);
        } else {
            summary.allocated_clusters = summary.allocated_clusters.saturating_add(cluster_count);
        }
    }
    summary
}

/// Little-endian unsigned integer of 1-8 bytes.
fn read_unsigned(bytes: &[u8]) -> u64 {
    let mut value = 0u64;
    for (i, &b) in bytes.iter().enumerate() {
        value |= u64::from(b) << (8 * i);
    }
    value
}

/// Little-endian **signed** integer of 1-8 bytes, sign-extended from its top
/// bit. Run offsets go backwards as often as forwards.
fn read_signed(bytes: &[u8]) -> i64 {
    let mut value = read_unsigned(bytes);
    let bits = bytes.len() * 8;
    if bits < 64 && value & (1 << (bits - 1)) != 0 {
        // Set every bit above the field's width.
        value |= u64::MAX << bits;
    }
    value as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_single_run() {
        // 0x21: 1 length byte, 2 offset bytes. 8 clusters at LCN 0x0100.
        let (runs, complete) = decode(&[0x21, 0x08, 0x00, 0x01, 0x00]);

        assert!(complete);
        assert_eq!(
            runs,
            vec![DataRun {
                lcn: Some(0x0100),
                cluster_count: 8
            }]
        );
    }

    #[test]
    fn offsets_accumulate_across_runs() {
        // Three runs, each offset relative to the one before:
        //   +0x0100 -> 0x0100, +0x0010 -> 0x0110, +0x0020 -> 0x0130
        let (runs, complete) = decode(&[
            0x21, 0x08, 0x00, 0x01, // 8 clusters @ 0x0100
            0x21, 0x04, 0x10, 0x00, // 4 clusters @ 0x0110
            0x21, 0x02, 0x20, 0x00, // 2 clusters @ 0x0130
            0x00,
        ]);

        assert!(complete);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].lcn, Some(0x0100));
        assert_eq!(runs[1].lcn, Some(0x0110));
        assert_eq!(runs[2].lcn, Some(0x0130));
    }

    #[test]
    fn offsets_can_go_backwards() {
        // The second run sits *before* the first on disk - ordinary on a
        // fragmented volume, and wrong by 0x200 clusters if the delta is read
        // as unsigned.
        let (runs, complete) = decode(&[
            0x21, 0x08, 0x00, 0x02, // 8 clusters @ 0x0200
            0x21, 0x04, 0x00, 0xFF, // delta -0x0100 -> 0x0100
            0x00,
        ]);

        assert!(complete);
        assert_eq!(runs[0].lcn, Some(0x0200));
        assert_eq!(runs[1].lcn, Some(0x0100), "delta must sign-extend");
    }

    #[test]
    fn single_byte_offsets_sign_extend() {
        let (runs, _) = decode(&[
            0x11, 0x08, 0x40, // 8 clusters @ 0x40
            0x11, 0x04, 0xF0, // delta -16 -> 0x30
            0x00,
        ]);
        assert_eq!(runs[0].lcn, Some(0x40));
        assert_eq!(runs[1].lcn, Some(0x30));
    }

    #[test]
    fn sparse_runs_have_no_location_and_do_not_move_the_cursor() {
        let (runs, complete) = decode(&[
            0x21, 0x08, 0x00, 0x01, // 8 clusters @ 0x0100
            0x01, 0x10, // 16 sparse clusters, no offset field
            0x21, 0x04, 0x10, 0x00, // delta from 0x0100, not from the hole
            0x00,
        ]);

        assert!(complete);
        assert_eq!(runs.len(), 3);
        assert!(runs[1].is_sparse());
        assert_eq!(runs[1].cluster_count, 16);
        assert_eq!(runs[2].lcn, Some(0x0110));
    }

    #[test]
    fn multi_byte_lengths_decode() {
        // 0x32: 2 length bytes, 3 offset bytes. 0x1234 clusters @ 0x0F4240.
        let (runs, complete) = decode(&[0x32, 0x34, 0x12, 0x40, 0x42, 0x0F, 0x00]);

        assert!(complete);
        assert_eq!(runs[0].cluster_count, 0x1234);
        assert_eq!(runs[0].lcn, Some(0x0F_4240));
    }

    #[test]
    fn an_empty_list_is_complete_and_empty() {
        assert_eq!(decode(&[0x00]), (vec![], true));
    }

    #[test]
    fn truncation_yields_what_was_decoded_and_says_so() {
        // Two good runs then a header promising bytes that are not there.
        let (runs, complete) = decode(&[
            0x21, 0x08, 0x00, 0x01, //
            0x21, 0x04, 0x10, 0x00, //
            0x21, 0x02, // cut off mid-run
        ]);

        assert!(!complete, "caller must be able to tell");
        assert_eq!(runs.len(), 2, "what did decode is still usable");
    }

    #[test]
    fn missing_terminator_is_reported() {
        let (runs, complete) = decode(&[0x21, 0x08, 0x00, 0x01]);
        assert_eq!(runs.len(), 1);
        assert!(!complete);
    }

    #[test]
    fn corrupt_headers_stop_decoding_without_panicking() {
        // Length field claiming 15 bytes.
        let (runs, complete) = decode(&[0x2F, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(runs.is_empty());
        assert!(!complete);

        // Zero-length field with a real offset field.
        let (runs, complete) = decode(&[0x20, 0x00, 0x01]);
        assert!(runs.is_empty());
        assert!(!complete);
    }

    #[test]
    fn a_negative_absolute_lcn_is_rejected() {
        // A delta that would put the run before the start of the disk.
        let (runs, complete) = decode(&[0x11, 0x08, 0x80, 0x00]);
        assert!(runs.is_empty(), "must not wrap into a huge u64");
        assert!(!complete);
    }

    #[test]
    fn empty_input_decodes_to_nothing() {
        assert_eq!(decode(&[]), (vec![], false));
    }

    #[test]
    fn summarize_counts_holes_and_allocation_without_decoding() {
        // 8 real @ 0x100, 16-cluster hole, 4 real.
        let buf = [
            0x21, 0x08, 0x00, 0x01, //
            0x01, 0x10, //
            0x21, 0x04, 0x10, 0x00, //
            0x00,
        ];
        let s = summarize(&buf);
        assert_eq!(s.allocated_clusters, 12);
        assert_eq!(s.hole_clusters, 16);
        assert!(s.has_holes());

        // All-hole ($BadClus:$Bad shaped) and all-real lists.
        let hole_only = summarize(&[0x02, 0x00, 0x10, 0x00]);
        assert_eq!(hole_only.allocated_clusters, 0);
        assert_eq!(hole_only.hole_clusters, 0x1000);

        let real_only = summarize(&[0x21, 0x08, 0x00, 0x01, 0x00]);
        assert!(!real_only.has_holes());
        assert_eq!(real_only.allocated_clusters, 8);

        // Agrees with the full decoder.
        let (runs, _) = decode(&buf);
        let allocated: u64 = runs
            .iter()
            .filter(|r| !r.is_sparse())
            .map(|r| r.cluster_count)
            .sum();
        assert_eq!(allocated, s.allocated_clusters);
    }
}
