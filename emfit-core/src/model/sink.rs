//! The seam itself: where scanners hand their findings over.
//!
//! [`EntrySink`] is the entire contract between "read a filesystem" and "the
//! rest of EmFit" (`architecture.md` §5). [`IndexBuilder`] is the one
//! implementation that matters; the rest of this module is the tooling that
//! makes a push-based seam workable.
//!
//! # Why doubles exist
//!
//! Push-based control flow inverts the stack: the scanner drives the loop and
//! calls into us, so there is no obvious place to stand and inspect "the
//! three-millionth entry" (`architecture.md` §9). The sink *is* that place.
//! [`ValidatingSink`] wraps any other sink and checks every entry on its way
//! past; [`RecordingSink`] captures a scan so a test can assert on what a
//! scanner produced without building an index at all; [`TeeSink`] does both at
//! once against a live run.
//!
//! [`IndexBuilder`]: crate::model::builder::IndexBuilder

use std::collections::HashSet;
use std::ops::ControlFlow;

use crate::model::entry::{EntryFlags, RawEntry, Times};

/// How many distinct warnings a [`WarningLog`] retains before it starts
/// counting instead. A corrupt volume can produce millions of
/// [`ScanWarning::MissingParent`]s; keeping them all would turn a degraded
/// scan into an out-of-memory kill.
const MAX_WARNINGS: usize = 128;

/// Something recoverable happened. The scan continues.
///
/// Unrecoverable problems are returned as `Err` from the scanner instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanWarning {
    /// A directory could not be read; its contents are missing from the index.
    UnreadableDirectory { fs_id: u64, reason: String },
    /// A metadata record was malformed and skipped.
    BadRecord { fs_id: u64, reason: String },
    /// An entry named a parent that never appeared in the scan.
    MissingParent { fs_id: u64, parent_id: u64 },
    /// A name was not valid UTF-8 and was replaced or dropped.
    NameNotUtf8 { fs_id: u64 },
    /// The scan produced no root, so one was invented.
    SynthesizedRoot,
    /// Entries that could not be reached from the root — unresolved parents or
    /// parent cycles — were gathered into a synthetic folder.
    OrphansCollected { count: u64 },
    /// Nodes remained unreachable after orphan collection. Indicates a bug in
    /// the builder rather than bad input; should never fire.
    UnreachableNodes { count: u64 },
    /// A hard limit was hit and the scan stopped early.
    LimitReached { limit: u64 },
    /// Warnings past [`MAX_WARNINGS`] were discarded.
    WarningsDropped { count: u64 },
}

/// A bounded collection of warnings.
///
/// Keeps the first [`MAX_WARNINGS`] and counts the rest, so a pathological
/// volume degrades the diagnostics rather than the process.
#[derive(Debug, Default)]
pub struct WarningLog {
    warnings: Vec<ScanWarning>,
    dropped: u64,
}

impl WarningLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a warning, or count it if the log is already full.
    pub fn push(&mut self, warning: ScanWarning) {
        if self.warnings.len() < MAX_WARNINGS {
            self.warnings.push(warning);
        } else {
            self.dropped += 1;
        }
    }

    /// Warnings recorded so far.
    pub fn as_slice(&self) -> &[ScanWarning] {
        &self.warnings
    }

    /// True when nothing has gone wrong.
    pub fn is_empty(&self) -> bool {
        self.warnings.is_empty() && self.dropped == 0
    }

    /// Consume the log, appending a [`ScanWarning::WarningsDropped`] tally if
    /// anything was discarded — so a truncated list never passes for a
    /// complete one.
    pub fn into_vec(mut self) -> Vec<ScanWarning> {
        if self.dropped > 0 {
            self.warnings.push(ScanWarning::WarningsDropped {
                count: self.dropped,
            });
        }
        self.warnings
    }
}

