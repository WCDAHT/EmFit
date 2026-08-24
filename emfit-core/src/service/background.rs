//! The background scan: what `EmFit.exe --background-scan` runs
//! (`background-scan.md`).
//!
//! One pass over the volumes the user asked to keep scanned, writing each
//! one's snapshot, then out. No window, no resident process - Windows Task
//! Scheduler decides when this happens, so there is nothing here that sleeps
//! or loops.
//!
//! The point is not to scan; it is to leave a *current snapshot* behind, so
//! that opening the app reloads it and replays a few minutes of change journal
//! instead of sweeping the whole MFT (`caching.md`).
//!
//! Three things it will not do:
//!
//! - **Run when the switch is off**, or when it is on with nothing selected.
//! - **Run twice at once.** A second background run finds the lock taken and
//!   leaves.
//! - **Compete with the user.** A volume the app is scanning right now is
//!   skipped, not waited for; the next interval will come round soon enough.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::service::config::Config;
use crate::service::lock::{self, ProcessLock};
use crate::service::task::{CancellationToken, Progress};
use crate::service::{cache, scan, view, volume};

/// What one background run did, for the log and for `--background-scan`'s
/// exit reporting.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RunSummary {
    /// Volumes scanned and written.
    pub scanned: Vec<String>,
    /// Volumes whose snapshot was already current, so it was left alone.
    pub current: Vec<String>,
    /// Volumes another process was already scanning.
    pub busy: Vec<String>,
    /// Volumes that scanned but cannot be kept scanned: without a change
    /// journal there is no position to replay from, so a snapshot of them
    /// could only ever be stale (`caching.md` sec 6.1).
    pub uncacheable: Vec<String>,
    /// Volumes that were asked for but are not present on this machine.
    pub missing: Vec<String>,
    /// Volumes whose scan or save failed, with why.
    pub failed: Vec<(String, String)>,
}

impl RunSummary {
    /// True when the run had nothing at all to report.
    pub fn is_empty(&self) -> bool {
        self.scanned.is_empty()
            && self.current.is_empty()
            && self.busy.is_empty()
            && self.uncacheable.is_empty()
            && self.missing.is_empty()
            && self.failed.is_empty()
    }

    /// One line for the log.
    pub fn summary(&self) -> String {
        format!(
            "{} scanned, {} current, {} busy, {} uncacheable, {} missing, {} failed",
            self.scanned.len(),
            self.current.len(),
            self.busy.len(),
            self.uncacheable.len(),
            self.missing.len(),
            self.failed.len()
        )
    }
}

/// What the last background run did, kept so the settings dialog can say
/// whether this is working.
///
/// Written by the background process and read by the app, in a file of its
/// own rather than in `config.toml`: the app holds the config in memory and
/// writes it back whole, so a background run recording itself there would be
/// erased by the next Save.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LastRun {
    /// When it finished, RFC 3339 in UTC.
    pub at: String,
    /// The one-line outcome ([`RunSummary::summary`]).
    pub summary: String,
    pub scanned: Vec<String>,
    pub failed: Vec<String>,
}

/// Where [`LastRun`] is kept - beside the cache, because it is state about
/// scanning rather than a setting anyone edits.
pub fn last_run_path() -> Result<std::path::PathBuf> {
    Ok(cache::dir()?.join("last-background-run.json"))
}

/// What the last background run did, if there has been one.
pub fn last_run() -> Option<LastRun> {
    let path = last_run_path().ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

impl LastRun {
    /// Roughly when the next run is due: this one plus the interval.
    ///
    /// Approximate on purpose, and worth saying so wherever it is shown:
    /// Windows owns the real schedule and will skip a run on battery or while
    /// the machine is asleep.
    pub fn next_due(&self, interval: crate::service::config::ScanInterval) -> Option<String> {
        let at = chrono::DateTime::parse_from_rfc3339(&self.at).ok()?;
        let next = at.checked_add_signed(chrono::Duration::minutes(interval.minutes().into()))?;
        Some(next.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
    }
}

fn record(summary: &RunSummary) -> Result<()> {
    let record = LastRun {
        at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        summary: summary.summary(),
        scanned: summary.scanned.clone(),
        failed: summary.failed.iter().map(|(v, _)| v.clone()).collect(),
    };
    let path = last_run_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&record)?)
        .map_err(|source| Error::Io { path, source })
}

