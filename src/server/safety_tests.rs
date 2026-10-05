#![expect(clippy::expect_used, reason = "safety regressions fail fast")]

use super::tests::{
    confirm_inactive, confirm_running, device_step, fixture_machine_info, mock_actor, test_config,
    unnamed_cycle,
};
use super::*;
use crate::controller::REPORT_FRESHNESS_TIMEOUT;
use crate::core::CycleStep;

fn report(state: ReportState, current_ma: u16) -> DeviceReport {
    DeviceReport {
        mode: device::DeviceMode::DischargeConstantCurrent,
        state,
        voltage_mv: 4000,
        current_ma,
        capacity_mah: 1,
        model: "EBC-MOCK".to_owned(),
        firmware_version: None,
    }
}

fn cycle(steps: Vec<CycleStep>) -> StartCycleRequest {
    unnamed_cycle(CycleRecipe {
        steps,
        repeat_count: 1,
    })
}

#[test]
fn interrupted_cycle_does_not_suppress_retries_or_a_later_manual_stop() {
    let (mut actor, directory) = mock_actor("interrupted-manual-stop");
    confirm_inactive(&mut actor);
    actor
        .start_cycle(cycle(vec![device_step()]))
        .expect("cycle");
    let t0 = Instant::now();
    actor.record_device_report_at(&report(ReportState::Active, 1000), true, t0);
    actor.expire_report_freshness(t0 + REPORT_FRESHNESS_TIMEOUT);
    let interrupted = actor.cycle.status().clone();
    actor.sent_frames.clear();
    actor.handle_command(ApiCommand::Stop).expect("first Stop");
    actor.record_device_report_at(&report(ReportState::Active, 1000), true, t0);
    actor.expire_report_freshness(t0 + REPORT_FRESHNESS_TIMEOUT);
    actor.handle_command(ApiCommand::Stop).expect("retry Stop");
    assert!(matches!(
        actor.sent_frames.as_slice(),
        [OutboundFrame::Stop, OutboundFrame::Stop]
    ));
    actor.stop_cycle().expect("cycle-specific safety retry");
    assert!(matches!(
        actor.sent_frames.as_slice(),
        [
            OutboundFrame::Stop,
            OutboundFrame::Stop,
            OutboundFrame::Stop
        ]
    ));
    actor.record_device_report_at(&report(ReportState::Idle, 0), true, Instant::now());
    actor
        .handle_command(ApiCommand::Start(test_config()))
        .expect("manual Start");
    assert!(actor.persistence.current_run_cycle.is_none());
    actor.record_device_report_at(&report(ReportState::Active, 1000), true, Instant::now());
    actor.sent_frames.clear();
    actor.handle_command(ApiCommand::Stop).expect("manual Stop");
    assert!(matches!(
        actor.sent_frames.as_slice(),
        [OutboundFrame::Stop]
    ));
    assert_eq!(actor.controller.test().state, TestState::Stopping);
    assert_eq!(actor.cycle.status(), &interrupted);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[tokio::test]
async fn http_manual_start_cannot_prepare_history_or_write_during_rest_or_settling() {
    use std::future::IntoFuture as _;
    use tokio::io::AsyncWriteExt as _;

    for settling in [false, true] {
        let (mut actor, directory) = mock_actor("http-cycle-start-guard");
        confirm_inactive(&mut actor);
        if settling {
            actor
                .start_cycle(cycle(vec![device_step(), device_step()]))
                .expect("cycle");
            actor.record_device_report_at(&report(ReportState::Active, 1000), true, Instant::now());
            actor.record_device_report_at(
                &report(ReportState::Finished, 1000),
                true,
                Instant::now(),
            );
            assert_eq!(actor.cycle.status().state, CycleState::Settling);
        } else {
            actor
                .start_cycle(cycle(vec![
                    CycleStep::Rest {
                        duration_seconds: 30,
                    },
                    device_step(),
                ]))
                .expect("Rest");
        }
        actor.persist_and_publish().expect("persist");
        let before = actor.current_snapshot();
        let metadata = fs::read(&actor.persistence.metadata_path).expect("metadata");
        let csv = fs::read(&actor.persistence.samples_path).unwrap_or_default();
        let runs = actor.persistence.run_summaries();
        actor.sent_frames.clear();
        assert!(!before.capabilities.start);
        assert!(
            actor
                .handle_command(ApiCommand::Start(test_config()))
                .is_err()
        );
        let (actor_tx, actor_rx) = std_mpsc::channel();
        let worker = thread::spawn(move || {
            while let Ok(message) = actor_rx.recv() {
                actor.handle_message(message);
            }
            actor
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let router = Router::new()
            .nest("/api", api_router())
            .with_state(AppState {
                actor_tx,
                allowed_origin: None,
                machine_info: fixture_machine_info(),
            });
        let server = tokio::spawn(axum::serve(listener, router).into_future());
        let body = serde_json::to_string(&StartTestRequest {
            config: test_config(),
            name: Some("competing manual".to_owned()),
        })
        .expect("request");
        let mut stream = tokio::net::TcpStream::connect(address).await.expect("HTTP");
        stream.write_all(format!("POST /api/test/start HTTP/1.1\r\nHost: {address}\r\nX-EBC-Command: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.expect("POST");
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
            .await
            .expect("response deadline")
            .expect("response");
        let response = String::from_utf8(response).expect("HTTP text");
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert!(
            response.contains("the active cycle owns test orchestration"),
            "{response}"
        );
        server.abort();
        let _stopped = server.await;
        let mut actor = worker.join().expect("actor stopped");
        assert_eq!(actor.current_snapshot(), before);
        assert!(actor.sent_frames.is_empty());
        assert_eq!(actor.persistence.run_summaries(), runs);
        assert_eq!(
            fs::read(&actor.persistence.metadata_path).expect("metadata"),
            metadata
        );
        assert_eq!(
            fs::read(&actor.persistence.samples_path).unwrap_or_default(),
            csv
        );
        fs::remove_dir_all(directory).expect("cleanup");
    }
}

#[test]
fn stop_during_rest_cannot_ignore_an_unexpected_owned_physical_run() {
    let (mut actor, directory) = mock_actor("rest-defensive-stop");
    confirm_inactive(&mut actor);
    actor
        .start_cycle(cycle(vec![CycleStep::Rest {
            duration_seconds: 30,
        }]))
        .expect("Rest");
    // Model a contradictory backend state, without using the forbidden manual API.
    let prepared = actor
        .controller
        .prepare_command(ApiCommand::Start(test_config()))
        .expect("Start");
    actor.controller.commit_command(prepared, None);
    actor.controller.report(report(ReportState::Active, 1000));
    actor.sent_frames.clear();
    actor
        .handle_command(ApiCommand::Stop)
        .expect("defensive Stop");
    assert!(matches!(
        actor.sent_frames.as_slice(),
        [OutboundFrame::Stop]
    ));
    assert_eq!(actor.cycle.status().state, CycleState::Stopping);
    assert_eq!(actor.controller.test().state, TestState::Stopping);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn rest_stop_checks_live_activity_even_if_test_metadata_says_idle() {
    let (mut actor, directory) = mock_actor("rest-live-stop");
    confirm_inactive(&mut actor);
    actor
        .start_cycle(cycle(vec![CycleStep::Rest {
            duration_seconds: 30,
        }]))
        .expect("Rest");
    // Defensive contradiction: cached test metadata is not physical authority.
    let mut live = actor.controller.device().clone();
    live.active = true;
    live.current_ma = Some(1000);
    actor
        .controller
        .replace_authoritative(true, live, actor.controller.test().clone());
    actor.sent_frames.clear();
    actor
        .handle_command(ApiCommand::Stop)
        .expect("physical safety Stop");
    assert!(matches!(
        actor.sent_frames.as_slice(),
        [OutboundFrame::Stop]
    ));
    assert_eq!(actor.controller.test().state, TestState::Stopping);
    assert_eq!(actor.cycle.status().state, CycleState::Stopped);
    assert!(!actor.controller.is_running_owned());
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn silent_stop_success_failure_and_delayed_completion_keep_real_retries_available() {
    for fail in [false, true] {
        let (mut actor, directory) = mock_actor("silent-stop-retry");
        confirm_inactive(&mut actor);
        confirm_running(&mut actor);
        let t0 = Instant::now();
        actor.record_device_report_at(&report(ReportState::Active, 1000), true, t0);
        actor.expire_report_freshness(t0 + REPORT_FRESHNESS_TIMEOUT);
        actor.sent_frames.clear();
        if fail {
            actor.write_failure = Some("injected Stop failure".to_owned());
        }
        let result = actor.handle_command(ApiCommand::Stop);
        assert_eq!(result.is_err(), fail);
        if fail {
            assert_eq!(actor.snapshot.connection, ServerConnectionState::Error);
            assert!(actor.handle_command(ApiCommand::Stop).is_err());
            actor.connect().expect("reconnect without observations");
        }
        // No recovered report or report deadline is needed to retry.
        actor.config.mock = false;
        actor.tick_at(t0 + Duration::from_secs(100));
        actor.config.mock = true;
        assert!(actor.current_snapshot().capabilities.stop);
        assert!(!actor.controller.device().activity_known);
        let prepared = actor
            .controller
            .prepare_command(ApiCommand::Stop)
            .expect("retry");
        assert!(matches!(prepared.frame(), Some(OutboundFrame::Stop)));
        actor.send_command_frame(prepared).expect("retry write");
        actor
            .commit_written_command_at(prepared, None, t0 + Duration::from_secs(101))
            .expect("delayed completion");
        assert!(!actor.controller.device().activity_known);
        assert!(!actor.controller.is_running_owned());
        actor
            .handle_command(ApiCommand::Stop)
            .expect("another explicit retry");
        assert_eq!(
            actor
                .sent_frames
                .iter()
                .filter(|frame| matches!(frame, OutboundFrame::Stop))
                .count(),
            3
        );
        actor.record_device_report_at(
            &report(ReportState::Idle, 0),
            true,
            t0 + Duration::from_secs(102),
        );
        assert_eq!(actor.controller.test().state, TestState::Stopped);
        fs::remove_dir_all(directory).expect("cleanup");
    }
}

#[test]
fn still_active_report_after_stop_allows_real_retry_without_reclaiming_owned_metrics() {
    let (mut actor, directory) = mock_actor("active-stop-retry");
    confirm_inactive(&mut actor);
    actor
        .start_cycle(cycle(vec![device_step()]))
        .expect("cycle");
    actor.record_device_report_at(&report(ReportState::Active, 1000), true, Instant::now());
    actor.handle_command(ApiCommand::Stop).expect("Stop");
    actor.sent_frames.clear();
    actor
        .handle_command(ApiCommand::Stop)
        .expect("deduplicated Stop");
    assert!(actor.sent_frames.is_empty());
    let metrics = actor.controller.test().clone();
    actor.record_device_report_at(&report(ReportState::Active, 1000), true, Instant::now());
    assert!(!actor.controller.is_running_owned());
    assert_eq!(actor.controller.test().capacity_mah, metrics.capacity_mah);
    assert_eq!(actor.controller.test().energy_wh, metrics.energy_wh);
    assert!(actor.current_snapshot().capabilities.stop);
    actor.handle_command(ApiCommand::Stop).expect("real retry");
    assert!(matches!(
        actor.sent_frames.as_slice(),
        [OutboundFrame::Stop]
    ));
    assert_eq!(actor.cycle.status().state, CycleState::Stopping);
    fs::remove_dir_all(directory).expect("cleanup");
}

fn raw_report(command: u8, current_units: u8) -> Vec<u8> {
    let mut raw = vec![
        0xfa,
        command,
        0,
        current_units,
        0x10,
        0xa0,
        0,
        1,
        0,
        0,
        0,
        10,
        1,
        0x3c,
        0,
        0,
        9,
    ];
    raw.push(raw[1..].iter().fold(0, |acc, byte| acc ^ byte));
    raw.push(0xf8);
    raw
}

fn receive_bytes(actor: &mut DeviceActor, mut bytes: Vec<u8>) {
    for (frame, _) in device::process_buffer(&mut bytes) {
        actor.handle_frame(frame);
    }
}

#[test]
fn corrupt_reports_neither_settle_sample_nor_refresh_and_valid_stream_recovers() {
    let (mut actor, directory) = mock_actor("corrupt-settling");
    confirm_inactive(&mut actor);
    actor
        .start_cycle(cycle(vec![device_step(), device_step()]))
        .expect("cycle");
    receive_bytes(&mut actor, raw_report(0x0a, 8));
    receive_bytes(&mut actor, raw_report(0x14, 8));
    assert_eq!(actor.cycle.status().state, CycleState::Settling);
    actor.sent_frames.clear();
    let before = actor.current_snapshot();
    let csv = fs::read(
        actor
            .persistence
            .cycle_path(actor.cycle.status().execution_id.as_deref().expect("ID")),
    )
    .expect("cycle CSV");
    let mut corrupt = raw_report(0, 8);
    corrupt[3] ^= 8; // Exactly the audit's single-bit 80mA -> 0 corruption.
    receive_bytes(&mut actor, corrupt);
    for command in [0x64, 0x6e] {
        let mut corrupt = raw_report(command, 0);
        corrupt[17] ^= 1;
        receive_bytes(&mut actor, corrupt);
    }
    assert_eq!(actor.current_snapshot(), before);
    assert!(actor.sent_frames.is_empty());
    assert_eq!(
        fs::read(
            actor
                .persistence
                .cycle_path(actor.cycle.status().execution_id.as_deref().expect("ID"))
        )
        .expect("cycle CSV"),
        csv
    );
    receive_bytes(&mut actor, raw_report(0, 0));
    assert!(matches!(
        actor.sent_frames.as_slice(),
        [OutboundFrame::StartConstantCurrentDischarge(..)]
    ));
    assert_eq!(actor.cycle.status().step_index, 1);
    assert_eq!(
        actor
            .snapshot
            .cycle_history
            .last()
            .expect("boundary sample")
            .cycle_state,
        CycleState::Settling
    );
    // Corrupt firmware cannot grant initial authority or renew an old deadline.
    actor.controller.disconnect("test reset");
    actor.controller.connection_established();
    let mut corrupt = raw_report(0x64, 0);
    corrupt[17] ^= 1;
    receive_bytes(&mut actor, corrupt.clone());
    assert!(!actor.controller.device().activity_known);
    let old = Instant::now()
        .checked_sub(REPORT_FRESHNESS_TIMEOUT)
        .expect("old time");
    actor.record_device_report_at(&report(ReportState::Idle, 0), true, old);
    receive_bytes(&mut actor, corrupt);
    assert!(actor.expire_report_freshness(Instant::now()));
    assert!(!actor.controller.device().activity_known);
    fs::remove_dir_all(directory).expect("cleanup");
}