/// Where entries land.
///
/// The contract, in four rules (`architecture.md` §5):
///
/// 1. Entries arrive in **batches**, never one at a time.
/// 2. In **no guaranteed order** — a parent may arrive after its children.
/// 3. **Borrowed, not owned**: `name` points into the scanner's read buffer,
///    so copy what you need before returning.
/// 4. Returning `Break` **stops the scan** — cancellation and early-exit
///    limits are the same mechanism.
pub trait EntrySink {
    /// Copy what you need before returning — entries borrow the scanner's buffer.
    /// Returns `Break` to stop the scan (cancel, or an early-exit limit).
    fn push_batch(&mut self, entries: &[RawEntry<'_>]) -> ControlFlow<()>;

    /// A recoverable problem. Record it and keep scanning.
    fn warn(&mut self, w: ScanWarning);
}

impl<S: EntrySink + ?Sized> EntrySink for &mut S {
    fn push_batch(&mut self, entries: &[RawEntry<'_>]) -> ControlFlow<()> {
        (**self).push_batch(entries)
    }

    fn warn(&mut self, w: ScanWarning) {
        (**self).warn(w);
    }
}

// ---------------------------------------------------------------------------
// RecordingSink
// ---------------------------------------------------------------------------

/// An owned copy of a [`RawEntry`], for a sink that keeps what it is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedEntry {
    pub fs_id: u64,
    pub parent_id: u64,
    pub name: String,
    pub size: u64,
    pub allocated: u64,
    pub times: Times,
    pub flags: EntryFlags,
}

impl RecordedEntry {
    fn capture(entry: &RawEntry<'_>) -> Self {
        Self {
            fs_id: entry.fs_id,
            parent_id: entry.parent_id,
            name: entry.name.to_string(),
            size: entry.size,
            allocated: entry.allocated,
            times: entry.times,
            flags: entry.flags,
        }
    }

    /// Borrow this back as a [`RawEntry`], so a recorded scan can be replayed
    /// into a builder.
    pub fn as_raw(&self) -> RawEntry<'_> {
        RawEntry {
            fs_id: self.fs_id,
            parent_id: self.parent_id,
            name: &self.name,
            size: self.size,
            allocated: self.allocated,
            times: self.times,
            flags: self.flags,
        }
    }

    pub fn is_directory(&self) -> bool {
        self.flags.is_directory()
    }
}

/// Keeps everything pushed to it, and builds nothing.
///
/// The way to test a scanner: run it against a fixture image, then assert on
/// exactly what it produced — no index, no rollup, no interpretation in
/// between. Also records batch boundaries, so a test can check that a scanner
/// batches at all rather than pushing one entry at a time.
#[derive(Debug, Default)]
pub struct RecordingSink {
    entries: Vec<RecordedEntry>,
    warnings: Vec<ScanWarning>,
    batch_sizes: Vec<usize>,
    stop_after: Option<usize>,
}

impl RecordingSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return `Break` once `n` entries have been recorded.
    ///
    /// Tests that a scanner actually honours the stop signal instead of
    /// running to completion — the failure mode that would make Cancel look
    /// broken to a user.
    #[must_use]
    pub fn stop_after(mut self, n: usize) -> Self {
        self.stop_after = Some(n);
        self
    }

    pub fn entries(&self) -> &[RecordedEntry] {
        &self.entries
    }

    pub fn warnings(&self) -> &[ScanWarning] {
        &self.warnings
    }

