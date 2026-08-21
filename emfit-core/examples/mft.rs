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
                layout.records_to_scan(),
                100.0 * layout.records_to_scan() as f64 / layout.capacity_records() as f64,
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
                            "NO - record 0 gave {} fragments / {} clusters, \
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

    // The sweep: read every record and build an index from it.
    let layout = ntfs::bootstrap::probe_with_boot(&src, boot)?;
    sweep_and_build(&src, &layout, vol.used_bytes(), out)?;

    Ok(())
}

/// Run the full sweep into a real `IndexBuilder`, then report on both.
#[cfg(windows)]
fn sweep_and_build(
    src: &FileBlockSource,
    layout: &ntfs::MftLayout,
    used_bytes: u64,
    out: &mut String,
) -> Result<(), Box<dyn std::error::Error>> {
    use emfit_core::model::builder::IndexBuilder;
    use emfit_core::model::caps::VolumeCaps;
    use emfit_core::service::task::{CancellationToken, Progress};

    let caps = VolumeCaps {
        case_sensitive: false,
        has_stable_ids: true,
        has_hard_links: true,
        has_allocated_size: true,
        live_updates: true,
        sizes_are_exact: true,
        path_separator: '\\',
        root_label: "C:".to_string(),
    };

    let cancel = CancellationToken::new();
    let mut builder = IndexBuilder::new(caps, cancel.clone());
    let mut phase = String::new();
    let mut progress = |p: Progress| {
        if let Progress::Started { message, .. } = &p {
            phase = message.clone();
        }
    };

    writeln!(out, "\n{}", "=".repeat(60))?;
    writeln!(out, "SWEEP")?;

    let outcome = ntfs::sweep(
        src,
        layout,
        &mut builder,
        &mut progress,
        &cancel,
        ntfs::ScanOptions::default(),
    )?;
    let stats = outcome.stats;

    writeln!(out, "\nread:")?;
    writeln!(out, "  records read      {}", stats.records_read)?;
    writeln!(out, "  in use            {}", stats.records_in_use)?;
    if let Some(live) = stats.bitmap_in_use {
        writeln!(
            out,
            "  bitmap says       {live}  (agrees: {})",
            live == stats.records_in_use
        )?;
    }
    writeln!(
        out,
        "  bytes read        {} ({} reads, {:.1} MiB avg)",
        stats.bytes_read,
        stats.reads,
        stats.bytes_read as f64 / stats.reads.max(1) as f64 / (1024.0 * 1024.0)
    )?;

    writeln!(out, "\nemitted:")?;
    writeln!(out, "  entries           {}", stats.entries_emitted)?;
    writeln!(out, "  files             {}", stats.files)?;
    writeln!(out, "  directories       {}", stats.directories)?;
    writeln!(
        out,
        "  hard link names   {}  (from {} multi-linked records)",
        stats.hard_link_aliases, stats.multi_linked_records
    )?;
    writeln!(
        out,
        "  ads streams       {}  occupying {}",
        stats.ads_streams,
        format_bytes(stats.ads_bytes)
    )?;

    writeln!(out, "\nsplit files (attributes in extension records):")?;
    writeln!(out, "  extension records {}", stats.extension_records)?;
    writeln!(out, "  bases held back   {}", stats.deferred_bases)?;
    writeln!(
        out,
        "  gained a name/size {}",
        stats.resolved_from_extensions
    )?;
    writeln!(
        out,
        "  orphaned exts     {}  (base never appeared)",
        stats.orphaned_extensions
    )?;

    writeln!(out, "\nstill dropped:")?;
    writeln!(out, "  unnamed           {}", stats.unnamed_records)?;
    writeln!(out, "  8.3-only          {}", stats.dos_only_records)?;
    writeln!(out, "  bad signature     {}", stats.bad_records)?;
    writeln!(out, "  never-used slots  {}", stats.never_used_slots)?;
    writeln!(out, "  failed fixup      {}", stats.failed_fixup)?;
    writeln!(
        out,
        "  accounted for     {} of {} live records ({:.2}%)",
        stats.entries_emitted,
        stats.records_in_use,
        100.0 * stats.entries_emitted as f64 / stats.records_in_use.max(1) as f64
    )?;

    writeln!(out, "\nspeed:")?;
    writeln!(
        out,
        "  elapsed           {:.3}s",
        stats.elapsed.as_secs_f64()
    )?;
    writeln!(
        out,
        "  rate              {:.0} records/s, {:.0} MiB/s",
        stats.records_per_second(),
        stats.mib_per_second()
    )?;

    // Build the index and see whether the tree actually holds together.
    let build_started = std::time::Instant::now();
    let (index, warnings) = builder.finish();
    let build_time = build_started.elapsed();

    writeln!(out, "\nindex:")?;
    writeln!(out, "  build time        {:.3}s", build_time.as_secs_f64())?;
    writeln!(out, "  nodes             {}", index.len())?;
    let root = index.node(index.root());
    writeln!(out, "  root path         {}", index.path(index.root()))?;
    writeln!(
        out,
        "  root children     {}",
        index.children(index.root()).len()
    )?;
    writeln!(out, "  total size        {} bytes", root.total_size())?;
    writeln!(out, "  total allocated   {} bytes", root.total_allocated())?;
    writeln!(out, "  files in tree     {}", root.file_count())?;
    writeln!(out, "  dirs in tree      {}", root.dir_count())?;

    // The number that says whether the scan is believable: does what we found
    // add up to what the filesystem says is in use?
    writeln!(
        out,
        "\n  volume in use     {}\n  we account for    {}  ({:.1}%)\n  unaccounted       {}",
        format_bytes(used_bytes),
        format_bytes(root.total_size()),
        100.0 * root.total_size() as f64 / used_bytes.max(1) as f64,
        format_bytes(used_bytes.saturating_sub(root.total_size())),
    )?;

    if warnings.is_empty() {
        writeln!(out, "  warnings          none")?;
    } else {
        writeln!(out, "  warnings          {}", warnings.len())?;
        for w in warnings.iter().take(10) {
            writeln!(out, "    {w:?}")?;
        }
    }

    size_breakdown(&index, out)?;

    // A few real paths, as a sanity check that the tree is the shape it should
    // be rather than merely well-formed.
    writeln!(out, "\nsample paths (deepest found):")?;
    let mut deepest: Vec<(usize, String)> = Vec::new();
    for id in index.ids().take(400_000) {
        if index.node(id).is_directory() {
            continue;
        }
        let path = index.path(id);
        let depth = path.matches('\\').count();
        if deepest.len() < 8 {
            deepest.push((depth, path));
            deepest.sort_by_key(|(depth, _)| std::cmp::Reverse(*depth));
        } else if depth > deepest[7].0 {
            deepest[7] = (depth, path);
            deepest.sort_by_key(|(depth, _)| std::cmp::Reverse(*depth));
        }
    }
    for (_, path) in &deepest {
        writeln!(out, "  {path}")?;
    }

    writeln!(out, "\nlargest directories:")?;
    let mut dirs: Vec<_> = index
        .ids()
        .filter(|&id| index.node(id).is_directory())
        .map(|id| (index.node(id).total_size(), id))
        .collect();
    dirs.sort_unstable_by_key(|(size, _)| std::cmp::Reverse(*size));
    for (size, id) in dirs.iter().take(10) {
        writeln!(out, "  {:>15}  {}", format_bytes(*size), index.path(*id))?;
    }

    writeln!(out, "\nlargest files:")?;
    let mut files: Vec<_> = index
        .ids()
        .filter(|&id| !index.node(id).is_directory())
        .map(|id| (index.node(id).size(), id))
        .collect();
    files.sort_unstable_by_key(|(size, _)| std::cmp::Reverse(*size));
    for (size, id) in files.iter().take(10) {
        writeln!(out, "  {:>15}  {}", format_bytes(*size), index.path(*id))?;
    }

    Ok(())
}

