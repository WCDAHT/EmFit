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

use crate::error::Result;
use crate::service::config::Config;
use crate::service::lock::{self, ProcessLock};
use crate::service::task::{CancellationToken, Progress};
use crate::service::{cache, scan, volume};

/// What one background run did, for the log and for `--background-scan`'s
/// exit reporting.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RunSummary {
    /// Volumes scanned and written.
    pub scanned: Vec<String>,
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
            && self.busy.is_empty()
            && self.uncacheable.is_empty()
            && self.missing.is_empty()
            && self.failed.is_empty()
    }

    /// One line for the log.
    pub fn summary(&self) -> String {
        format!(
            "{} scanned, {} busy, {} uncacheable, {} missing, {} failed",
            self.scanned.len(),
            self.busy.len(),
            self.uncacheable.len(),
            self.missing.len(),
            self.failed.len()
        )
    }
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
            Ok(true) => summary.scanned.push(wanted.clone()),
            Ok(false) => summary.uncacheable.push(wanted.clone()),
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

    tracing::info!(result = %summary.summary(), "background: run finished");
    Ok(summary)
}

/// Scan one volume and write its snapshot - the whole point of the run.
///
/// `false` means the scan succeeded but produced nothing worth keeping: a
/// volume with no change journal has no position to replay a snapshot from, so
/// storing one would only be a way to show stale data later. That is a
/// property of the volume, not a fault, so it is reported rather than retried
/// as an error every interval.
fn scan_and_save(
    info: &crate::model::volume::VolumeInfo,
    config: &Config,
    cancel: &CancellationToken,
) -> Result<bool> {
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
        return Ok(false);
    };

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
    Ok(true)
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
            busy: vec!["D:".to_string()],
            uncacheable: Vec::new(),
            missing: vec!["Z:".to_string()],
            failed: vec![("E:".to_string(), "denied".to_string())],
        };
        assert!(!summary.is_empty());
        assert_eq!(
            summary.summary(),
            "1 scanned, 1 busy, 0 uncacheable, 1 missing, 1 failed"
        );
        assert!(RunSummary::default().is_empty());
    }
}