    /// Sizes of each batch received, in order.
    pub fn batch_sizes(&self) -> &[usize] {
        &self.batch_sizes
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The first entry with this name. Convenience for readable assertions.
    pub fn find(&self, name: &str) -> Option<&RecordedEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Every entry whose parent is `parent_id`.
    pub fn children_of(&self, parent_id: u64) -> Vec<&RecordedEntry> {
        self.entries
            .iter()
            .filter(|e| e.parent_id == parent_id && e.fs_id != parent_id)
            .collect()
    }

    pub fn into_entries(self) -> Vec<RecordedEntry> {
        self.entries
    }
}

impl EntrySink for RecordingSink {
    fn push_batch(&mut self, entries: &[RawEntry<'_>]) -> ControlFlow<()> {
        self.batch_sizes.push(entries.len());

        for entry in entries {
            self.entries.push(RecordedEntry::capture(entry));
            if let Some(limit) = self.stop_after
                && self.entries.len() >= limit
            {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    fn warn(&mut self, w: ScanWarning) {
        self.warnings.push(w);
    }
}

// ---------------------------------------------------------------------------
// ValidatingSink
// ---------------------------------------------------------------------------

/// A scanner produced something structurally impossible.
///
/// Each of these is a scanner bug, not bad input: the filesystem may well be
/// corrupt, but it is the scanner's job to normalize what it reads before
/// pushing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkViolation {
    /// The name holds a path separator. Scanners push *file names*; assembling
    /// paths is the index's job, and a separator here would produce a node
    /// whose path cannot be split back apart.
    NameContainsSeparator { fs_id: u64, name: String },
    /// A literal `.` or `..` entry. FAT and ext4 store these in directory
    /// data; pushed through, they become self-loops and phantom parents.
    DotEntry { fs_id: u64, name: String },
    /// The same `fs_id` was pushed twice. The second silently displaces the
    /// first in the builder's id map, so the first entry's children reattach
    /// to the wrong node.
    DuplicateId { fs_id: u64 },
    /// More than one entry claimed to be the root by parenting itself.
    MultipleRoots { fs_id: u64, previous: u64 },
    /// A non-root entry with an empty name, which cannot be displayed or
    /// searched for.
    EmptyName { fs_id: u64 },
}

/// Wraps another sink and checks every entry on its way past.
///
/// Forwards everything unchanged, so it can be dropped into a real pipeline:
/// `ValidatingSink::new(IndexBuilder::new(..))` builds exactly the same index
/// while recording anything the scanner did wrong.
///
/// # Cost
///
/// Duplicate detection keeps a [`HashSet`] of every id seen — tens of
/// megabytes on a volume with millions of files. Intended for tests, fixtures,
/// and diagnostics, not for wrapping every production scan.
#[derive(Debug)]
pub struct ValidatingSink<S: EntrySink> {
    inner: S,
    violations: Vec<SinkViolation>,
    seen: HashSet<u64>,
    root: Option<u64>,
    strict: bool,
}

impl<S: EntrySink> ValidatingSink<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            violations: Vec::new(),
            seen: HashSet::new(),
            root: None,
            strict: false,
        }
    }

    /// Panic on the first violation instead of collecting it.
    ///
    /// This is the answer to inverted control flow: the panic unwinds through
    /// the scanner's own loop, so the backtrace points at the code that
    /// produced the bad entry rather than at the sink that noticed.
    #[must_use]
    pub fn strict(mut self) -> Self {
        self.strict = true;
        self
    }

    pub fn violations(&self) -> &[SinkViolation] {
        &self.violations
    }

    pub fn is_valid(&self) -> bool {
        self.violations.is_empty()
    }

    /// The wrapped sink.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// Unwrap, discarding the violation list.
    pub fn into_inner(self) -> S {
        self.inner
    }

    /// Unwrap, keeping the violations.
    pub fn into_parts(self) -> (S, Vec<SinkViolation>) {
        (self.inner, self.violations)
    }

    fn record(&mut self, violation: SinkViolation) {
        assert!(!self.strict, "sink validation failed: {violation:?}");
        self.violations.push(violation);
    }

