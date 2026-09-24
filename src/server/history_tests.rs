#![expect(clippy::expect_used, reason = "history regression fixtures fail fast")]
use super::tests::{
    confirm_inactive, confirm_running, cycle_recipe, device_step, mock_actor,
    numbered_cycle_sample, numbered_sample, read_export, test_config, unnamed_cycle,
};
use super::*;
use std::thread;

fn history_report(actor: &mut DeviceActor, state: ReportState, current: u16) {
    actor.record_report(
        device::DeviceMode::DischargeConstantCurrent,
        3900,
        current,
        12,
        state,
        "EBC-MOCK",
        None,
    );
}

fn restart_history_actor(actor: DeviceActor) -> DeviceActor {
    let config = actor.config.clone();
    drop(actor);
    let (sender, _) = broadcast::channel(16);
    DeviceActor::new(config, sender).expect("restart history actor")
}

#[test]
fn manual_terminal_history_is_immediate_idempotent_and_keeps_live_samples() {
    let (mut actor, directory) = mock_actor("immediate-history");
    confirm_inactive(&mut actor);
    confirm_running(&mut actor);
    let id = actor.persistence.current_run_id.clone();
    let samples = actor.snapshot.history.clone();
    actor.sent_frames.clear();
    history_report(&mut actor, ReportState::Finished, 1000);
    assert!(actor.sent_frames.is_empty());
    assert_eq!(actor.snapshot.history, samples);
    assert_eq!(actor.snapshot.current_run.id.as_deref(), Some(id.as_str()));
    let summary = actor
        .persistence
        .run_summaries()
        .pop()
        .expect("immediate archive");
    assert_eq!(summary.state, TestState::Completed);
    assert_eq!(summary.id, id);
    let csv_path = actor.persistence.runs_dir.join(format!("{id}.csv"));
    let csv = fs::read(&csv_path).expect("original CSV");
    actor
        .rename_run(
            &id,
            RenameRequest {
                name: Some("still current".to_owned()),
            },
        )
        .expect("rename current archive");
    assert_eq!(
        actor.persistence.run_summaries()[0].name.as_deref(),
        Some("still current")
    );
    history_report(&mut actor, ReportState::Idle, 0);
    actor
        .start_test(test_config(), None, None)
        .expect("next run");
    assert_eq!(actor.persistence.run_summaries().len(), 1);
    actor.shutdown().expect("flush");
    let actor = restart_history_actor(actor);
    let history = actor
        .persistence
        .run_history(&id)
        .expect("history")
        .expect("exists");
    assert_eq!(history.summary.name.as_deref(), Some("still current"));
    assert_eq!(history.samples, samples);
    assert_eq!(fs::read(csv_path).expect("unchanged CSV"), csv);
    assert!(actor.sent_frames.is_empty());
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn stop_archive_waits_for_confirmation_and_resume_updates_same_archive() {
    let (mut actor, directory) = mock_actor("stop-history");
    confirm_inactive(&mut actor);
    confirm_running(&mut actor);
    let id = actor.persistence.current_run_id.clone();
    actor.handle_command(ApiCommand::Stop).expect("stop");
    assert!(actor.persistence.runs.is_empty());
    actor.sent_frames.clear();
    history_report(&mut actor, ReportState::Idle, 0);
    assert_eq!(actor.persistence.runs[0].state, TestState::Stopped);
    assert!(actor.sent_frames.is_empty());
    actor
        .handle_command(ApiCommand::Resume)
        .expect("resume stopped run");
    history_report(&mut actor, ReportState::Active, 1000);
    history_report(&mut actor, ReportState::Finished, 1000);
    assert_eq!(actor.persistence.runs.len(), 1);
    assert_eq!(actor.persistence.runs[0].id, id);
    assert_eq!(actor.persistence.runs[0].state, TestState::Completed);
    assert_eq!(
        actor.persistence.runs[0].sample_count,
        actor.persistence.raw_sample_count
    );
    assert_eq!(
        actor
            .persistence
            .run_history(&id)
            .expect("history")
            .expect("exists")
            .samples
            .len(),
        2
    );
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn recovered_uncertainty_does_not_create_a_terminal_archive() {
    let (mut actor, directory) = mock_actor("uncertain-history");
    confirm_inactive(&mut actor);
    confirm_running(&mut actor);
    actor.set_connection_error("observation gap");
    assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
    assert!(actor.persistence.runs.is_empty());
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "complete persisted multi-step execution and restart regression"
)]
fn cycle_history_preserves_terminal_duration_provenance_children_and_csv() {
    let (mut actor, directory) = mock_actor("cycle-history-complete");
    confirm_inactive(&mut actor);
    let recipe = cycle_recipe(
        vec![
            device_step(),
            crate::core::CycleStep::Rest {
                duration_seconds: 1,
            },
            device_step(),
        ],
        2,
    );
    let saved = actor
        .create_saved_recipe(CreateSavedRecipeRequest {
            name: "Template".to_owned(),
            recipe: recipe.clone(),
        })
        .expect("save recipe");
    actor
        .start_saved_recipe(
            &saved.id,
            StartSavedRecipeRequest {
                execution_name: Some("Execution".to_owned()),
            },
        )
        .expect("start saved");
    let id = actor
        .cycle
        .status()
        .execution_id
        .clone()
        .expect("execution ID");
    let active = actor
        .cycle_summaries()
        .expect("current history")
        .pop()
        .expect("summary");
    assert_eq!(active.state, Some(CycleState::StartingStep));
    assert_eq!(active.sample_count, 0);
    for index in 0..4 {
        history_report(&mut actor, ReportState::Active, 1000);
        // Reports inside a state do not rewrite the sidecar.
        let sidecar = fs::read(actor.persistence.cycle_metadata_path(&id)).expect("sidecar");
        history_report(&mut actor, ReportState::Active, 1000);
        assert_eq!(
            fs::read(actor.persistence.cycle_metadata_path(&id)).expect("same sidecar"),
            sidecar
        );
        history_report(&mut actor, ReportState::Finished, 1000);
        assert_eq!(actor.cycle.status().state, CycleState::Settling);
        history_report(&mut actor, ReportState::Idle, 0);
        if index % 2 == 0 {
            assert_eq!(actor.cycle.status().state, CycleState::Resting);
            history_report(&mut actor, ReportState::Idle, 0);
            let action = actor
                .cycle
                .tick(Instant::now() + Duration::from_secs(1))
                .expect("rest advances");
            actor
                .execute_cycle_action(action)
                .expect("next physical step");
        }
    }
    assert_eq!(actor.cycle.status().state, CycleState::Completed);
    let history = actor.cycle_history(&id).expect("history").expect("exists");
    assert_eq!(history.summary.state, Some(CycleState::Completed));
    assert_eq!(history.summary.result, actor.cycle.status().result);
    assert_eq!(history.summary.recipe, Some(recipe));
    let reference = history.summary.saved_recipe.as_ref().expect("provenance");
    assert_eq!(
        (&reference.id, &reference.name, reference.revision),
        (&saved.id, &saved.name, 1)
    );
    assert_eq!(history.summary.sample_count, 18);
    assert_eq!(history.summary.child_run_count, 4);
    let positions: Vec<_> = history
        .child_runs
        .iter()
        .map(|run| {
            assert!(run.name.is_none());
            let context = run.cycle.as_ref().expect("child context");
            assert_eq!(context.execution_id, id);
            (context.repeat_index, context.step_index)
        })
        .collect();
    assert_eq!(positions, vec![(0, 0), (0, 2), (1, 0), (1, 2)]);
    let terminal = actor
        .persistence
        .load_cycle_metadata(&id)
        .expect("sidecar")
        .expect("exists");
    assert_eq!(terminal.state, history.summary.state);
    assert_eq!(terminal.result, history.summary.result);
    assert_eq!(terminal.sample_count, Some(18));
    assert_eq!(
        terminal.elapsed_milliseconds,
        history.summary.elapsed_milliseconds
    );
    thread::sleep(Duration::from_millis(25));
    actor.current_snapshot();
    assert_eq!(
        actor
            .cycle_history(&id)
            .expect("later")
            .expect("exists")
            .summary
            .elapsed_milliseconds,
        terminal.elapsed_milliseconds
    );
    let csv_path = actor.persistence.cycle_path(&id);
    let csv = fs::read(&csv_path).expect("CSV");
    actor.sent_frames.clear();
    actor
        .rename_cycle(
            &id,
            RenameRequest {
                name: Some("Renamed history".to_owned()),
            },
        )
        .expect("rename");
    let mut renamed = actor
        .persistence
        .load_cycle_metadata(&id)
        .expect("renamed sidecar")
        .expect("exists");
    assert_eq!(renamed.name.as_deref(), Some("Renamed history"));
    renamed.name = terminal.name.clone();
    assert_eq!(renamed, terminal);
    for child in &history.child_runs {
        assert!(matches!(
            actor.rename_run(&child.id, RenameRequest::default()),
            Err(RenameError::BadRequest(_))
        ));
        assert_eq!(
            actor
                .persistence
                .run_history(&child.id)
                .expect("child detail")
                .expect("exists")
                .summary,
            *child
        );
    }
    assert_eq!(
        read_export(actor.persistence.cycle_export(Some(&id)).expect("export")).as_bytes(),
        csv
    );
    assert!(actor.sent_frames.is_empty());
    actor.shutdown().expect("flush");
    let mut actor = restart_history_actor(actor);
    let restarted = actor
        .cycle_history(&id)
        .expect("restart history")
        .expect("exists");
    assert_eq!(
        restarted.summary.elapsed_milliseconds,
        terminal.elapsed_milliseconds
    );
    assert_eq!(restarted.summary.state, Some(CycleState::Completed));
    assert_eq!(restarted.summary.name.as_deref(), Some("Renamed history"));
    assert_eq!(restarted.samples, history.samples);
    assert_eq!(fs::read(csv_path).expect("unchanged CSV"), csv);
    assert!(actor.sent_frames.is_empty());
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn interrupted_history_persists_gap_and_restart_reasons_and_durable_counts() {
    for gap in [false, true] {
        let (mut actor, directory) = mock_actor(if gap {
            "history-gap"
        } else {
            "history-restart"
        });
        confirm_inactive(&mut actor);
        actor
            .start_cycle(unnamed_cycle(cycle_recipe(vec![device_step()], 1)))
            .expect("start");
        history_report(&mut actor, ReportState::Active, 1000);
        let id = actor.cycle.status().execution_id.clone().expect("id");
        actor
            .persistence
            .flush_cycle_samples()
            .expect("durable sample");
        if gap {
            actor.set_connection_error("test serial gap");
        }
        let mut actor = restart_history_actor(actor);
        let history = actor.cycle_history(&id).expect("history").expect("exists");
        assert_eq!(history.summary.state, Some(CycleState::Interrupted));
        assert!(
            history
                .summary
                .result
                .as_deref()
                .expect("reason")
                .contains(if gap {
                    "test serial gap"
                } else {
                    "process restart"
                })
        );
        assert_eq!(history.summary.sample_count, 1);
        assert!(
            history.summary.elapsed_milliseconds
                >= history
                    .samples
                    .last()
                    .map(|sample| sample.elapsed_milliseconds)
        );
        let sidecar = actor
            .persistence
            .load_cycle_metadata(&id)
            .expect("sidecar")
            .expect("exists");
        assert_eq!(sidecar.state, Some(CycleState::Interrupted));
        assert_eq!(sidecar.sample_count, Some(1));
        assert!(actor.sent_frames.is_empty());
        fs::remove_dir_all(directory).expect("cleanup");
    }
}

fn legacy_cycle_fixture(actor: &DeviceActor, id: &str, count: u64) -> Vec<CycleSample> {
    let samples: Vec<_> = (0..count)
        .map(|sequence| numbered_cycle_sample(id, sequence))
        .collect();
    let mut csv = cycle_csv_header().to_owned();
    for sample in &samples {
        csv.push_str(&cycle_sample_csv_row(sample));
    }
    fs::write(actor.persistence.cycle_path(id), csv).expect("legacy CSV");
    samples
}

#[test]
fn legacy_cycles_missing_minimal_and_enriched_sidecars_are_discovered_and_sorted() {
    let (mut actor, directory) = mock_actor("legacy-history-list");
    for id in ["legacy-a", "legacy-z", "minimal", "enriched"] {
        legacy_cycle_fixture(&actor, id, 3);
    }
    fs::write(
        actor.persistence.cycle_metadata_path("minimal"),
        r#"{"execution_id":"minimal","name":"Old name"}"#,
    )
    .expect("minimal sidecar");
    actor
        .persistence
        .write_cycle_metadata(&CycleExecutionMetadata {
            execution_id: "enriched".to_owned(),
            started_at_utc: Some("2026-01-01T00:00:00Z".to_owned()),
            state: Some(CycleState::Completed),
            elapsed_milliseconds: Some(750),
            sample_count: Some(3),
            ..CycleExecutionMetadata::default()
        })
        .expect("enriched sidecar");
    let list = actor.cycle_summaries().expect("all histories");
    assert_eq!(
        list.iter()
            .map(|cycle| cycle.execution_id.as_str())
            .collect::<Vec<_>>(),
        vec!["enriched", "minimal", "legacy-z", "legacy-a"]
    );
    for cycle in &list[1..] {
        assert_eq!(cycle.state, None);
        assert_eq!(cycle.recipe, None);
        assert_eq!(cycle.saved_recipe, None);
        assert_eq!(cycle.started_at_utc, None);
        assert_eq!(cycle.elapsed_milliseconds, Some(500));
        assert_eq!(cycle.sample_count, 3);
    }
    assert_eq!(list[0].state, Some(CycleState::Completed));
    let mut actor = restart_history_actor(actor);
    assert_eq!(actor.cycle_summaries().expect("restart list"), list);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn history_corruption_errors_are_visible_and_torn_final_row_is_repaired() {
    let (mut actor, directory) = mock_actor("corrupt-history");
    legacy_cycle_fixture(&actor, "legacy", 2);
    let metadata_path = actor.persistence.cycle_metadata_path("legacy");
    fs::write(&metadata_path, "{").expect("bad JSON");
    assert!(actor.cycle_summaries().is_err());
    fs::write(&metadata_path, r#"{"execution_id":"wrong"}"#).expect("mismatched sidecar");
    assert!(
        actor
            .cycle_history("legacy")
            .expect_err("error")
            .contains("mismatch")
    );
    fs::remove_file(metadata_path).expect("remove sidecar");
    let path = actor.persistence.cycle_path("legacy");
    let original = fs::read(&path).expect("original");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open CSV");
    file.write_all(b"legacy,unfinished").expect("torn row");
    assert_eq!(
        actor
            .cycle_history("legacy")
            .expect("repair")
            .expect("exists")
            .summary
            .sample_count,
        2
    );
    assert_eq!(fs::read(&path).expect("repaired"), original);
    file.write_all(b"broken,complete\n")
        .expect("complete corruption");
    assert!(actor.cycle_history("legacy").is_err());
    fs::write(
        &path,
        format!(
            "{}{}",
            cycle_csv_header(),
            cycle_sample_csv_row(&numbered_cycle_sample("wrong", 0))
        ),
    )
    .expect("wrong telemetry ID");
    assert!(
        actor
            .cycle_history("legacy")
            .expect_err("error")
            .contains("mismatch")
    );
    fs::remove_dir_all(directory).expect("cleanup");
}

fn bounded_history_fixture(actor: &mut DeviceActor) -> (String, Vec<Sample>, Vec<CycleSample>) {
    confirm_inactive(actor);
    confirm_running(actor);
    let id = actor.persistence.current_run_id.clone();
    let mut samples = actor.snapshot.history.clone();
    for sequence in 1..10_001 {
        let mut sample = numbered_sample(
            sequence,
            if sequence == 333 { 9000 } else { 4000 },
            if sequence == 5555 { 6000 } else { 1000 },
        );
        sample.run_id.clone_from(&id);
        actor.persistence.append_sample(&sample).expect("sample");
        samples.push(sample);
    }
    history_report(actor, ReportState::Finished, 1000);
    let cycles = legacy_cycle_fixture(actor, "legacy-bounded", 10_001);
    (id, samples, cycles)
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "canonical HTTP resource flow with bounded JSON and byte-identical raw exports"
)]
async fn history_http_routes_are_bounded_read_only_and_device_independent() {
    use std::future::IntoFuture as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let (mut actor, directory) = mock_actor("history-http");
    let (run_id, raw_samples, raw_cycles) = bounded_history_fixture(&mut actor);
    let run_csv_path = actor.persistence.runs_dir.join(format!("{run_id}.csv"));
    let cycle_csv_path = actor.persistence.cycle_path("legacy-bounded");
    let run_csv = fs::read(&run_csv_path).expect("run CSV");
    let cycle_csv = fs::read(&cycle_csv_path).expect("cycle CSV");
    actor.set_connection_error("physical device absent");
    actor.sent_frames.clear();
    let (actor_tx, actor_rx) = std_mpsc::channel();
    let worker = thread::spawn(move || {
        while let Ok(message) = actor_rx.recv() {
            actor.handle_message(message);
        }
        actor
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("address");
    let router = Router::new()
        .nest("/api", api_router())
        .with_state(AppState {
            actor_tx,
            allowed_origin: None,
        });
    let server = tokio::spawn(axum::serve(listener, router).into_future());
    let mut paths = vec![
        ("/api/runs".to_owned(), 200),
        (format!("/api/runs/{run_id}"), 200),
        (format!("/api/runs/{run_id}/history.csv"), 200),
        ("/api/cycles".to_owned(), 200),
        ("/api/cycles/legacy-bounded".to_owned(), 200),
        ("/api/cycles/legacy-bounded/history.csv".to_owned(), 200),
    ];
    for resource in ["runs", "cycles"] {
        for suffix in ["", "/history.csv"] {
            paths.push((format!("/api/{resource}/unknown{suffix}"), 404));
            paths.push((format!("/api/{resource}/bad.id{suffix}"), 400));
        }
    }
    for (path, status) in paths {
        let mut stream = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect HTTP");
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .expect("GET");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.expect("response");
        let split = response
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .expect("headers");
        let headers = String::from_utf8_lossy(&response[..split]);
        assert!(
            headers.starts_with(&format!("HTTP/1.1 {status}")),
            "{path}: {headers}"
        );
        let body = &response[split + 4..];
        if path == format!("/api/runs/{run_id}") {
            let history: RunHistory = serde_json::from_slice(body).expect("run JSON");
            assert_eq!(history.summary.sample_count, raw_samples.len());
            assert_eq!(
                history.samples,
                presentation_history(&raw_samples, SNAPSHOT_SAMPLE_LIMIT)
            );
            assert!(history.samples.len() <= SNAPSHOT_SAMPLE_LIMIT);
        } else if path == "/api/cycles/legacy-bounded" {
            let history: CycleHistory = serde_json::from_slice(body).expect("cycle JSON");
            assert_eq!(history.summary.sample_count, raw_cycles.len());
            assert_eq!(
                history.samples,
                cycle_presentation_history(&raw_cycles, SNAPSHOT_SAMPLE_LIMIT)
            );
            assert!(history.samples.len() <= SNAPSHOT_SAMPLE_LIMIT);
        } else if status == 200 && path.ends_with("/history.csv") {
            assert_eq!(
                body,
                if path.contains("/runs/") {
                    &run_csv
                } else {
                    &cycle_csv
                }
            );
        }
    }
    server.abort();
    let _stopped = server.await;
    let actor = worker.join().expect("actor exits");
    assert!(actor.sent_frames.is_empty());
    assert_eq!(fs::read(run_csv_path).expect("unchanged run CSV"), run_csv);
    assert_eq!(
        fs::read(cycle_csv_path).expect("unchanged cycle CSV"),
        cycle_csv
    );
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn legacy_current_terminal_history_uses_telemetry_elapsed_and_actual_row_count() {
    let (mut actor, directory) = mock_actor("legacy-terminal-clock");
    let id = "legacy-terminal";
    legacy_cycle_fixture(&actor, id, 3);
    fs::write(
        actor.persistence.cycle_metadata_path(id),
        format!("{{\"execution_id\":\"{id}\"}}"),
    )
    .expect("old sidecar");
    actor.snapshot.cycle = CycleStatus {
        execution_id: Some(id.to_owned()),
        state: CycleState::Completed,
        ..CycleStatus::default()
    };
    actor
        .persistence
        .save_metadata(&actor.snapshot)
        .expect("old session");
    let mut actor = restart_history_actor(actor);
    // Synchronizing old terminal session metadata must not use a missing monotonic clock.
    actor.current_snapshot();
    let history = actor
        .cycle_history(id)
        .expect("legacy detail")
        .expect("exists");
    assert_eq!(history.summary.elapsed_milliseconds, Some(500));
    assert_eq!(history.summary.sample_count, 3);
    thread::sleep(Duration::from_millis(10));
    actor.current_snapshot();
    assert_eq!(
        actor
            .cycle_history(id)
            .expect("later")
            .expect("exists")
            .summary
            .elapsed_milliseconds,
        Some(500)
    );
    fs::remove_dir_all(directory).expect("cleanup");
}
