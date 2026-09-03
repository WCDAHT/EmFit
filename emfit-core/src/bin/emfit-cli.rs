//! Verification CLI - a thin binary over `emfit-core` (roadmap M1).
//!
//! Just enough surface to test the engine without a UI: list volumes, scan
//! one, print the raw counters, dump a single MFT record, print a size tree.
//! The full CLI of features.md sec 10 (search, largest, export, monitor, stable
//! machine-readable output) waits for M6; nothing here is a stability promise.
//!
//! Every command that reads a volume also accepts `--image FILE`, which works
//! on any platform and needs no elevation - that is how the fixture images in
//! `emfit-core/tests/` and evidence images in the field are exercised.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use emfit_core::error::{Error, Result};
use emfit_core::model::index::{FS_OBJECT_MASK, Index, NodeId};
use emfit_core::model::volume::VolumeInfo;
use emfit_core::parser::block::{AlignedBuf, BlockSource, FileBlockSource};
use emfit_core::parser::ntfs::attr::AttributeType;
use emfit_core::parser::ntfs::record::{Record, RecordHeader};
use emfit_core::parser::ntfs::{ScanOptions, bootstrap};
use emfit_core::service::task::{CancellationToken, Progress};
use emfit_core::service::{benchlog, cache, elevation, replay, scan, view, volume};

const USAGE: &str = "\
EmFit verification CLI (M1) - see features.md sec 10 for the eventual full surface

Usage:
  emfit-cli volumes
  emfit-cli scan      (--drive C | --image FILE) [--mode auto|physical|volume]
                      [--no-free-space]
  emfit-cli stats     (--drive C | --image FILE) [--mode ...] [--no-free-space]
  emfit-cli read-mft  (--drive C | --image FILE) --record N [--mode ...]
  emfit-cli tree-size (--drive C | --image FILE) [--depth N] [--top N]
  emfit-cli cache     list | verify | clear
  emfit-cli cache     replay --drive C   [--find TEXT] [--top N]
  emfit-cli cache     repair --drive C   [--top N]

Scanning a drive uses its cached snapshot and replays the change journal onto
it when it can (caching.md); --no-cache forces a real sweep, --save-cache
writes the result back for next time.
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
        "cache" => cmd_cache(&parsed),
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
/// dependency (STANDARDS sec 5.7).
#[derive(Debug, Default)]
struct CliArgs {
    drive: Option<char>,
    image: Option<PathBuf>,
    mode: scan::ScanMode,
    record: Option<u64>,
    depth: usize,
    top: usize,
    no_free_space: bool,
    no_cache: bool,
    save_cache: bool,
    find: Option<String>,
    rest: Vec<String>,
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
                "--find" => out.find = Some(value("--find")?.to_string()),
                "--no-cache" => out.no_cache = true,
                "--save-cache" => out.save_cache = true,
                // Sub-commands (`cache list`) rather than flags.
                other if !other.starts_with('-') => out.rest.push(other.to_string()),
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
            cache: if self.no_cache {
                scan::CachePolicy::Ignore
            } else {
                scan::CachePolicy::Use
            },
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
        eprintln!("\nnote: not elevated - scans of these volumes will be refused");
    }
    Ok(())
}