    fn check(&mut self, entry: &RawEntry<'_>) {
        if entry.name.contains(['\\', '/']) {
            self.record(SinkViolation::NameContainsSeparator {
                fs_id: entry.fs_id,
                name: entry.name.to_string(),
            });
        }

        if entry.name == "." || entry.name == ".." {
            self.record(SinkViolation::DotEntry {
                fs_id: entry.fs_id,
                name: entry.name.to_string(),
            });
        }

        let is_root = entry.parent_id == entry.fs_id;

        if entry.name.is_empty() && !is_root {
            self.record(SinkViolation::EmptyName { fs_id: entry.fs_id });
        }

        if is_root {
            if let Some(previous) = self.root
                && previous != entry.fs_id
            {
                self.record(SinkViolation::MultipleRoots {
                    fs_id: entry.fs_id,
                    previous,
                });
            }
            self.root = Some(entry.fs_id);
        }

        if !self.seen.insert(entry.fs_id) {
            self.record(SinkViolation::DuplicateId { fs_id: entry.fs_id });
        }
    }
}

impl<S: EntrySink> EntrySink for ValidatingSink<S> {
    fn push_batch(&mut self, entries: &[RawEntry<'_>]) -> ControlFlow<()> {
        for entry in entries {
            self.check(entry);
        }
        self.inner.push_batch(entries)
    }

    fn warn(&mut self, w: ScanWarning) {
        self.inner.warn(w);
    }
}

// ---------------------------------------------------------------------------
// TeeSink
// ---------------------------------------------------------------------------

/// Forwards everything to two sinks.
///
/// Lets a live scan build its index *and* be recorded at the same time —
/// `TeeSink::new(IndexBuilder::new(..), RecordingSink::new())` turns a
/// reproducible fixture out of a real volume, which is how a bug found on
/// someone's disk becomes a test that runs anywhere.
#[derive(Debug)]
pub struct TeeSink<A: EntrySink, B: EntrySink> {
    a: A,
    b: B,
}

impl<A: EntrySink, B: EntrySink> TeeSink<A, B> {
    pub fn new(a: A, b: B) -> Self {
        Self { a, b }
    }

    pub fn first(&self) -> &A {
        &self.a
    }

    pub fn second(&self) -> &B {
        &self.b
    }

