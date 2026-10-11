//! Summary revisions are independent of ordinary CSV rows and rounded elapsed time.
#![expect(clippy::expect_used, reason = "archive regression fixtures fail fast")]

use super::tests::{cycle_recipe, device_step, read_export, temporary_directory, test_config};
use super::*;

fn fixture(name: &str) -> (Persistence, AuthoritativeSnapshot, PathBuf) {
    let directory = temporary_directory(name);
    let mut persistence = Persistence::new(&directory).expect("persistence");
    persistence.begin_current_run("run-1".to_owned(), Some("renamed run".to_owned()), None);
    persistence.reset_samples().expect("empty ordinary CSV");
    let mut snapshot = AuthoritativeSnapshot::default();
    snapshot.test.config = Some(test_config());
    snapshot.test.started_at_utc = Some("2026-01-01T00:00:00Z".to_owned());
    snapshot.test.state = TestState::Stopped;
    snapshot.test.result = Some("stop confirmed by hardware".to_owned());
    snapshot.device.capacity_mah = Some(51); // Diagnostic, never an owned fallback.
    (persistence, snapshot, directory)
}

fn archive(persistence: &mut Persistence, snapshot: &AuthoritativeSnapshot) -> RunSummary {
    let summary = persistence
        .archive_current(snapshot)
        .expect("archive write")
        .expect("configured run");
    assert_eq!(persistence.run_summaries(), vec![summary.clone()]);
    assert_eq!(
        persistence
            .run_history(&summary.id)
            .expect("detail")
            .expect("exists")
            .summary,
        summary
    );
    let disk: RunSummary = serde_json::from_slice(
        &fs::read(persistence.runs_dir.join(format!("{}.json", summary.id))).expect("JSON"),
    )
    .expect("persisted summary");
    assert_eq!(disk, summary);
    summary
}

