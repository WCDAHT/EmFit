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

use std::fmt::Write as _;

use emfit_core::parser::block::{AlignedBuf, BlockSource, FileBlockSource};
use emfit_core::parser::ntfs::{BootSector, MftExtents, retrieval};
use emfit_core::service::{elevation, volume};

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

    match retrieval::mft_extents(letter, boot.bytes_per_cluster, boot.bytes_per_record) {
        Ok(map) => {
            writeln!(out, "\n$MFT extent map (FSCTL_GET_RETRIEVAL_POINTERS):")?;
            describe(&map, out)?;
        }
        Err(e) => writeln!(out, "\nretrieval pointers unavailable: {e}")?,
    }

    Ok(())
}

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