    pub fn into_parts(self) -> (A, B) {
        (self.a, self.b)
    }
}

impl<A: EntrySink, B: EntrySink> EntrySink for TeeSink<A, B> {
    fn push_batch(&mut self, entries: &[RawEntry<'_>]) -> ControlFlow<()> {
        // Both see every batch; either may call a halt.
        let first = self.a.push_batch(entries);
        let second = self.b.push_batch(entries);
        match (first, second) {
            (ControlFlow::Continue(()), ControlFlow::Continue(())) => ControlFlow::Continue(()),
            _ => ControlFlow::Break(()),
        }
    }

    fn warn(&mut self, w: ScanWarning) {
        self.a.warn(w.clone());
        self.b.warn(w);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(fs_id: u64, parent_id: u64, name: &str) -> RawEntry<'_> {
        RawEntry {
            fs_id,
            parent_id,
            name,
            size: 0,
            allocated: 0,
            times: Times::default(),
            flags: EntryFlags::empty(),
        }
    }

    #[test]
    fn warning_log_bounds_itself_and_says_so() {
        let mut log = WarningLog::new();
        for i in 0..MAX_WARNINGS as u64 + 50 {
            log.push(ScanWarning::MissingParent {
                fs_id: i,
                parent_id: 0,
            });
        }
        let warnings = log.into_vec();
        assert_eq!(warnings.len(), MAX_WARNINGS + 1, "the tally takes one slot");
        assert_eq!(
            warnings.last(),
            Some(&ScanWarning::WarningsDropped { count: 50 })
        );
    }

    #[test]
    fn warning_log_adds_no_tally_when_nothing_was_dropped() {
        let mut log = WarningLog::new();
        log.push(ScanWarning::SynthesizedRoot);
        assert_eq!(log.into_vec(), vec![ScanWarning::SynthesizedRoot]);
    }

    #[test]
    fn recording_sink_keeps_entries_and_batch_shape() {
        let mut sink = RecordingSink::new();
        let _ = sink.push_batch(&[entry(5, 5, ""), entry(100, 5, "a.txt")]);
        let _ = sink.push_batch(&[entry(200, 5, "b.txt")]);

        assert_eq!(sink.len(), 3);
        assert_eq!(sink.batch_sizes(), &[2, 1]);
        assert_eq!(sink.find("a.txt").unwrap().fs_id, 100);
        assert_eq!(
            sink.children_of(5).len(),
            2,
            "the root is not its own child"
        );
    }

    #[test]
    fn recording_sink_stops_where_told() {
        let mut sink = RecordingSink::new().stop_after(2);
        let flow = sink.push_batch(&[entry(1, 1, ""), entry(2, 1, "a"), entry(3, 1, "b")]);

        assert_eq!(flow, ControlFlow::Break(()));
        assert_eq!(sink.len(), 2, "nothing recorded past the limit");
    }

    #[test]
    fn recorded_entries_replay_as_raw() {
        let mut sink = RecordingSink::new();
        let _ = sink.push_batch(&[entry(100, 5, "round.trip")]);

        let recorded = &sink.entries()[0];
        let raw = recorded.as_raw();
        assert_eq!(raw.fs_id, 100);
        assert_eq!(raw.name, "round.trip");
    }

    #[test]
    fn validator_catches_separators_and_dot_entries() {
        let mut sink = ValidatingSink::new(RecordingSink::new());
        let _ = sink.push_batch(&[
            entry(5, 5, ""),
            entry(100, 5, r"docs\notes.txt"),
            entry(200, 5, "."),
            entry(300, 5, ".."),
        ]);

        let v = sink.violations();
        assert_eq!(v.len(), 3, "{v:?}");
        assert!(matches!(
            v[0],
            SinkViolation::NameContainsSeparator { fs_id: 100, .. }
        ));
        assert!(matches!(v[1], SinkViolation::DotEntry { fs_id: 200, .. }));
        assert!(matches!(v[2], SinkViolation::DotEntry { fs_id: 300, .. }));
    }

    #[test]
    fn validator_catches_duplicate_ids_and_extra_roots() {
        let mut sink = ValidatingSink::new(RecordingSink::new());
        let _ = sink.push_batch(&[
            entry(5, 5, ""),
            entry(100, 5, "a"),
            entry(100, 5, "a-again"),
            entry(9, 9, "second-root"),
        ]);

        let v = sink.violations();
        assert!(v.contains(&SinkViolation::DuplicateId { fs_id: 100 }));
        assert!(v.contains(&SinkViolation::MultipleRoots {
            fs_id: 9,
            previous: 5
        }));
    }

    #[test]
    fn validator_allows_an_empty_name_only_for_the_root() {
        let mut sink = ValidatingSink::new(RecordingSink::new());
        let _ = sink.push_batch(&[entry(5, 5, ""), entry(100, 5, "")]);

        assert_eq!(
            sink.violations(),
            &[SinkViolation::EmptyName { fs_id: 100 }]
        );
    }

    #[test]
    fn validator_forwards_everything_unchanged() {
        let mut sink = ValidatingSink::new(RecordingSink::new());
        let _ = sink.push_batch(&[entry(5, 5, ""), entry(100, 5, "kept")]);
        sink.warn(ScanWarning::SynthesizedRoot);

        let inner = sink.into_inner();
        assert_eq!(inner.len(), 2, "validation must not filter");
        assert_eq!(inner.warnings(), &[ScanWarning::SynthesizedRoot]);
    }

    #[test]
    #[should_panic(expected = "sink validation failed")]
    fn strict_validator_panics_at_the_bad_entry() {
        let mut sink = ValidatingSink::new(RecordingSink::new()).strict();
        let _ = sink.push_batch(&[entry(100, 5, "bad/name")]);
    }

    #[test]
    fn tee_forwards_to_both() {
        let mut sink = TeeSink::new(RecordingSink::new(), RecordingSink::new());
        let _ = sink.push_batch(&[entry(5, 5, ""), entry(100, 5, "a")]);
        sink.warn(ScanWarning::SynthesizedRoot);

        let (a, b) = sink.into_parts();
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 2);
        assert_eq!(a.warnings(), b.warnings());
    }

    #[test]
    fn tee_breaks_when_either_side_does() {
        let mut sink = TeeSink::new(RecordingSink::new(), RecordingSink::new().stop_after(1));
        let flow = sink.push_batch(&[entry(5, 5, ""), entry(100, 5, "a")]);

        assert_eq!(flow, ControlFlow::Break(()));
        // The unlimited side still saw the whole batch.
        assert_eq!(sink.first().len(), 2);
    }

    #[test]
    fn a_sink_reference_is_itself_a_sink() {
        // So a scanner taking `impl EntrySink` can be handed `&mut sink` and
        // the caller keeps ownership.
        fn feed(mut sink: impl EntrySink) {
            let _ = sink.push_batch(&[entry(5, 5, "")]);
        }

        let mut sink = RecordingSink::new();
        feed(&mut sink);
        assert_eq!(sink.len(), 1);
    }
}