/// Run one background pass, per the saved config.
///
/// Returns an empty summary when there is nothing to do - a switched-off
/// feature is not an error, and neither is another run already going.
pub fn run(cancel: &CancellationToken) -> Result<RunSummary> {
    let config = Config::load();
    if !config.background.is_active() {
        tracing::info!("background: not enabled; nothing to do");
        return Ok(RunSummary::default());
    }

    // One background run at a time. Not waited for: if the previous interval's
    // run is somehow still going, this one is redundant by definition.
    let Some(_run_lock) = ProcessLock::try_acquire(lock::BACKGROUND_RUN) else {
        tracing::info!("background: another run is in progress; leaving it to finish");
        return Ok(RunSummary::default());
    };

    let available = volume::enumerate()?;
    let mut summary = RunSummary::default();

    for wanted in &config.background.volumes {
        if cancel.is_cancelled() {
            tracing::info!("background: cancelled");
            break;
        }
        let Some(info) = available.iter().find(|v| &v.display_name() == wanted) else {
            tracing::warn!(volume = %wanted, "background: volume not present");
            summary.missing.push(wanted.clone());
            continue;
        };

        // The app is scanning this one. It is doing the same work with a user
        // watching, so it wins; this run tries again next interval.
        let Some(_volume_lock) = ProcessLock::try_acquire(&lock::volume_scan(wanted)) else {
            tracing::info!(volume = %wanted, "background: already being scanned; skipping");
            summary.busy.push(wanted.clone());
            continue;
        };

        match scan_and_save(info, &config, cancel) {
            Ok(Outcome::Written) => summary.scanned.push(wanted.clone()),
            Ok(Outcome::AlreadyCurrent) => summary.current.push(wanted.clone()),
            Ok(Outcome::Uncacheable) => summary.uncacheable.push(wanted.clone()),
            Err(e) => {
                tracing::warn!(volume = %wanted, error = %e, "background: scan failed");
                summary.failed.push((wanted.clone(), e.to_string()));
            }
        }
    }

    // Background runs write snapshots like any other scan, so they are subject
    // to the same ceiling - otherwise an unattended job is exactly the thing
    // that would fill a disk.
    match cache::evict(config.cache_budget_mb.saturating_mul(1 << 20)) {
        Ok(0) => {}
        Ok(n) => tracing::info!(evicted = n, "background: cache trimmed to budget"),
        Err(e) => tracing::warn!(error = %e, "background: could not trim the cache"),
    }

    // Recorded even when every volume failed: "it ran and got nowhere" is a
    // far more useful thing for the settings dialog to say than silence.
    if let Err(e) = record(&summary) {
        tracing::warn!(error = %e, "background: could not record the run");
    }

    tracing::info!(result = %summary.summary(), "background: run finished");
    Ok(summary)
}

/// What one volume's pass came to.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// A fresh snapshot replaced the old one.
    Written,
    /// The snapshot on disk was already up to date, so it was left alone.
    AlreadyCurrent,
    /// The scan worked but cannot be kept: a volume with no change journal has
    /// no position to replay a snapshot from, so storing one would only be a
    /// way to show stale data later. A property of the volume, not a fault, so
    /// it is reported rather than retried as an error every interval.
    Uncacheable,
}

