//! The sink doubles, composed with the real builder.
//!
//! The unit tests in `model::sink` cover each double in isolation. These check
//! the thing that actually matters: that wrapping a real `IndexBuilder` in
//! them changes nothing about the index it produces, so a validated or
//! recorded run is the same run.

use std::ops::ControlFlow;

use emfit_core::model::builder::IndexBuilder;
use emfit_core::model::caps::VolumeCaps;
use emfit_core::model::entry::{EntryFlags, RawEntry, Times};
use emfit_core::model::sink::{
    EntrySink, RecordingSink, ScanWarning, SinkViolation, TeeSink, ValidatingSink,
};
use emfit_core::service::task::CancellationToken;

const ROOT_FS: u64 = 5;

fn caps() -> VolumeCaps {
    VolumeCaps {
        root_label: "C:".to_string(),
        path_separator: '\\',
        ..VolumeCaps::default()
    }
}

fn builder() -> IndexBuilder {
    IndexBuilder::new(caps(), CancellationToken::new())
}

fn dir(fs_id: u64, parent_id: u64, name: &str) -> RawEntry<'_> {
    RawEntry {
        fs_id,
        parent_id,
        name,
        size: 0,
        allocated: 0,
        times: Times::default(),
        flags: EntryFlags::DIRECTORY,
    }
}

fn file(fs_id: u64, parent_id: u64, name: &str, size: u64) -> RawEntry<'_> {
    RawEntry {
        fs_id,
        parent_id,
        name,
        size,
        allocated: size,
        times: Times::default(),
        flags: EntryFlags::empty(),
    }
}

/// A small tree, as a scanner would push it.
fn sample() -> Vec<RawEntry<'static>> {
    vec![
        dir(ROOT_FS, ROOT_FS, ""),
        dir(100, ROOT_FS, "docs"),
        file(200, 100, "a.txt", 1000),
        file(300, 100, "b.txt", 500),
        dir(400, ROOT_FS, "tmp"),
        file(500, 400, "log.txt", 25),
    ]
}

#[test]
fn validation_does_not_change_the_index() {
    let (plain, plain_warnings) = {
        let mut sink = builder();
        assert_eq!(sink.push_batch(&sample()), ControlFlow::Continue(()));
        sink.finish()
    };

    let (validated, validated_warnings) = {
        let mut sink = ValidatingSink::new(builder());
        assert_eq!(sink.push_batch(&sample()), ControlFlow::Continue(()));
        assert!(sink.is_valid(), "{:?}", sink.violations());
        sink.into_inner().finish()
    };

    assert_eq!(plain.len(), validated.len());
    assert_eq!(plain_warnings, validated_warnings);
    assert_eq!(
        plain.node(plain.root()).total_size(),
        validated.node(validated.root()).total_size()
    );
    for (a, b) in plain.ids().zip(validated.ids()) {
        assert_eq!(plain.path(a), validated.path(b));
    }
}

#[test]
fn a_recorded_run_replays_into_an_identical_index() {
    // The workflow that turns a bug on someone's disk into a portable test:
    // record a live scan, then rebuild from the recording anywhere.
    let mut recorder = RecordingSink::new();
    assert_eq!(recorder.push_batch(&sample()), ControlFlow::Continue(()));
    let recorded = recorder.into_entries();

    let (from_live, _) = {
        let mut sink = builder();
        assert_eq!(sink.push_batch(&sample()), ControlFlow::Continue(()));
        sink.finish()
    };

    let (from_recording, _) = {
        let replay: Vec<RawEntry<'_>> = recorded.iter().map(|e| e.as_raw()).collect();
        let mut sink = builder();
        assert_eq!(sink.push_batch(&replay), ControlFlow::Continue(()));
        sink.finish()
    };

    assert_eq!(from_live.len(), from_recording.len());
    for (a, b) in from_live.ids().zip(from_recording.ids()) {
        assert_eq!(from_live.path(a), from_recording.path(b));
        assert_eq!(
            from_live.node(a).total_size(),
            from_recording.node(b).total_size()
        );
    }
}

#[test]
fn tee_builds_and_records_in_one_pass() {
    let mut sink = TeeSink::new(builder(), RecordingSink::new());
    assert_eq!(sink.push_batch(&sample()), ControlFlow::Continue(()));

    let (build, record) = sink.into_parts();
    assert_eq!(record.len(), sample().len());

    let (index, _) = build.finish();
    assert_eq!(index.len(), sample().len());
    assert_eq!(index.node(index.root()).total_size(), 1525);
}

#[test]
fn validation_catches_a_scanner_pushing_paths_instead_of_names() {
    // The classic mistake: a walker that concatenates as it descends, so every
    // name arrives as a path. The index would build without complaint and
    // produce nonsense like `C:\docs\docs\a.txt`.
    let mut sink = ValidatingSink::new(builder());
    let _ = sink.push_batch(&[
        dir(ROOT_FS, ROOT_FS, ""),
        dir(100, ROOT_FS, "docs"),
        file(200, 100, r"docs\a.txt", 10),
    ]);

    assert_eq!(
        sink.violations(),
        &[SinkViolation::NameContainsSeparator {
            fs_id: 200,
            name: r"docs\a.txt".to_string(),
        }]
    );

    // The entry still reached the builder - validation observes, it does not
    // filter, so the index is exactly what an unvalidated run would produce.
    let (index, _) = sink.into_inner().finish();
    assert_eq!(index.len(), 3);
}

#[test]
fn validation_catches_dot_entries_a_walker_forgot_to_skip() {
    // FAT and ext4 store `.` and `..` literally in directory data. Pushed
    // through, `.` is a node parented to itself and `..` duplicates the
    // grandparent link.
    let mut sink = ValidatingSink::new(builder());
    let _ = sink.push_batch(&[
        dir(ROOT_FS, ROOT_FS, ""),
        dir(100, ROOT_FS, "docs"),
        dir(101, 100, "."),
        dir(102, 100, ".."),
    ]);

    assert_eq!(sink.violations().len(), 2);
    assert!(
        sink.violations()
            .iter()
            .all(|v| matches!(v, SinkViolation::DotEntry { .. }))
    );
}

#[test]
fn warnings_pass_through_every_wrapper() {
    let mut sink = TeeSink::new(ValidatingSink::new(builder()), RecordingSink::new());
    sink.warn(ScanWarning::UnreadableDirectory {
        fs_id: 100,
        reason: "access denied".to_string(),
    });
    let _ = sink.push_batch(&sample());

    let (validating, recording) = sink.into_parts();
    assert_eq!(recording.warnings().len(), 1);

    let (_, warnings) = validating.into_inner().finish();
    assert!(
        warnings.contains(&ScanWarning::UnreadableDirectory {
            fs_id: 100,
            reason: "access denied".to_string(),
        }),
        "the builder kept the scanner's warning: {warnings:?}"
    );
}

#[test]
fn a_stop_signal_from_a_recorder_still_yields_a_usable_index() {
    // Modelling cancellation: the scan halts partway, and whatever arrived
    // must still build into something coherent.
    let mut sink = TeeSink::new(builder(), RecordingSink::new().stop_after(3));
    assert_eq!(sink.push_batch(&sample()), ControlFlow::Break(()));

    let (build, _) = sink.into_parts();
    let (index, warnings) = build.finish();

    // The builder saw the whole batch; only the recorder cut off. The index is
    // complete and consistent either way.
    assert_eq!(index.len(), sample().len());
    assert!(warnings.is_empty(), "{warnings:?}");
}
