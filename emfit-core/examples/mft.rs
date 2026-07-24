//! Scratch diagnostic: read real MFT geometry from a volume.
//!
//! Raw volume reads need Administrator, so this must run elevated. Writes its
//! report to `mft-report.txt` in the working directory as well as stdout, so an
//! elevated console that closes still leaves the output behind.
//!
//! ```text
//! cargo build -p emfit-core --example mft
//! target\debug\examples\mft.exe C
//! ```

#[cfg(windows)]
use std::fmt::Write as _;

#[cfg(windows)]
use emfit_core::parser::block::{AlignedBuf, BlockSource, FileBlockSource};
#[cfg(windows)]
use emfit_core::parser::ntfs::{self as ntfs, BootSector, MftExtents, retrieval};
#[cfg(windows)]
use emfit_core::service::{elevation, volume};

#[cfg(not(windows))]
fn main() {
    eprintln!("This diagnostic reads raw NTFS volumes; it only runs on Windows.");
}

#[cfg(windows)]
fn main() {
    let letter = std::env::args()
        .nth(1)
        .and_then(|a| a.chars().next())
        .unwrap_or('C')
        .to_ascii_uppercase();

    let mut out = String::new();
    if let Err(e) = report(letter, &mut out) {
        let _ = writeln!(out, "\nFAILED: {e}");
        if !elevation::is_elevated() {
            let _ = writeln!(
                out,
                "\nRaw volume reads require Administrator. Re-run this exe from an\n\
                 elevated console (build it unelevated first, so target/ stays writable)."
            );
        }
    }

    print!("{out}");
    let _ = std::fs::write("mft-report.txt", &out);
}