/// Break the total down by file kind, to find where an overcount comes from.
///
/// The tell is logical size far exceeding allocated size: a sparse file, a
/// compressed one, or a cloud placeholder all report their full logical length
/// while occupying little or nothing on disk. Summing logical sizes then
/// overshoots what the volume says is used.
#[cfg(windows)]
fn size_breakdown(
    index: &emfit_core::model::index::Index,
    out: &mut String,
) -> Result<(), Box<dyn std::error::Error>> {
    use emfit_core::model::entry::EntryFlags;

    #[derive(Default)]
    struct Bucket {
        count: u64,
        size: u64,
        allocated: u64,
    }
    impl Bucket {
        fn add(&mut self, size: u64, allocated: u64) {
            self.count += 1;
            self.size += size;
            self.allocated += allocated;
        }
    }

    let mut metafiles = Bucket::default();
    let mut sparse = Bucket::default();
    let mut compressed = Bucket::default();
    let mut reparse = Bucket::default();
    let mut resident = Bucket::default();
    let mut ordinary = Bucket::default();

    // Files whose logical size most exceeds what they occupy - the individual
    // culprits, if there are a few big ones rather than many small ones.
    let mut inflated: Vec<(u64, emfit_core::model::index::NodeId)> = Vec::new();

    for id in index.ids() {
        let node = index.node(id);
        if node.is_directory() || node.is_synthetic() {
            continue;
        }
        let (size, allocated) = (node.size(), node.allocated());
        let flags = node.flags();

        // Records 0-15 are the filesystem's own metadata.
        let is_metafile = index.native_id(id).is_some_and(|n| n < 16);

        if is_metafile {
            metafiles.add(size, allocated);
        } else if flags.contains(EntryFlags::SPARSE) {
            sparse.add(size, allocated);
        } else if flags.contains(EntryFlags::COMPRESSED) {
            compressed.add(size, allocated);
        } else if flags.contains(EntryFlags::REPARSE) {
            reparse.add(size, allocated);
        } else if allocated == 0 && size > 0 {
            // No clusters: the contents live inside the MFT record itself, and
            // those bytes are already counted as part of $MFT.
            resident.add(size, allocated);
        } else {
            ordinary.add(size, allocated);
        }

        if size > allocated {
            let excess = size - allocated;
            if excess > 16 * 1024 * 1024 {
                inflated.push((excess, id));
            }
        }
    }

    writeln!(out, "\nsize breakdown (files only):")?;
    writeln!(
        out,
        "  {:<22} {:>10} {:>15} {:>15}",
        "kind", "count", "logical", "allocated"
    )?;
    for (label, bucket) in [
        ("system metafiles", &metafiles),
        ("sparse", &sparse),
        ("compressed", &compressed),
        ("reparse points", &reparse),
        ("resident (0 clusters)", &resident),
        ("ordinary", &ordinary),
    ] {
        writeln!(
            out,
            "  {:<22} {:>10} {:>15} {:>15}",
            label,
            bucket.count,
            format_bytes(bucket.size),
            format_bytes(bucket.allocated)
        )?;
    }

    let total_logical = metafiles.size
        + sparse.size
        + compressed.size
        + reparse.size
        + resident.size
        + ordinary.size;
    let total_alloc = metafiles.allocated
        + sparse.allocated
        + compressed.allocated
        + reparse.allocated
        + resident.allocated
        + ordinary.allocated;
    writeln!(
        out,
        "  {:<22} {:>10} {:>15} {:>15}",
        "TOTAL",
        metafiles.count
            + sparse.count
            + compressed.count
            + reparse.count
            + resident.count
            + ordinary.count,
        format_bytes(total_logical),
        format_bytes(total_alloc)
    )?;

    writeln!(
        out,
        "\n  if logical were replaced by allocated: {}",
        format_bytes(total_alloc)
    )?;

    inflated.sort_unstable_by_key(|(excess, _)| std::cmp::Reverse(*excess));
    writeln!(out, "\nfiles reporting far more than they occupy:")?;
    if inflated.is_empty() {
        writeln!(out, "  none over 16 MiB")?;
    }
    for (excess, id) in inflated.iter().take(15) {
        let node = index.node(*id);
        writeln!(
            out,
            "  {:>12} over  ({} logical, {} on disk)  {}",
            format_bytes(*excess),
            format_bytes(node.size()),
            format_bytes(node.allocated()),
            index.path(*id)
        )?;
    }

    Ok(())
}

#[cfg(windows)]
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

#[cfg(windows)]
/// Parse record 0 and report what it says - the first real exercise of the
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
