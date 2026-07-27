//! Verification CLI — a thin binary over `emfit-core` (roadmap M1).
//!
//! Just enough surface to test the engine without a UI: list volumes, scan
//! one, print the raw counters, dump a single MFT record, print a size tree.
//! The full CLI of features.md §10 (search, largest, export, monitor, stable
//! machine-readable output) waits for M6; nothing here is a stability promise.
//!
//! Every command that reads a volume also accepts `--image FILE`, which works
//! on any platform and needs no elevation — that is how the fixture images in
//! `emfit-core/tests/` and evidence images in the field are exercised.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use emfit_core::error::{Error, Result};
use emfit_core::model::index::{Index, NodeId};
use emfit_core::model::volume::VolumeInfo;
use emfit_core::parser::block::{AlignedBuf, BlockSource, FileBlockSource};
use emfit_core::parser::ntfs::attr::AttributeType;
use emfit_core::parser::ntfs::record::{Record, RecordHeader};
use emfit_core::parser::ntfs::{ScanOptions, bootstrap};
use emfit_core::service::task::{CancellationToken, Progress};
use emfit_core::service::{benchlog, elevation, scan, volume};

const USAGE: &str = "\
EmFit verification CLI (M1) — see features.md §10 for the eventual full surface

Usage:
  emfit-cli volumes
  emfit-cli scan      (--drive C | --image FILE) [--mode auto|physical|volume]
                      [--no-free-space]
  emfit-cli stats     (--drive C | --image FILE) [--mode ...] [--no-free-space]
  emfit-cli read-mft  (--drive C | --image FILE) --record N [--mode ...]
  emfit-cli tree-size (--drive C | --image FILE) [--depth N] [--top N]

Raw volume access needs Administrator; --image does not.
RUST_LOG=debug for engine diagnostics.";