#[test]
fn archive_revision_distinguishes_unknown_zero_and_owned_capacity_without_rows() {
    let (mut persistence, mut snapshot, directory) = fixture("archive-options");
    let unknown = archive(&mut persistence, &snapshot);
    assert_eq!(unknown.capacity_mah, None);
    snapshot.test.capacity_mah = Some(0);
    let zero = archive(&mut persistence, &snapshot);
    assert_eq!(zero.capacity_mah, Some(0));
    snapshot.test.capacity_mah = Some(11);
    let nonzero = archive(&mut persistence, &snapshot);
    assert_eq!(nonzero.capacity_mah, Some(11));
    assert_eq!(unknown.id, nonzero.id);
    assert_eq!(nonzero.sample_count, 0);
    assert_eq!(nonzero.elapsed_seconds, unknown.elapsed_seconds);
    assert!(
        persistence
            .run_history(&nonzero.id)
            .expect("history")
            .expect("exists")
            .samples
            .is_empty()
    );
    drop(persistence);
    let restarted = Persistence::new(&directory).expect("restart");
    assert_eq!(restarted.run_summaries(), vec![nonzero]);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn archive_revision_refreshes_energy_only_including_sub_csv_precision() {
    let (mut persistence, mut snapshot, directory) = fixture("archive-energy");
    snapshot.test.capacity_mah = Some(10);
    snapshot.test.energy_wh = 0.125;
    let first = archive(&mut persistence, &snapshot);
    // Firmware endpoints can integrate energy without ordinary rows or a
    // capacity tick. Even less than CSV's nine decimal places is a JSON revision.
    snapshot.test.energy_wh = f64::from_bits(snapshot.test.energy_wh.to_bits() + 1);
    let updated = archive(&mut persistence, &snapshot);
    assert_ne!(updated.energy_wh, first.energy_wh);
    assert_eq!(updated.energy_wh, snapshot.test.energy_wh);
    assert_eq!(updated.capacity_mah, first.capacity_mah);
    assert_eq!(updated.sample_count, first.sample_count);
    assert_eq!(updated.elapsed_seconds, first.elapsed_seconds);
    assert_eq!(updated.state, first.state);
    assert_eq!(updated.result, first.result);
    assert_eq!(updated.id, first.id);
    drop(persistence);
    let mut restarted = Persistence::new(&directory).expect("restart");
    let recovered = restarted.load().expect("session");
    assert_eq!(archive(&mut restarted, &recovered), updated);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn archive_revision_refreshes_metadata_and_classification_without_metric_changes() {
    let (mut persistence, mut snapshot, directory) = fixture("archive-fields");
    snapshot.test.capacity_mah = Some(10);
    let original = archive(&mut persistence, &snapshot);
    snapshot.device.model = Some("EBC-A20".to_owned());
    let model = archive(&mut persistence, &snapshot);
    assert_eq!(model.model, snapshot.device.model);
    snapshot.device.firmware_version = Some("3.0.2".to_owned());
    let firmware = archive(&mut persistence, &snapshot);
    assert_eq!(firmware.firmware_version, snapshot.device.firmware_version);
    snapshot.test.result = Some("terminal reason resolved".to_owned());
    let reason = archive(&mut persistence, &snapshot);
    assert_eq!(reason.result, snapshot.test.result);
    snapshot.test.state = TestState::Completed;
    assert_eq!(
        archive(&mut persistence, &snapshot).state,
        TestState::Completed
    );
    snapshot.test.elapsed_seconds = 1;
    assert_eq!(archive(&mut persistence, &snapshot).elapsed_seconds, 1);
    // Same-run CC adjustment changes the configuration persisted at closing.
    snapshot.test.config = Some(TestConfiguration::DischargeConstantCurrent {
        current_ma: 200,
        cutoff_voltage_mv: 3000,
        cutoff_time_min: 0,
    });
    let adjusted = archive(&mut persistence, &snapshot);
    assert_eq!(adjusted.config, snapshot.test.config);
    assert_eq!(adjusted.started_at_utc, original.started_at_utc);
    assert_eq!(adjusted.name, original.name);
    assert_eq!(adjusted.id, original.id);
    assert_eq!(adjusted.capacity_mah, original.capacity_mah);
    assert_eq!(adjusted.sample_count, original.sample_count);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn archive_revision_does_not_skip_uncertain_continued_run_at_rollover() {
    let (mut persistence, mut snapshot, directory) = fixture("archive-uncertain");
    snapshot.test.capacity_mah = Some(10);
    let first = archive(&mut persistence, &snapshot);
    // Continue acquired new owned metrics, then lost authority before a second
    // Stop. The next explicit Start must retain that legitimate frozen outcome.
    snapshot.test.capacity_mah = Some(11);
    snapshot.test.state = TestState::RecoveredUncertain;
    snapshot.test.result = Some("physical report freshness expired".to_owned());
    let updated = archive(&mut persistence, &snapshot);
    assert_eq!(updated.capacity_mah, Some(11));
    assert_eq!(updated.state, TestState::RecoveredUncertain);
    assert_eq!(updated.id, first.id);
    assert_eq!(updated.sample_count, first.sample_count);
    persistence.begin_current_run("run-2".to_owned(), None, None);
    persistence.reset_samples().expect("rollover");
    assert_eq!(persistence.run_summaries(), vec![updated]);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn archive_revision_identical_rearchive_performs_no_writes() {
    let (mut persistence, mut snapshot, directory) = fixture("archive-idempotent");
    snapshot.test.capacity_mah = Some(0);
    snapshot.test.energy_wh = 0.125;
    let first = archive(&mut persistence, &snapshot);
    let json_path = persistence.runs_dir.join("run-1.json");
    let csv_path = persistence.runs_dir.join("run-1.csv");
    let metadata_path = persistence.metadata_path.clone();
    let paths = [&json_path, &csv_path, &metadata_path];
    let before: Vec<_> = paths
        .iter()
        .map(|path| fs::read(path).expect("before"))
        .collect();
    let modified: Vec<_> = paths
        .iter()
        .map(|path| {
            fs::metadata(path)
                .expect("metadata")
                .modified()
                .expect("mtime")
        })
        .collect();
    // A write would overwrite these temporary files, even on a coarse-mtime FS.
    for path in &paths {
        fs::write(
            path.with_extension(format!(
                "{}.tmp",
                path.extension().expect("extension").to_str().expect("text")
            )),
            b"untouched",
        )
        .expect("sentinel");
    }
    for _ in 0..3 {
        assert_eq!(archive(&mut persistence, &snapshot), first);
    }
    for ((path, bytes), time) in paths.iter().zip(before).zip(modified) {
        assert_eq!(fs::read(path).expect("after"), bytes);
        assert_eq!(
            fs::metadata(path)
                .expect("metadata")
                .modified()
                .expect("mtime"),
            time
        );
        assert_eq!(
            fs::read(path.with_extension(format!(
                "{}.tmp",
                path.extension().expect("extension").to_str().expect("text")
            )))
            .expect("sentinel retained"),
            b"untouched"
        );
    }
    assert_eq!(first.name.as_deref(), Some("renamed run"));
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn archive_revision_cycle_child_refresh_preserves_parent_sidecars_and_identity() {
    let (mut persistence, mut snapshot, directory) = fixture("archive-child");
    persistence
        .begin_cycle(
            "parent-1".to_owned(),
            Some("parent name".to_owned()),
            cycle_recipe(vec![device_step(), device_step()], 3),
            Some(SavedRecipeReference {
                id: "recipe-1".to_owned(),
                name: "recipe name".to_owned(),
                revision: 7,
            }),
            "2026-01-01T00:00:00Z".to_owned(),
        )
        .expect("parent sidecars");
    persistence.current_run_cycle = Some(CycleRunContext {
        execution_id: "parent-1".to_owned(),
        repeat_index: 2,
        step_index: 1,
    });
    snapshot.test.capacity_mah = Some(10);
    snapshot.test.state = TestState::Running;
    snapshot.test.result = None;
    let first = archive(&mut persistence, &snapshot);
    let parent_json = persistence.cycle_metadata_path("parent-1");
    let parent_csv = persistence.cycle_path("parent-1");
    let metadata = fs::read(&parent_json).expect("parent JSON");
    let telemetry = fs::read(&parent_csv).expect("parent CSV");
    // Persistence-level snapshots of compatible firmware endpoints: no cycle
    // Continue or fabricated rows. Energy can grow before the next counter tick.
    snapshot.test.energy_wh = 0.125;
    let energy = archive(&mut persistence, &snapshot);
    assert_eq!(energy.energy_wh, 0.125);
    assert_eq!(energy.id, first.id);
    assert_eq!(energy.cycle, first.cycle);
    assert_eq!(energy.result, first.result);
    assert!(energy.name.is_none());
    assert_eq!(energy.sample_count, 0);
    snapshot.test.capacity_mah = Some(11);
    let counter = archive(&mut persistence, &snapshot);
    assert_eq!(counter.capacity_mah, Some(11));
    snapshot.test.state = TestState::Completed;
    snapshot.test.result = Some("hardware completed test".to_owned());
    let closing = archive(&mut persistence, &snapshot);
    assert_eq!(closing.state, TestState::Completed);
    assert_eq!(closing.cycle, first.cycle);
    assert_eq!(persistence.run_summaries(), vec![closing.clone()]);
    assert_eq!(archive(&mut persistence, &snapshot), closing);
    persistence.begin_current_run("run-2".to_owned(), None, None);
    persistence.reset_samples().expect("next explicit run");
    drop(persistence);
    let restarted = Persistence::new(&directory).expect("restart");
    assert_eq!(restarted.run_summaries(), vec![closing]);
    assert_eq!(
        fs::read(parent_json).expect("parent metadata retained"),
        metadata
    );
    assert_eq!(
        fs::read(parent_csv).expect("parent telemetry retained"),
        telemetry
    );
    assert!(
        read_export(restarted.run_export("run-1").expect("export"))
            .lines()
            .count()
            == 1
    );
    fs::remove_dir_all(directory).expect("cleanup");
}