#[cfg(windows)]
fn report(letter: char, out: &mut String) -> Result<(), Box<dyn std::error::Error>> {
    writeln!(out, "elevated: {}", elevation::is_elevated())?;

    let volumes = volume::enumerate()?;
    let vol = volumes
        .iter()
        .find(|v| v.drive_letter() == Some(letter))
        .ok_or_else(|| format!("no volume with letter {letter}"))?;
    let loc = vol
        .location
        .ok_or("volume spans multiple disk extents; no single partition offset")?;

    writeln!(
        out,
        "{}  [{}]  disk {} @ offset {}  ({} bytes)\n  sector {}  cluster {}  {} total / {} free",
        vol.display_name(),
        vol.filesystem.as_str(),
        loc.disk_number,
        loc.starting_offset,
        loc.length,
        vol.bytes_per_sector,
        vol.bytes_per_cluster,
        vol.total_bytes,
        vol.free_bytes,
    )?;

    // Two ways in: the volume device is addressed from zero; the physical drive
    // needs the partition offset applied. Both need Administrator.
    let (src, how) = match FileBlockSource::open_device(&vol.device_path()) {
        Ok(s) => (s.with_sector_size(vol.bytes_per_sector), "volume device"),
        Err(volume_err) => {
            writeln!(out, "\n  volume device unavailable: {volume_err}")?;
            let s = FileBlockSource::open_device(&loc.physical_drive_path())?
                .partition(loc.starting_offset, Some(loc.length))
                .with_sector_size(vol.bytes_per_sector);
            (s, "physical drive + partition offset")
        }
    };
    writeln!(out, "\nreading via: {how}")?;

    let mut buf = AlignedBuf::for_source(&src, vol.bytes_per_sector as usize);
    src.read_exact_at(0, buf.as_mut_slice())?;
    let boot = BootSector::parse(buf.as_slice())?;

    writeln!(out, "\nboot sector:")?;
    writeln!(out, "  bytes/sector    {}", boot.bytes_per_sector)?;
    writeln!(out, "  bytes/cluster   {}", boot.bytes_per_cluster)?;
    writeln!(out, "  bytes/record    {}", boot.bytes_per_record)?;
    writeln!(
        out,
        "  mft start lcn   {}  (volume byte {})",
        boot.mft_start_lcn,
        boot.mft_byte_offset()
    )?;
    writeln!(out, "  volume bytes    {}", boot.volume_bytes())?;
    writeln!(out, "  serial          {:016X}", boot.serial)?;
    writeln!(
        out,
        "  agrees with the OS: sector {}, cluster {}",
        boot.bytes_per_sector == vol.bytes_per_sector,
        boot.bytes_per_cluster == vol.bytes_per_cluster
    )?;

    // Does record 0 look like an MFT record where the boot sector says it is?
    let bootstrap = MftExtents::contiguous(
        boot.mft_start_lcn,
        1,
        boot.bytes_per_cluster,
        boot.bytes_per_record,
    )?;
    let r0 = bootstrap.locate(0).ok_or("record 0 not locatable")?;
    let mut rec = AlignedBuf::for_source(&src, boot.bytes_per_cluster as usize);
    src.read_exact_at(r0.byte_offset, rec.as_mut_slice())?;
    let sig = &rec.as_slice()[..4];
    writeln!(
        out,
        "\nrecord 0 @ byte {}: signature {:?} {}",
        r0.byte_offset,
        String::from_utf8_lossy(sig),
        if sig == b"FILE" { "OK" } else { "UNEXPECTED" }
    )?;

    // Parse record 0 properly: fixup, attribute walk, and what it says about
    // the table it lives in.
    dump_record_zero(&src, &boot, r0.byte_offset, out)?;

    // Route 1: read the map out of $MFT itself.
    let from_record = match ntfs::bootstrap::probe_with_boot(&src, boot) {
        Ok(layout) => {
            writeln!(out, "\n$MFT extent map (from record 0's $DATA):")?;
            describe(&layout.extents, out)?;
            writeln!(
                out,
                "  $MFT data size  {} bytes = {} records in use ({:.1}% of capacity)",
                layout.data_size,
                layout.record_count(),
                100.0 * layout.record_count() as f64 / layout.capacity_records() as f64,
            )?;
            Some(layout.extents)
        }
        Err(e) => {
            writeln!(out, "\nbootstrap from record 0 failed: {e}")?;
            None
        }
    };

    // Route 2: ask the driver, and check the two agree.
    match retrieval::mft_extents(letter, boot.bytes_per_cluster, boot.bytes_per_record) {
        Ok(map) => {
            writeln!(out, "\n$MFT extent map (FSCTL_GET_RETRIEVAL_POINTERS):")?;
            describe(&map, out)?;
            if let Some(from_record) = from_record {
                writeln!(
                    out,
                    "\nthe two routes agree: {}",
                    if from_record == map {
                        "YES".to_string()
                    } else {
                        format!(
                            "NO — record 0 gave {} fragments / {} clusters, \
                             the driver gave {} / {}",
                            from_record.fragment_count(),
                            from_record.total_clusters(),
                            map.fragment_count(),
                            map.total_clusters()
                        )
                    }
                )?;
            }
        }
        Err(e) => writeln!(out, "\nretrieval pointers unavailable: {e}")?,
    }

    Ok(())
}

