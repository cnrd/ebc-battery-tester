#![expect(clippy::expect_used, reason = "freshness regressions fail fast")]

use super::tests::{
    confirm_inactive, confirm_running, device_step, mock_actor, test_config, unnamed_cycle,
};
use super::*;
use crate::controller::REPORT_FRESHNESS_TIMEOUT;
use crate::core::CycleStep;

fn report(state: ReportState) -> DeviceReport {
    DeviceReport {
        mode: device::DeviceMode::DischargeConstantCurrent,
        state,
        voltage_mv: 4000,
        current_ma: if state == ReportState::Active {
            1000
        } else {
            0
        },
        capacity_mah: 0,
        model: "EBC-MOCK".to_owned(),
        firmware_version: None,
    }
}

#[test]
fn silent_rest_expires_before_same_tick_start_and_persists_once() {
    let (mut actor, directory) = mock_actor("silent-rest");
    confirm_inactive(&mut actor);
    actor
        .start_cycle(unnamed_cycle(CycleRecipe {
            steps: vec![
                CycleStep::Rest {
                    duration_seconds: REPORT_FRESHNESS_TIMEOUT.as_secs(),
                },
                device_step(),
            ],
            repeat_count: 1,
        }))
        .expect("cycle");
    let t0 = Instant::now();
    actor.record_device_report_at(&report(ReportState::Idle), true, t0);
    // Keep an open-like connection but provide no mock observations, mirroring
    // the real actor's serial TimedOut path without PTYs or wall-clock sleeps.
    actor.config.mock = false;
    actor.sent_frames.clear();
    let mut updates = actor.snapshot_tx.subscribe();
    actor.tick_at(t0 + REPORT_FRESHNESS_TIMEOUT);
    assert!(actor.sent_frames.is_empty());
    assert_eq!(actor.snapshot.connection, ServerConnectionState::Connected);
    assert!(!actor.snapshot.device.activity_known);
    assert_eq!(actor.snapshot.cycle.state, CycleState::Interrupted);
    assert_eq!(actor.snapshot.cycle.step_index, 0);
    assert_eq!(
        actor.snapshot.cycle.result.as_deref(),
        Some(REPORT_TIMEOUT_REASON)
    );
    assert!(matches!(updates.try_recv(), Ok(WebSocketEvent::Update(_))));
    assert!(updates.try_recv().is_err());
    let metadata = fs::read(directory.join("session.json")).expect("timeout metadata");
    let persisted: serde_json::Value = serde_json::from_slice(&metadata).expect("JSON");
    assert!(!String::from_utf8_lossy(&metadata).contains("freshness_deadline"));
    assert_eq!(persisted["cycle"]["state"], "interrupted");
    assert_eq!(persisted["device"]["activity_known"], false);
    assert_eq!(persisted["connection"], "connected");
    actor.tick_at(t0 + Duration::from_secs(65));
    assert!(actor.sent_frames.is_empty());
    assert!(updates.try_recv().is_err());
    assert_eq!(
        fs::read(directory.join("session.json")).expect("same metadata"),
        metadata
    );
    assert!(
        actor
            .execute_cycle_action(CycleAction::Start(test_config()))
            .is_err()
    );
    assert_eq!(
        actor.cycle.status().result.as_deref(),
        Some(REPORT_TIMEOUT_REASON)
    );
    assert!(actor.sent_frames.is_empty());
    actor.record_device_report_at(
        &report(ReportState::Idle),
        true,
        t0 + Duration::from_secs(66),
    );
    assert!(actor.snapshot.device.activity_known);
    assert_eq!(actor.snapshot.connection_error, None);
    actor.tick_at(t0 + Duration::from_secs(67));
    assert!(actor.sent_frames.is_empty());
    assert_eq!(actor.snapshot.cycle.state, CycleState::Interrupted);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn owned_run_timeout_suppresses_due_timer_sync_and_explicit_stop_still_writes() {
    let (mut actor, directory) = mock_actor("silent-owned");
    confirm_inactive(&mut actor);
    confirm_running(&mut actor);
    let t0 = Instant::now();
    actor.record_device_report_at(&report(ReportState::Active), true, t0);
    actor.config.mock = false;
    actor.sent_frames.clear();
    actor.tick_at(t0 + Duration::from_secs(65));
    assert!(actor.sent_frames.is_empty());
    assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
    assert!(!actor.snapshot.device.activity_known);
    actor.config.mock = true; // write recorder only, no synthetic input
    actor
        .handle_command(ApiCommand::Stop)
        .expect("uncertain Stop");
    assert!(matches!(
        actor.sent_frames.as_slice(),
        [OutboundFrame::Stop]
    ));
    assert_eq!(actor.snapshot.test.state, TestState::Stopping);
    actor
        .handle_command(ApiCommand::Disconnect)
        .expect("uncertain Disconnect");
    assert!(matches!(
        actor.sent_frames.as_slice(),
        [OutboundFrame::Stop, OutboundFrame::Disconnect]
    ));
    assert_eq!(
        actor.snapshot.connection,
        ServerConnectionState::Disconnected
    );
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn actor_commands_expire_old_observation_without_a_periodic_tick() {
    let (mut actor, directory) = mock_actor("silent-command");
    confirm_inactive(&mut actor);
    let old = Instant::now()
        .checked_sub(REPORT_FRESHNESS_TIMEOUT)
        .expect("old report time");
    actor.record_device_report_at(&report(ReportState::Idle), true, old);
    actor.sent_frames.clear();
    assert!(
        actor
            .handle_command(ApiCommand::Start(test_config()))
            .is_err()
    );
    assert!(
        actor
            .start_cycle(unnamed_cycle(CycleRecipe {
                steps: vec![device_step()],
                repeat_count: 1
            }))
            .is_err()
    );
    assert!(
        actor
            .handle_command(ApiCommand::Calibration(CalibrationCommand::VoltageLow(
                1000
            )))
            .is_err()
    );
    assert!(!actor.snapshot.device.activity_known);
    assert!(actor.sent_frames.is_empty());
    actor
        .handle_command(ApiCommand::Disconnect)
        .expect("idle stale Disconnect");
    assert!(matches!(
        actor.sent_frames.as_slice(),
        [OutboundFrame::Disconnect]
    ));
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn firmware_refreshes_observation_without_creating_telemetry_or_reclaiming_ownership() {
    let (mut actor, directory) = mock_actor("fresh-firmware");
    confirm_inactive(&mut actor);
    actor
        .start_cycle(unnamed_cycle(CycleRecipe {
            steps: vec![device_step()],
            repeat_count: 1,
        }))
        .expect("cycle");
    let t0 = Instant::now();
    let mut firmware = report(ReportState::Active);
    firmware.firmware_version = Some("3.0.2".to_owned());
    actor.record_device_report_at(&firmware, false, t0);
    assert!(actor.controller.is_running_owned());
    assert!(actor.snapshot.history.is_empty());
    assert!(actor.snapshot.cycle_history.is_empty());
    actor.record_device_report_at(&firmware, false, t0 + Duration::from_secs(9));
    assert!(!actor.expire_report_freshness(t0 + REPORT_FRESHNESS_TIMEOUT));
    assert!(actor.snapshot.device.activity_known);
    assert!(actor.expire_report_freshness(t0 + Duration::from_secs(19)));
    actor.record_device_report_at(&firmware, false, t0 + Duration::from_secs(20));
    assert!(actor.snapshot.device.activity_known);
    assert!(!actor.controller.is_running_owned());
    assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
    assert_eq!(actor.snapshot.cycle.state, CycleState::Interrupted);
    assert!(actor.snapshot.history.is_empty());
    assert!(actor.snapshot.cycle_history.is_empty());
    assert_eq!(actor.persistence.next_sequence, 0);
    assert_eq!(actor.persistence.next_cycle_sequence, 0);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn no_frame_repeated_stop_completion_retains_durable_interruption_and_real_retry() {
    let (mut actor, directory) = mock_actor("no-frame-stop-expiry");
    confirm_inactive(&mut actor);
    actor
        .start_cycle(unnamed_cycle(CycleRecipe {
            steps: vec![device_step()],
            repeat_count: 1,
        }))
        .expect("cycle");
    let t0 = Instant::now();
    actor.record_device_report_at(&report(ReportState::Active), true, t0);
    assert_eq!(actor.cycle.status().state, CycleState::RunningStep);
    actor.sent_frames.clear();
    actor.stop_cycle().expect("first written Stop");
    assert!(matches!(
        actor.sent_frames.as_slice(),
        [OutboundFrame::Stop]
    ));
    assert_eq!(actor.controller.test().state, TestState::Stopping);
    assert_eq!(actor.cycle.status().state, CycleState::Stopping);
    let boundary = t0 + REPORT_FRESHNESS_TIMEOUT;
    let before = t0 + REPORT_FRESHNESS_TIMEOUT.saturating_sub(Duration::from_nanos(1));
    let action = actor
        .cycle
        .stop(actor.controller.test())
        .expect("repeated Stop");
    let prepared = actor
        .controller
        .prepare_command_at(ApiCommand::Stop, before)
        .expect("fresh repeated Stop");
    assert!(prepared.frame().is_none());
    actor.send_command_frame(prepared).expect("no-frame send");
    assert!(
        actor
            .commit_written_command_at(prepared, None, boundary)
            .is_err()
    );
    actor
        .cycle
        .on_action_committed(&action, actor.controller.test());
    assert!(matches!(
        actor.sent_frames.as_slice(),
        [OutboundFrame::Stop]
    ));
    assert_eq!(actor.snapshot.cycle.state, CycleState::Interrupted);
    assert_eq!(
        actor.snapshot.cycle.result.as_deref(),
        Some(REPORT_TIMEOUT_REASON)
    );
    assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
    assert_eq!(
        actor.snapshot.test.result.as_deref(),
        Some(REPORT_TIMEOUT_REASON)
    );
    assert!(!actor.snapshot.device.activity_known);
    assert_eq!(actor.snapshot.connection, ServerConnectionState::Connected);
    let metadata = fs::read(directory.join("session.json")).expect("timeout metadata");
    let persisted: serde_json::Value = serde_json::from_slice(&metadata).expect("JSON");
    assert_eq!(persisted["cycle"]["state"], "interrupted");
    assert_eq!(persisted["test"]["state"], "recovered_uncertain");
    assert_eq!(persisted["device"]["activity_known"], false);
    assert!(
        actor
            .commit_written_command_at(prepared, None, boundary)
            .is_err()
    );
    assert_eq!(
        fs::read(directory.join("session.json")).expect("same metadata"),
        metadata
    );
    actor.config.mock = false; // no synthetic reports during later ticks
    actor.tick_at(boundary + Duration::from_secs(1));
    assert_eq!(actor.snapshot.cycle.state, CycleState::Interrupted);
    actor.config.mock = true; // explicit write recorder
    actor.stop_cycle().expect("new explicit safety Stop");
    assert!(matches!(
        actor.sent_frames.as_slice(),
        [OutboundFrame::Stop, OutboundFrame::Stop]
    ));
    assert!(!actor.snapshot.device.activity_known);
    assert_eq!(actor.snapshot.cycle.state, CycleState::Interrupted);
    actor.record_device_report_at(
        &report(ReportState::Idle),
        true,
        boundary + Duration::from_secs(2),
    );
    assert_eq!(actor.snapshot.cycle.state, CycleState::Interrupted);
    assert_eq!(
        actor.snapshot.cycle.result.as_deref(),
        Some(REPORT_TIMEOUT_REASON)
    );
    assert!(!actor.controller.is_running_owned());
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn written_stop_crossing_deadline_still_commits_without_restoring_cycle() {
    let (mut actor, directory) = mock_actor("written-stop-expiry");
    confirm_inactive(&mut actor);
    actor
        .start_cycle(unnamed_cycle(CycleRecipe {
            steps: vec![device_step()],
            repeat_count: 1,
        }))
        .expect("cycle");
    let t0 = Instant::now();
    actor.record_device_report_at(&report(ReportState::Active), true, t0);
    let action = actor.cycle.stop(actor.controller.test()).expect("Stop");
    let prepared = actor
        .controller
        .prepare_command_at(ApiCommand::Stop, t0)
        .expect("written Stop");
    actor.sent_frames.clear();
    actor.send_command_frame(prepared).expect("real Stop write");
    actor
        .commit_written_command_at(prepared, None, t0 + REPORT_FRESHNESS_TIMEOUT)
        .expect("late Stop completion");
    actor
        .cycle
        .on_action_committed(&action, actor.controller.test());
    actor.sync_controller_state();
    assert!(matches!(
        actor.sent_frames.as_slice(),
        [OutboundFrame::Stop]
    ));
    assert_eq!(actor.snapshot.test.state, TestState::Stopping);
    assert!(!actor.snapshot.device.activity_known);
    assert_eq!(actor.snapshot.cycle.state, CycleState::Interrupted);
    assert_eq!(
        actor.snapshot.cycle.result.as_deref(),
        Some(REPORT_TIMEOUT_REASON)
    );
    assert!(!actor.controller.is_running_owned());
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn mock_idle_reports_keep_observation_fresh() {
    let (mut actor, directory) = mock_actor("mock-idle-fresh");
    confirm_inactive(&mut actor);
    actor.last_mock_sample = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("earlier mock time");
    actor.tick_mock();
    assert!(actor.controller.capabilities().start);
    assert!(actor.snapshot.history.is_empty());
    fs::remove_dir_all(directory).expect("cleanup");
}