/// Scan one volume and write its snapshot - the whole point of the run.
fn scan_and_save(
    info: &crate::model::volume::VolumeInfo,
    config: &Config,
    cancel: &CancellationToken,
) -> Result<Outcome> {
    let options = scan::VolumeScanOptions {
        // Replaying the journal onto the last snapshot gives the same index
        // for a fraction of the reads, which matters more here than anywhere:
        // this runs unattended and must not monopolize the disk.
        cache: if config.cache_enabled {
            scan::CachePolicy::Use
        } else {
            scan::CachePolicy::Ignore
        },
        ..scan::VolumeScanOptions::default()
    };

    let mut progress = |_: Progress| {};
    let outcome = scan::scan_volume(info, &options, &mut progress, cancel)?;

    let Some(journal) = outcome.journal else {
        tracing::info!(
            volume = %info.display_name(),
            "background: no change journal, so this volume cannot be kept scanned"
        );
        return Ok(Outcome::Uncacheable);
    };

    // The index came straight out of the snapshot with nothing replayed onto
    // it, so the file on disk already says exactly this. Rewriting it would
    // spend a hundred megabytes of writes to change a timestamp - and would
    // throw away the sort orders the app appended to it, which cost ten
    // seconds to build and are the difference between opening sorted and
    // waiting. Leaving it alone is both cheaper and better.
    if matches!(
        &outcome.source,
        scan::OutcomeSource::Cached { replay, .. } if replay.records == 0
    ) {
        tracing::info!(
            volume = %info.display_name(),
            "background: the snapshot is already current; leaving it as it is"
        );
        return Ok(Outcome::AlreadyCurrent);
    }

    let manifest = cache::manifest_for(
        info,
        &outcome.index,
        journal,
        outcome.stats.elapsed,
        outcome.mode.as_str(),
    );
    // Truth only. The sort orders are the app's business - they cost seconds
    // to build and only matter to a window that is open.
    let path = cache::save(&outcome.index, &outcome.fold, &manifest, &[])?;
    tracing::info!(
        volume = %info.display_name(),
        nodes = outcome.index.len(),
        path = %path.display(),
        "background: snapshot written"
    );

    // The replay renumbered nodes, so whatever orders the old snapshot carried
    // no longer address the right rows and had to go. Build them again here
    // rather than leaving the app to do it: this is unattended time, and it is
    // the whole point of scanning ahead that opening the window costs nothing.
    append_orders(&manifest.volume, &outcome.index);
    Ok(Outcome::Written)
}

/// Rebuild and store the sort columns worth caching. Best-effort: a snapshot
/// without them is merely slower to open, so nothing here is worth failing a
/// run over.
fn append_orders(stamp: &cache::VolumeStamp, index: &crate::model::index::Index) {
    let started = std::time::Instant::now();
    let orders: Vec<(view::SortKey, Vec<u32>)> = cache::CACHED_ORDERS
        .iter()
        .map(|&key| (key, view::build_order(index, key)))
        .collect();

    match cache::append_orders(stamp, index.len(), &orders) {
        Ok(0) => {}
        Ok(added) => tracing::info!(
            added,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "background: sort orders rebuilt and stored"
        ),
        Err(e) => tracing::warn!(error = %e, "background: could not store the sort orders"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_with_the_switch_off_does_nothing() {
        // `Config::load` reads the real config, which on a test machine has
        // background scanning off (it is off unless someone turns it on), so
        // this exercises the early return without touching a disk.
        let summary = run(&CancellationToken::new()).expect("never fails when idle");
        if !Config::load().background.is_active() {
            assert!(summary.is_empty(), "got {summary:?}");
        }
    }

    #[test]
    fn a_summary_reports_each_outcome() {
        let summary = RunSummary {
            scanned: vec!["C:".to_string()],
            current: Vec::new(),
            busy: vec!["D:".to_string()],
            uncacheable: Vec::new(),
            missing: vec!["Z:".to_string()],
            failed: vec![("E:".to_string(), "denied".to_string())],
        };
        assert!(!summary.is_empty());
        assert_eq!(
            summary.summary(),
            "1 scanned, 0 current, 1 busy, 0 uncacheable, 1 missing, 1 failed"
        );
        assert!(RunSummary::default().is_empty());
    }
}