#[cfg(windows)]
/// Parse record 0 and report what it says — the first real exercise of the
/// fixup and attribute walk against live data.
fn dump_record_zero(
    src: &FileBlockSource,
    boot: &BootSector,
    offset: u64,
    out: &mut String,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = AlignedBuf::for_source(src, boot.bytes_per_cluster as usize);
    src.read_exact_at(offset, buf.as_mut_slice())?;

    let record_bytes = &mut buf.as_mut_slice()[..boot.bytes_per_record as usize];
    let header = ntfs::RecordHeader::parse(record_bytes)?;

    writeln!(out, "\nrecord 0 header:")?;
    writeln!(out, "  in use          {}", header.is_in_use())?;
    writeln!(out, "  directory       {}", header.is_directory())?;
    writeln!(out, "  hard links      {}", header.hard_link_count)?;
    writeln!(out, "  sequence        {}", header.sequence_number)?;
    writeln!(
        out,
        "  used / alloc    {} / {} bytes",
        header.used_size, header.allocated_size
    )?;
    writeln!(out, "  base record     {:?}", header.base_record())?;
    writeln!(out, "  self-reported # {}", header.record_number)?;
    writeln!(
        out,
        "  fixup array     offset {}, {} entries",
        header.update_sequence_offset, header.update_sequence_count
    )?;

    let record = ntfs::Record::parse(record_bytes, boot.bytes_per_sector)?;
    writeln!(out, "  fixup applied   OK")?;

    writeln!(out, "\nrecord 0 attributes:")?;
    let mut name_buf = String::new();
    for attribute in record.attributes() {
        let kind = match attribute.kind() {
            ntfs::AttributeType::StandardInformation => "$STANDARD_INFORMATION".to_string(),
            ntfs::AttributeType::AttributeList => "$ATTRIBUTE_LIST".to_string(),
            ntfs::AttributeType::FileName => "$FILE_NAME".to_string(),
            ntfs::AttributeType::Data => "$DATA".to_string(),
            ntfs::AttributeType::Other(code) => format!("type {code:#04X}"),
            ntfs::AttributeType::End => break,
        };
        writeln!(
            out,
            "  {:<22} {:>5} bytes  {}",
            kind,
            attribute.len(),
            if attribute.is_non_resident() {
                "non-resident"
            } else {
                "resident"
            }
        )?;

        match attribute.kind() {
            ntfs::AttributeType::FileName => {
                if let Some(value) = attribute.resident_value()
                    && let Some(fname) = ntfs::FileName::parse(value)
                {
                    fname.decode_name_into(&mut name_buf);
                    writeln!(
                        out,
                        "      name '{}'  parent record {}  namespace {:?}",
                        name_buf,
                        fname.parent_record(),
                        fname.namespace()
                    )?;
                }
            }
            ntfs::AttributeType::StandardInformation => {
                if let Some(value) = attribute.resident_value()
                    && let Some(si) = ntfs::StandardInfo::parse(value)
                {
                    writeln!(
                        out,
                        "      attributes {:#010X}  created {}  modified {}",
                        si.attributes,
                        unix_nanos_to_date(si.created),
                        unix_nanos_to_date(si.modified)
                    )?;
                }
            }
            ntfs::AttributeType::Data => {
                if let Some(nr) = attribute.non_resident() {
                    writeln!(
                        out,
                        "      vcn {}..{}  data {} bytes  allocated {} bytes  {} runs",
                        nr.starting_vcn(),
                        nr.last_vcn(),
                        nr.data_size(),
                        nr.allocated_size(),
                        nr.data_runs().len()
                    )?;
                }
            }
            _ => {}
        }
    }

    Ok(())
}

#[cfg(windows)]
/// Just enough date formatting to eyeball a timestamp.
fn unix_nanos_to_date(nanos: i64) -> String {
    if nanos == 0 {
        return "(unset)".to_string();
    }
    let secs = nanos / 1_000_000_000;
    // Days since the epoch, converted with the civil-from-days algorithm.
    let days = secs.div_euclid(86_400);
    let time = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

#[cfg(windows)]
fn describe(map: &MftExtents, out: &mut String) -> Result<(), Box<dyn std::error::Error>> {
    writeln!(out, "  fragments       {}", map.fragment_count())?;
    writeln!(out, "  total clusters  {}", map.total_clusters())?;
    writeln!(out, "  capacity        {} records", map.capacity_records())?;

    for (i, e) in map.extents().iter().enumerate().take(12) {
        writeln!(
            out,
            "    [{i:>3}] vcn {:>12}  lcn {:>12}  {:>10} clusters",
            e.vcn, e.lcn, e.cluster_count
        )?;
    }
    if map.fragment_count() > 12 {
        writeln!(out, "    ... {} more", map.fragment_count() - 12)?;
    }

    // How the bulk reader would batch this volume, and whether the clamp ever
    // actually fires.
    let total = map.capacity_records();
    let mut record = 0u64;
    let mut batches = 0u64;
    let mut smallest = u64::MAX;
    while record < total {
        let loc = map.locate(record).ok_or("gap in the map")?;
        if loc.contiguous_records == 0 {
            writeln!(out, "  record {record} straddles a fragment boundary")?;
            record += 1;
            continue;
        }
        smallest = smallest.min(loc.contiguous_records);
        record += loc.contiguous_records;
        batches += 1;
    }
    writeln!(
        out,
        "  batching        {batches} contiguous runs, smallest {smallest} records"
    )?;
    writeln!(
        out,
        "  first record    byte {}",
        map.locate(0).map_or(0, |l| l.byte_offset)
    )?;
    Ok(())
}