fn main() -> ExitCode {
    // Diagnostics to stderr only, so stdout stays clean output. Quiet unless
    // RUST_LOG says otherwise.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };

    let parsed = match CliArgs::parse(&args[1..]) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("error: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let outcome = match command.as_str() {
        "volumes" => cmd_volumes(),
        "scan" => cmd_scan(&parsed, false),
        "stats" => cmd_scan(&parsed, true),
        "read-mft" => cmd_read_mft(&parsed),
        "tree-size" => cmd_tree_size(&parsed),
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        other => {
            eprintln!("error: unknown command `{other}`\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            if matches!(&e, Error::Device { .. }) && !elevation::is_elevated() {
                eprintln!("hint: raw volume access requires Administrator; re-run elevated");
            }
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// argument parsing
// ---------------------------------------------------------------------------

/// What can follow a command. Hand-rolled: five flags do not justify a
/// dependency (STANDARDS §5.7).
#[derive(Debug, Default)]
struct CliArgs {
    drive: Option<char>,
    image: Option<PathBuf>,
    mode: scan::ScanMode,
    record: Option<u64>,
    depth: usize,
    top: usize,
    no_free_space: bool,
}

impl CliArgs {
    fn parse(args: &[String]) -> std::result::Result<Self, String> {
        let mut out = Self {
            depth: 3,
            top: 12,
            ..Self::default()
        };

        let mut it = args.iter();
        while let Some(flag) = it.next() {
            let mut value = |name: &str| {
                it.next()
                    .map(String::as_str)
                    .ok_or_else(|| format!("{name} needs a value"))
            };
            match flag.as_str() {
                "--drive" => {
                    let v = value("--drive")?;
                    let letter = v.trim_end_matches(['\\', '/', ':']);
                    let mut chars = letter.chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) if c.is_ascii_alphabetic() => {
                            out.drive = Some(c.to_ascii_uppercase());
                        }
                        _ => return Err(format!("`{v}` is not a drive letter")),
                    }
                }
                "--image" => out.image = Some(PathBuf::from(value("--image")?)),
                "--mode" => {
                    out.mode = match value("--mode")? {
                        "auto" => scan::ScanMode::Auto,
                        "physical" => scan::ScanMode::Physical,
                        "volume" => scan::ScanMode::VolumeHandle,
                        other => return Err(format!("unknown mode `{other}`")),
                    };
                }
                "--record" => {
                    out.record = Some(
                        value("--record")?
                            .parse()
                            .map_err(|_| "--record needs a number".to_string())?,
                    );
                }
                "--depth" => {
                    out.depth = value("--depth")?
                        .parse()
                        .map_err(|_| "--depth needs a number".to_string())?;
                }
                "--top" => {
                    out.top = value("--top")?
                        .parse()
                        .map_err(|_| "--top needs a number".to_string())?;
                }
                "--no-free-space" => out.no_free_space = true,
                other => return Err(format!("unknown flag `{other}`")),
            }
        }
        Ok(out)
    }

    /// The scan target every volume-reading command needs.
    fn target(&self) -> std::result::Result<Target, String> {
        match (&self.image, self.drive) {
            (Some(path), None) => Ok(Target::Image(path.clone())),
            (None, Some(letter)) => Ok(Target::Drive(letter)),
            (None, None) => Err("pass --drive C or --image FILE".to_string()),
            (Some(_), Some(_)) => Err("--drive and --image are mutually exclusive".to_string()),
        }
    }

    fn scan_options(&self) -> scan::VolumeScanOptions {
        scan::VolumeScanOptions {
            mode: self.mode,
            sweep: ScanOptions::default(),
            skip_free_space: self.no_free_space,
        }
    }
}

enum Target {
    Drive(char),
    Image(PathBuf),
}

fn usage_err(message: String) -> Error {
    Error::UnsupportedFormat(message)
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

fn cmd_volumes() -> Result<()> {
    let volumes = volume::enumerate()?;
    // Header spaced by hand to match the row format below.
    println!(
        "Drive  FS      Label                 Total       Free  Physical               Raw scan"
    );
    for v in &volumes {
        let physical = match v.location {
            Some(loc) => format!("disk {} @ {}", loc.disk_number, human(loc.starting_offset)),
            None => "-".to_string(),
        };
        println!(
            "{:<6} {:<7} {:<16} {:>10} {:>10}  {:<22} {}",
            v.display_name(),
            v.filesystem.as_str(),
            v.label.as_deref().unwrap_or("-"),
            human(v.total_bytes),
            human(v.free_bytes),
            physical,
            if v.supports_raw_scan() { "yes" } else { "no" },
        );
    }
    if !elevation::is_elevated() {
        eprintln!("\nnote: not elevated — scans of these volumes will be refused");
    }
    Ok(())
}

fn cmd_scan(args: &CliArgs, verbose_stats: bool) -> Result<()> {
    let outcome = run_scan(args)?;
    let stats = &outcome.stats;
    let index = &outcome.index;
    let root = index.node(index.root());

    println!("mode:             {}", outcome.mode.as_str());
    println!("index nodes:      {}", index.len());
    println!(
        "files:            {}   directories: {}",
        stats.files, stats.directories
    );
    println!(
        "total size:       {}   allocated: {}",
        human(root.total_size()),
        human(root.total_allocated())
    );
    println!(
        "hard link names:  {}   ({} records with several names)",
        stats.hard_link_aliases, stats.multi_linked_records
    );
    println!(
        "ads streams:      {}   occupying {}",
        stats.ads_streams,
        human(stats.ads_bytes)
    );
    println!(
        "elapsed:          {:.2}s   {:.0} records/s   {:.0} MiB/s",
        stats.elapsed.as_secs_f64(),
        stats.records_per_second(),
        stats.mib_per_second()
    );
    if let Some(rss) = peak_rss() {
        println!("peak rss:         {}", human(rss));
    }
    if !outcome.warnings.is_empty() {
        println!("warnings:         {}", outcome.warnings.len());
        for w in outcome.warnings.iter().take(8) {
            println!("  - {w:?}");
        }
    }

    if verbose_stats {
        println!("\nraw counters:");
        println!("  records read          {}", stats.records_read);
        println!("  records in use        {}", stats.records_in_use);
        println!("  entries emitted       {}", stats.entries_emitted);
        println!("  never-used slots      {}", stats.never_used_slots);
        println!("  bad records           {}", stats.bad_records);
        println!("  failed fixup          {}", stats.failed_fixup);
        println!("  unnamed records       {}", stats.unnamed_records);
        println!("  dos-only records      {}", stats.dos_only_records);
        println!("  extension records     {}", stats.extension_records);
        println!("  deferred bases        {}", stats.deferred_bases);
        println!("  resolved from ext.    {}", stats.resolved_from_extensions);
        println!("  orphaned extensions   {}", stats.orphaned_extensions);
        println!("  reads issued          {}", stats.reads);
        println!("  bytes read            {}", stats.bytes_read);
        match stats.bitmap_in_use {
            Some(n) => println!("  bitmap in-use count   {n}"),
            None => println!("  bitmap in-use count   (bitmap not read)"),
        }
    }

    if let Ok(path) = benchlog::path() {
        eprintln!("benchlog: {}", path.display());
    }
    Ok(())
}

fn cmd_tree_size(args: &CliArgs) -> Result<()> {
    let outcome = run_scan(args)?;
    let index = &outcome.index;

    print_tree(index, index.root(), 0, args.depth, args.top);
    Ok(())
}

/// Directories first, largest allocation first — the WizTree ordering.
fn print_tree(index: &Index, id: NodeId, depth: usize, max_depth: usize, top: usize) {
    let node = index.node(id);
    let name = if id == index.root() {
        index.caps().root_label.clone()
    } else {
        index.name(id).to_string()
    };
    println!(
        "{:indent$}{:<width$} {:>10} {:>10}  {} files, {} dirs",
        "",
        name,
        human(node.total_size()),
        human(node.total_allocated()),
        node.file_count(),
        node.dir_count(),
        indent = depth * 2,
        width = 40usize.saturating_sub(depth * 2),
    );

    if depth >= max_depth {
        return;
    }
    let mut children: Vec<NodeId> = index
        .children(id)
        .iter()
        .copied()
        .filter(|&c| index.node(c).is_directory())
        .collect();
    children.sort_by_key(|&c| std::cmp::Reverse(index.node(c).total_allocated()));

    let shown = children.len().min(top);
    for &child in &children[..shown] {
        print_tree(index, child, depth + 1, max_depth, top);
    }
    if children.len() > shown {
        println!(
            "{:indent$}… {} more directories",
            "",
            children.len() - shown,
            indent = (depth + 1) * 2
        );
    }
}

fn cmd_read_mft(args: &CliArgs) -> Result<()> {
    let record_number = args
        .record
        .ok_or_else(|| usage_err("read-mft needs --record N".to_string()))?;

    let (source, mode): (FileBlockSource, scan::AccessMode) =
        match args.target().map_err(usage_err)? {
            Target::Drive(letter) => {
                let volume = find_volume(letter)?;
                scan::open_volume_source(&volume, args.mode)?
            }
            Target::Image(path) => (FileBlockSource::open_image(&path)?, scan::AccessMode::Image),
        };

    let layout = bootstrap::probe(&source)?;
    let record_size = layout.boot.bytes_per_record as usize;
    println!(
        "mode {} · {} fragments · {} records to scan · {} bytes/record",
        mode.as_str(),
        layout.extents.fragment_count(),
        layout.records_to_scan(),
        record_size
    );

    let location = layout
        .extents
        .locate(record_number)
        .ok_or_else(|| Error::Parse {
            format: "read-mft".to_string(),
            message: format!("record {record_number} lies past the mapped extents"),
        })?;

    // Devices demand sector-aligned reads; the record's offset is only
    // record-aligned (1 KB records on a 4Kn disk are *less* aligned than a
    // sector). Read the surrounding aligned window and slice.
    let sector = u64::from(source.sector_size());
    let start = location.byte_offset / sector * sector;
    let lead = (location.byte_offset - start) as usize;
    let span = (lead + record_size).div_ceil(sector as usize) * sector as usize;
    let mut buf = AlignedBuf::for_source(&source, span);
    source.read_exact_at(start, buf.as_mut_slice())?;
    let record_bytes = &mut buf.as_mut_slice()[lead..lead + record_size];

    println!(
        "record {record_number} at byte offset {}",
        location.byte_offset
    );
    let header = RecordHeader::parse(record_bytes)?;
    println!(
        "  signature ok · in use: {} · directory: {} · links: {} · sequence: {}",
        header.is_in_use(),
        header.is_directory(),
        header.hard_link_count,
        header.sequence_number
    );
    if let Some(base) = header.base_record() {
        println!("  extension record of base {base}");
    }
    println!(
        "  used {} of {} bytes · first attribute at {:#x}",
        header.used_size, header.allocated_size, header.first_attribute_offset
    );

    let record = Record::parse(record_bytes, layout.boot.bytes_per_sector)?;
    for (i, attribute) in record.attributes().enumerate() {
        let name = attribute_name(attribute.kind());
        let stream = if attribute.is_unnamed() {
            String::new()
        } else {
            format!(" stream `{}`", decode_utf16(attribute.name_bytes()))
        };
        if let Some(nr) = attribute.non_resident() {
            println!(
                "  [{i}] {name}{stream} non-resident · vcn {}..{} · size {} · allocated {} · physical {}",
                nr.starting_vcn(),
                nr.last_vcn(),
                nr.data_size(),
                nr.allocated_size(),
                nr.physical_size(),
            );
        } else {
            let len = attribute.resident_value().map(<[u8]>::len).unwrap_or(0);
            println!("  [{i}] {name}{stream} resident · {len} bytes");
            if attribute.kind() == AttributeType::FileName
                && let Some(value) = attribute.resident_value()
                && let Some(fname) = emfit_core::parser::ntfs::FileName::parse(value)
            {
                println!(
                    "        name `{}` · parent {} · namespace {:?} · fn-size {}",
                    fname.to_name(),
                    fname.parent_record(),
                    fname.namespace(),
                    fname.data_size()
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// shared plumbing
// ---------------------------------------------------------------------------

fn run_scan(args: &CliArgs) -> Result<scan::VolumeScanOutcome> {
    let options = args.scan_options();
    let cancel = CancellationToken::new();
    let mut progress = progress_printer();
    let started = Instant::now();

    let outcome = match args.target().map_err(usage_err)? {
        Target::Drive(letter) => {
            if !elevation::is_elevated() {
                eprintln!("warning: not elevated; raw volume access will likely be refused");
            }
            let volume = find_volume(letter)?;
            eprintln!(
                "scanning {} ({}, {} total, {} free)",
                volume.display_name(),
                volume.filesystem.as_str(),
                human(volume.total_bytes),
                human(volume.free_bytes)
            );
            scan::scan_volume(&volume, &options, &mut progress, &cancel)?
        }
        Target::Image(path) => scan::scan_image(&path, &options, &mut progress, &cancel)?,
    };
    eprintln!("scan finished in {:.2}s", started.elapsed().as_secs_f64());
    Ok(outcome)
}

fn find_volume(letter: char) -> Result<VolumeInfo> {
    volume::enumerate()?
        .into_iter()
        .find(|v| v.drive_letter() == Some(letter))
        .ok_or_else(|| Error::UnsupportedFormat(format!("no volume mounted as {letter}:")))
}

/// Progress on stderr: one line per phase, a live counter for ticks.
fn progress_printer() -> impl FnMut(Progress) {
    use std::io::Write;
    let mut ticking = false;
    move |p: Progress| match p {
        Progress::Started { message, total } => {
            if ticking {
                eprintln!();
                ticking = false;
            }
            match total {
                Some(total) => eprintln!("{message} ({total} expected)"),
                None => eprintln!("{message}"),
            }
        }
        Progress::Tick { done } => {
            eprint!("\r  {done}");
            let _ = std::io::stderr().flush();
            ticking = true;
        }
        Progress::Finished => {
            if ticking {
                eprintln!();
                ticking = false;
            }
        }
        Progress::Failed { message } => {
            if ticking {
                eprintln!();
                ticking = false;
            }
            eprintln!("failed: {message}");
        }
    }
}

fn attribute_name(kind: AttributeType) -> String {
    match kind {
        AttributeType::StandardInformation => "$STANDARD_INFORMATION".to_string(),
        AttributeType::AttributeList => "$ATTRIBUTE_LIST".to_string(),
        AttributeType::FileName => "$FILE_NAME".to_string(),
        AttributeType::Data => "$DATA".to_string(),
        AttributeType::Other(code) => format!("type {code:#x}"),
        AttributeType::End => "$END".to_string(),
    }
}

fn decode_utf16(bytes: &[u8]) -> String {
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
    char::decode_utf16(units)
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

fn human(bytes: u64) -> String {
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
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Peak working set of this process, for the M1 exit criterion "peak RSS
/// documented". `None` where the platform cannot say cheaply.
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "K32GetProcessMemoryInfo has no std equivalent; one call, out-param \
              is a live local of the documented size"
)]
fn peak_rss() -> Option<u64> {
    use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: the pseudo-handle needs no closing; `counters` is a live local
    // and `cb` reports its true size.
    let ok = unsafe {
        K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb).as_bool()
    };
    ok.then_some(counters.PeakWorkingSetSize as u64)
}

#[cfg(not(windows))]
fn peak_rss() -> Option<u64> {
    None
}