fn cmd_scan(args: &CliArgs, verbose_stats: bool) -> Result<()> {
    let outcome = run_scan(args)?;
    let stats = &outcome.stats;
    let index = &outcome.index;
    let root = index.node(index.root());

    println!("mode:             {}", outcome.mode.as_str());
    match &outcome.source {
        scan::OutcomeSource::Scanned => println!("source:           MFT sweep"),
        scan::OutcomeSource::Cached {
            age,
            replay,
            orders,
        } => println!(
            "source:           cache ({} old, {} journal events, {} records re-read,              {} sort orders)",
            format_age(*age),
            replay.events,
            replay.records,
            orders.len(),
        ),
    }
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

/// `cache list`, `cache verify`, `cache replay`, `cache clear`.
fn cmd_cache(args: &CliArgs) -> Result<()> {
    match args.rest.first().map(String::as_str) {
        Some("list") | None => {
            let entries = cache::list()?;
            if entries.is_empty() {
                println!("no cached scans in {}", cache::dir()?.display());
                return Ok(());
            }
            println!("Volume  Nodes        Size  Age        Journal  Taken");
            let mut total = 0u64;
            for entry in &entries {
                let manifest = &entry.manifest;
                total += entry.bytes;
                println!(
                    "{:<7} {:>10} {:>9}  {:<10} {:<8} {}",
                    manifest.volume.root_label,
                    manifest.scan.nodes,
                    human(entry.bytes),
                    format_age(manifest.scan.age()),
                    if manifest.journal.is_some() {
                        "yes"
                    } else {
                        "no"
                    },
                    manifest.scan.taken_at,
                );
            }
            println!("\n{} snapshots, {} total", entries.len(), human(total));
            println!("{}", cache::dir()?.display());
            Ok(())
        }
        Some("verify") => {
            let entries = cache::list()?;
            if entries.is_empty() {
                println!("no cached scans to verify");
                return Ok(());
            }
            for entry in entries {
                verify_snapshot(&entry)?;
            }
            Ok(())
        }
        Some("replay") => cmd_cache_replay(args),
        Some("repair") => cmd_cache_repair(args),
        Some("clear") => {
            let removed = cache::clear()?;
            println!("removed {removed} snapshot(s)");
            Ok(())
        }
        Some(other) => Err(usage_err(format!("unknown cache command `{other}`"))),
    }
}

/// `cache replay --drive C`: what the journal says changed since the snapshot.
///
/// A pure diagnostic - it reads, reports, and writes nothing back, so it can be
/// run twice in a row and say the same thing. This is how to check that a file
/// created, edited, or deleted a moment ago actually reaches the index.
fn cmd_cache_replay(args: &CliArgs) -> Result<()> {
    let letter = args
        .drive
        .ok_or_else(|| usage_err("cache replay needs --drive C".to_string()))?;
    let volume = find_volume(letter)?;
    let stamp = cache::VolumeStamp::of(&volume);

    let Some(loaded) = cache::load(&stamp)? else {
        println!("no cached scan for {}", volume.display_name());
        return Ok(());
    };
    println!(
        "snapshot: {} nodes, taken {} ({} ago)",
        loaded.columns.len(),
        loaded.manifest.scan.taken_at,
        format_age(loaded.manifest.scan.age())
    );
    let Some(journal) = loaded.manifest.journal else {
        println!("it recorded no journal position, so it cannot be replayed");
        return Ok(());
    };

    let (source, _) = scan::open_volume_source(&volume, scan::ScanMode::Auto)?;
    let layout = bootstrap::probe(&source)?;
    let cancel = CancellationToken::new();

    let started = Instant::now();
    match replay::replay(letter, &layout, journal, loaded.columns.len(), &cancel)? {
        replay::Replay::Blocked(reason) => {
            println!("cannot replay: {}", reason.explain());
            println!("a scan of this volume would sweep the MFT instead");
        }
        replay::Replay::Unchanged { .. } => {
            println!("nothing has changed since the snapshot was taken");
        }
        replay::Replay::Changed { patch, stats, .. } => {
            println!(
                "{} journal events over {} records, in {:.0} ms",
                stats.events,
                stats.records,
                started.elapsed().as_secs_f64() * 1000.0
            );

            // A record that was touched but produced no entry is a file that
            // no longer exists; the rest are its current state.
            let fresh: std::collections::HashSet<u64> = patch
                .entries
                .iter()
                .map(|e| e.fs_id & FS_OBJECT_MASK)
                .collect();
            let gone: Vec<u64> = patch
                .replace
                .iter()
                .copied()
                .filter(|record| !fresh.contains(record))
                .collect();

            // Record number -> node, so a changed file can be shown with the
            // path the snapshot knew it by. One pass over the id column; the
            // alternative is a scan of three million ids per line printed.
            let wanted: std::collections::HashSet<u64> = gone
                .iter()
                .copied()
                .chain(patch.entries.iter().map(|e| e.parent_id & FS_OBJECT_MASK))
                .collect();
            let mut by_record: std::collections::HashMap<u64, usize> =
                std::collections::HashMap::with_capacity(wanted.len());
            for (at, &fs_id) in loaded.columns.fs_ids.iter().enumerate() {
                let record = fs_id & FS_OBJECT_MASK;
                if wanted.contains(&record) {
                    by_record.entry(record).or_insert(at);
                }
            }

            let root = loaded.manifest.volume.root_label.as_str();
            let needle = args.find.as_ref().map(|text| text.to_lowercase());
            let matches = |text: &str| match &needle {
                Some(needle) => text.to_lowercase().contains(needle.as_str()),
                None => true,
            };
            // A search is asking about one thing; a listing is asking for a
            // sample. Only the sample needs a limit.
            let limit = if needle.is_some() {
                usize::MAX
            } else {
                args.top
            };
            if let Some(text) = &args.find {
                println!("\nshowing only what matches `{text}`");
            }

            let added: Vec<(String, u64, u64)> = patch
                .entries
                .iter()
                .map(|entry| {
                    let parent = by_record
                        .get(&(entry.parent_id & FS_OBJECT_MASK))
                        .map(|&at| snapshot_path(&loaded.columns, at, root))
                        .unwrap_or_else(|| "(a folder the snapshot never saw)".to_string());
                    (
                        format!("{parent}\\{}", entry.name),
                        entry.size,
                        entry.fs_id & FS_OBJECT_MASK,
                    )
                })
                .filter(|(path, _, _)| matches(path))
                .collect();

            println!("\n{} of {} entries now:", added.len(), patch.entries.len());
            for (path, size, record) in added.iter().take(limit) {
                println!("  + {:>10}  {path}   [record {record}]", human(*size));
            }
            if added.len() > limit {
                println!(
                    "  ... and {} more (raise --top, or narrow with --find TEXT)",
                    added.len() - limit
                );
            }

            let removed: Vec<(String, u64)> = gone
                .iter()
                .map(|&record| {
                    let was = by_record
                        .get(&record)
                        .map(|&at| snapshot_path(&loaded.columns, at, root))
                        .unwrap_or_else(|| "(not in the snapshot either)".to_string());
                    (was, record)
                })
                .filter(|(was, _)| matches(was))
                .collect();

            println!("\n{} of {} records now empty:", removed.len(), gone.len());
            for (was, record) in removed.iter().take(limit) {
                println!("  - {was}   [record {record}]");
            }
            if removed.len() > limit {
                println!(
                    "  ... and {} more (raise --top, or narrow with --find TEXT)",
                    removed.len() - limit
                );
            }
        }
    }
    Ok(())
}

/// `cache repair --drive C`: prove the order repair against the real volume.
///
/// Replays the journal for real, rebuilds under the patch, then for every
/// cached column does the same job twice - repair the stored order, and build
/// the order from scratch - and says whether the two agree and what each cost.
/// Writes nothing, so it is safe to run on a live drive as often as you like.
///
/// Agreement is the whole claim. A repair that is merely *fast* and produces a
/// different order than a rebuild would is a view that sorts by the wrong
/// thing, which is worse than a slow one.
fn cmd_cache_repair(args: &CliArgs) -> Result<()> {
    let letter = args
        .drive
        .ok_or_else(|| usage_err("cache repair needs --drive C".to_string()))?;
    let volume = find_volume(letter)?;
    let stamp = cache::VolumeStamp::of(&volume);

    let Some(loaded) = cache::load(&stamp)? else {
        println!("no cached scan for {}", volume.display_name());
        return Ok(());
    };
    println!(
        "snapshot: {} nodes, {} of {} columns cached, taken {} ago",
        loaded.columns.len(),
        loaded.orders.len(),
        cache::CACHED_ORDERS.len(),
        format_age(loaded.manifest.scan.age())
    );
    if loaded.orders.is_empty() {
        println!("no sort orders stored yet - open the app once and let it warm them");
        return Ok(());
    }
    let cancel = CancellationToken::new();

    // The real journal when the volume can be opened, and a stand-in when it
    // cannot. Reading the journal needs Administrator, and the timings are
    // worth having either way - the repair does not care where the patch came
    // from, only what is in it.
    let patch = match live_patch(&volume, letter, &loaded, &cancel) {
        Ok(Some(patch)) => patch,
        Ok(None) => return Ok(()),
        Err(e) => {
            println!("cannot read the journal ({e}); simulating instead");
            simulated_patch(&loaded.columns, args.top.max(1) * 100)
        }
    };

    let started = Instant::now();
    let damage = cache::snapshot::path_damage(&loaded.columns, &patch);
    let paths_repairable = damage.repairable(loaded.columns.len());
    let damage_ms = started.elapsed().as_secs_f64() * 1000.0;

    let started = Instant::now();
    let (index, _, rebuilt, remap) = cache::snapshot::rebuild(
        &loaded.columns,
        loaded.manifest.caps.clone(),
        &patch,
        &cancel,
    )?;
    println!(
        "rebuild:  {} kept, {} dropped, {} added, in {:.0} ms",
        rebuilt.kept,
        rebuilt.dropped,
        rebuilt.added,
        started.elapsed().as_secs_f64() * 1000.0
    );
    if damage.moved == 0 {
        println!("paths:    no directory moved, in {damage_ms:.0} ms");
    } else {
        println!(
            "paths:    {} directories moved, stranding {} of {} nodes ({:.2}%), in {:.0} ms",
            damage.moved,
            damage.stranded.len(),
            loaded.columns.len(),
            100.0 * damage.stranded.len() as f64 / loaded.columns.len() as f64,
            damage_ms
        );
        println!("          {}", damage.sample.join(", "));
        if !paths_repairable {
            println!("          too much of the tree to be worth repairing");
        }
    }

    println!("\nColumn      repair    rebuild   speedup  agrees");
    let mut all_agree = true;
    for &key in cache::CACHED_ORDERS.iter() {
        let Some(stored) = loaded.orders.get(&key) else {
            println!(
                "{:<10}  {:>8}   {:>8}   {:>7}  not cached",
                format!("{key:?}"),
                "-",
                "-",
                "-"
            );
            continue;
        };
        let displaced: &[u32] = match key {
            view::SortKey::Path if !paths_repairable => {
                println!(
                    "{:<10}  {:>8}   {:>8}   {:>7}  refused (too much moved)",
                    format!("{key:?}"),
                    "-",
                    "-",
                    "-"
                );
                continue;
            }
            view::SortKey::Path => &damage.stranded,
            _ => &[],
        };

        let started = Instant::now();
        let repaired = view::repair_order(&index, key, stored, &remap, displaced);
        let repair_ms = started.elapsed().as_secs_f64() * 1000.0;

        let started = Instant::now();
        let fresh = view::build_order(&index, key);
        let build_ms = started.elapsed().as_secs_f64() * 1000.0;

        let verdict = match &repaired {
            None => {
                all_agree = false;
                "REFUSED".to_string()
            }
            Some(order) if *order == fresh => "yes".to_string(),
            Some(order) => {
                all_agree = false;
                let wrong = order
                    .iter()
                    .zip(&fresh)
                    .enumerate()
                    .find(|(_, (a, b))| a != b)
                    .map(|(at, _)| at);
                format!("NO (first differs at {wrong:?})")
            }
        };
        println!(
            "{:<10}  {:>7.0}ms  {:>7.0}ms   {:>6.0}x  {verdict}",
            format!("{key:?}"),
            repair_ms,
            build_ms,
            if repair_ms > 0.0 {
                build_ms / repair_ms
            } else {
                0.0
            }
        );
    }

    println!();
    if all_agree {
        println!("every repaired column is the order a rebuild would have produced");
    } else {
        println!("MISMATCH - a repaired column disagrees with a rebuild; do not ship this");
    }
    Ok(())
}

/// The journal's own patch, when the volume can be opened. `Ok(None)` means
/// there is nothing to report and the caller should stop.
fn live_patch(
    volume: &VolumeInfo,
    letter: char,
    loaded: &cache::Loaded,
    cancel: &CancellationToken,
) -> Result<Option<cache::Patch>> {
    let Some(journal) = loaded.manifest.journal else {
        println!("it recorded no journal position, so it cannot be replayed");
        return Ok(None);
    };
    let (source, _) = scan::open_volume_source(volume, scan::ScanMode::Auto)?;
    let layout = bootstrap::probe(&source)?;

    Ok(
        match replay::replay(letter, &layout, journal, loaded.columns.len(), cancel)? {
            replay::Replay::Blocked(reason) => {
                println!("cannot replay: {}", reason.explain());
                None
            }
            replay::Replay::Unchanged { .. } => {
                println!("journal: nothing has changed since the snapshot was taken");
                Some(cache::Patch::default())
            }
            replay::Replay::Changed { patch, stats, .. } => {
                println!(
                    "journal: {} events over {} records",
                    stats.events, stats.records
                );
                Some(patch)
            }
        },
    )
}

/// A patch the shape a journal replay produces, invented from the snapshot.
///
/// For measuring, and for checking the repair on a machine where the journal
/// cannot be read. Spread across the whole id range rather than clustered, so
/// the renumbering it forces is the worst case rather than a tail-end shuffle:
/// a third of the records are deleted outright, a third come back renamed, and
/// a third are brand new files.
fn simulated_patch(columns: &cache::Columns, records: usize) -> cache::Patch {
    let n = columns.len();
    let mut patch = cache::Patch::default();
    if n < 16 {
        return patch;
    }

    let stride = (n / records.max(1)).max(1);
    let mut fresh_id = u64::MAX / 2;
    for (turn, at) in (0..n).step_by(stride).enumerate() {
        let fs_id = columns.fs_ids[at];
        let flags = emfit_core::model::entry::EntryFlags::from_bits(columns.flags[at]);
        if flags.is_directory() || flags.is_synthetic() || fs_id == 0 {
            continue; // a moved directory is its own case; leave the tree alone
        }
        patch.replace.insert(fs_id & FS_OBJECT_MASK);
        if turn % 3 != 0 {
            // Back under a new name, which is what makes the column move
            // rather than merely renumber.
            patch.entries.push(emfit_core::model::sink::RecordedEntry {
                fs_id,
                parent_id: columns.fs_ids[columns.parents[at] as usize],
                name: format!("repaired-{turn:07}.tmp"),
                size: columns.sizes[at],
                allocated: columns.allocated[at],
                times: emfit_core::model::entry::Times {
                    mtime: columns.mtimes[at],
                    crtime: columns.crtimes[at],
                },
                flags,
            });
        }
        if turn % 3 == 1 {
            fresh_id += 1;
            patch.entries.push(emfit_core::model::sink::RecordedEntry {
                fs_id: fresh_id,
                parent_id: columns.fs_ids[columns.parents[at] as usize],
                name: format!("brand-new-{turn:07}.tmp"),
                size: 4096,
                allocated: 4096,
                times: emfit_core::model::entry::Times {
                    mtime: columns.mtimes[at],
                    crtime: columns.crtimes[at],
                },
                flags: emfit_core::model::entry::EntryFlags::empty(),
            });
        }
    }
    println!(
        "simulated: {} records replaced, {} entries back",
        patch.replace.len(),
        patch.entries.len()
    );
    patch
}

/// The full path of one node in a loaded snapshot, its own name included.
///
/// A snapshot is columns, not an `Index`, so there is no `path()` to call -
/// but the parent column is all a walk needs. The root parents itself and is
/// unnamed, which is where the volume's label goes.
fn snapshot_path(columns: &cache::Columns, at: usize, root_label: &str) -> String {
    let mut parts: Vec<&str> = vec![columns.name(at)];
    let mut cur = at;
    // The parent chain is acyclic by construction; the bound is a backstop
    // against a hand-edited snapshot.
    for _ in 0..256 {
        let parent = columns.parents[cur] as usize;
        if parent == cur {
            break;
        }
        cur = parent;
        parts.push(columns.name(cur));
    }

    let mut out = root_label.to_string();
    for part in parts.iter().rev().filter(|part| !part.is_empty()) {
        if !out.ends_with('\\') {
            out.push('\\');
        }
        out.push_str(part);
    }
    out
}

/// Load a snapshot and rebuild the index from it, reporting what that cost.
///
/// Needs no volume and no elevation, which is the point: it exercises
/// everything about a cached load except the journal replay, against the real
/// file rather than a fixture.
fn verify_snapshot(entry: &cache::Entry) -> Result<()> {
    println!(
        "{} ({}, {} nodes, taken {})",
        entry.path.display(),
        human(entry.bytes),
        entry.manifest.scan.nodes,
        entry.manifest.scan.taken_at
    );

    let started = Instant::now();
    let loaded = cache::snapshot::load(&entry.path)?;
    let read = started.elapsed();

    let started = Instant::now();
    let (index, warnings, stats, _remap) = cache::snapshot::rebuild(
        &loaded.columns,
        loaded.manifest.caps.clone(),
        &cache::Patch::default(),
        &CancellationToken::new(),
    )?;
    let rebuilt = started.elapsed();

    let root = index.node(index.root());
    println!("  read      {:>8.0} ms", read.as_secs_f64() * 1000.0);
    println!("  rebuild   {:>8.0} ms", rebuilt.as_secs_f64() * 1000.0);
    println!("  nodes     {:>8}   (kept {})", index.len(), stats.kept);
    println!(
        "  totals    {} in {} files, {} directories",
        human(root.total_size()),
        root.file_count(),
        root.dir_count()
    );
    println!("  root      {}", index.path(index.root()));
    println!(
        "  orders    {} of {} cached",
        loaded.orders.len(),
        cache::CACHED_ORDERS.len()
    );
    if !warnings.is_empty() {
        println!("  warnings  {}", warnings.len());
        for w in warnings.iter().take(4) {
            println!("    - {w:?}");
        }
    }
    Ok(())
}

/// `3h`, `2d`, `just now` - enough to judge a snapshot at a glance.
fn format_age(age: std::time::Duration) -> String {
    let secs = age.as_secs();
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// Write the scan out for next time, when the user asked for it.
///
/// Only sort columns that are expensive to rebuild are computed here; the app
/// warms them anyway after every scan, so the CLI does the same work rather
/// than shipping a snapshot the app would have to finish.
fn save_cache(volume: &VolumeInfo, outcome: &scan::VolumeScanOutcome) -> Result<()> {
    let Some(journal) = outcome.journal else {
        eprintln!("note: no journal position recorded; not caching this scan");
        return Ok(());
    };
    let started = Instant::now();
    let orders: Vec<_> = cache::CACHED_ORDERS
        .iter()
        .map(|&key| (key, view::build_order(&outcome.index, key)))
        .collect();
    eprintln!(
        "sort orders built in {:.2}s",
        started.elapsed().as_secs_f64()
    );

    let manifest = cache::manifest_for(
        volume,
        &outcome.index,
        journal,
        outcome.stats.elapsed,
        outcome.mode.as_str(),
    );
    let path = cache::save(&outcome.index, &outcome.fold, &manifest, &orders)?;
    eprintln!("cache written: {}", path.display());
    Ok(())
}

fn cmd_tree_size(args: &CliArgs) -> Result<()> {
    let outcome = run_scan(args)?;
    let index = &outcome.index;

    print_tree(index, index.root(), 0, args.depth, args.top);
    Ok(())
}

/// Directories first, largest allocation first - the WizTree ordering.
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
            "{:indent$}... {} more directories",
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
        "mode {} | {} fragments | {} records to scan | {} bytes/record",
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
        "  signature ok | in use: {} | directory: {} | links: {} | sequence: {}",
        header.is_in_use(),
        header.is_directory(),
        header.hard_link_count,
        header.sequence_number
    );
    if let Some(base) = header.base_record() {
        println!("  extension record of base {base}");
    }
    println!(
        "  used {} of {} bytes | first attribute at {:#x}",
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
                "  [{i}] {name}{stream} non-resident | vcn {}..{} | size {} | allocated {} | physical {}",
                nr.starting_vcn(),
                nr.last_vcn(),
                nr.data_size(),
                nr.allocated_size(),
                nr.physical_size(),
            );
        } else {
            let len = attribute.resident_value().map(<[u8]>::len).unwrap_or(0);
            println!("  [{i}] {name}{stream} resident | {len} bytes");
            if attribute.kind() == AttributeType::FileName
                && let Some(value) = attribute.resident_value()
                && let Some(fname) = emfit_core::parser::ntfs::FileName::parse(value)
            {
                println!(
                    "        name `{}` | parent {} | namespace {:?} | fn-size {}",
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
            let outcome = scan::scan_volume(&volume, &options, &mut progress, &cancel)?;
            if args.save_cache {
                save_cache(&volume, &outcome)?;
            }
            outcome
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
