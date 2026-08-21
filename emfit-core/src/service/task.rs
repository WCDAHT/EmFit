//! Progress reporting and cancellation for long-running operations.
//!
//! Every app spawned from this template will run at least one long task
//! (parse a big file, export a PDF, transcode a video). All four reference
//! projects reinvented this pattern; the template ships it once.
//!
//! Usage from a service method:
//!
//! ```ignore
//! use crate::service::task::{CancellationToken, Progress};
//!
//! pub fn export_pdf(
//!     dest: &Path,
//!     cancel: CancellationToken,
//!     on_progress: impl Fn(Progress) + Send,
//! ) -> Result<()> {
//!     on_progress(Progress::started("Rendering PDF", Some(100)));
//!     for i in 0..100 {
//!         if cancel.is_cancelled() {
//!             return Err(Error::Cancelled);
//!         }
//!         // ...do a chunk of work...
//!         on_progress(Progress::Tick { done: i + 1 });
//!     }
//!     on_progress(Progress::Finished);
//!     Ok(())
//! }
//! ```
//!
//! From the shell side, wire the `on_progress` closure to a
//! `tauri::ipc::Channel<Progress>` so the webview receives typed progress
//! events without polling. See STANDARDS sec 3.5.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A typed progress event. STANDARDS sec 3.5: avoid stringly-typed status.
///
/// Each `Started` opens a *phase* and carries a human-readable `message`
/// describing what is happening right now ("Reading log", "Parsing records",
/// "Exporting to PDF"). A long operation emits several `Started` events as it
/// moves between phases; the UI shows the latest message and resets the bar.
/// The message rides on `Started` rather than `Tick` so it is set once per
/// phase, not allocated on every iteration of a tight loop.
#[derive(Debug, Clone)]
pub enum Progress {
    /// A phase begins. `message` describes what is happening right now and is
    /// shown to the user. `total` is the expected unit count when known; use
    /// `None` for indeterminate phases (the UI should show a spinner).
    Started { message: String, total: Option<u64> },
    /// One unit of progress within the current phase. `done` is cumulative,
    /// not delta.
    Tick { done: u64 },
    /// Work finished successfully.
    Finished,
    /// Work failed partway through. `message` is for user display; the
    /// underlying error should still be returned from the service method
    /// via `Result`. Use this when the UI needs to react to mid-task
    /// failure (e.g. clear a progress bar, show a toast) before the
    /// `Result` reaches the caller.
    Failed { message: String },
}

impl Progress {
    /// Open a phase. Convenience over the struct literal so call sites read
    /// `Progress::started("Reading log", Some(n))` and any `Into<String>`
    /// (a `&str` literal or an owned `String`) works without `.to_string()`.
    pub fn started(message: impl Into<String>, total: Option<u64>) -> Self {
        Progress::Started {
            message: message.into(),
            total,
        }
    }
}

/// A cancellation signal that can be cloned and checked across threads.
/// Wire to a Cancel button per STANDARDS sec 3.5.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Signal cancellation. Idempotent.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Check whether cancellation has been requested. Cheap; call freely
    /// from inside tight loops.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_observable_across_clones() {
        let a = CancellationToken::new();
        let b = a.clone();
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled());
    }

    #[test]
    fn cancel_is_idempotent() {
        let t = CancellationToken::new();
        t.cancel();
        t.cancel();
        assert!(t.is_cancelled());
    }
}
