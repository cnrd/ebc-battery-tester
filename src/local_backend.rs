//! Local physical-test backend shared by native serial and browser `WebUSB` runners.

use crate::backend::{BackendEvent, BackendState, DiagnosticDirection, DiagnosticEvent};
use crate::controller::{
    CommandKind, ControllerMode, DeviceReport, PreparedCommand, ReportState, TestController,
};
use crate::core::{
    ApiCommand, AuthoritativeSnapshot, Sample, ServerConnectionState, SnapshotUpdate,
    TestConfiguration,
};
use crate::device::{self, InboundFrame, OutboundFrame};

#[derive(Default)]
pub(crate) struct LocalOutput {
    pub sends: Vec<LocalSend>,
    pub events: Vec<BackendEvent>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LocalSend {
    frame: OutboundFrame,
    completion: SendCompletion,
}

impl LocalSend {
    pub(crate) fn frame(self) -> OutboundFrame {
        self.frame
    }
}

#[derive(Clone, Copy, Debug)]
enum SendCompletion {
    Command(PreparedCommand),
    TimerSync,
    BestEffort,
}

pub(crate) struct LocalBackend {
    controller: TestController,
    connection: ServerConnectionState,
    connection_error: Option<String>,
    next_sequence: u64,
    last_published_elapsed: u64,
    shutdown_started: bool,
}

impl Default for LocalBackend {
    fn default() -> Self {
        Self {
            controller: TestController::new(ControllerMode::Direct),
            connection: ServerConnectionState::Disconnected,
            connection_error: None,
            next_sequence: 0,
            last_published_elapsed: 0,
            shutdown_started: false,
        }
    }
}

impl LocalBackend {
    pub(crate) fn begin_connection(&mut self) -> LocalOutput {
        self.controller.begin_connection("connection changed");
        self.connection = ServerConnectionState::Connecting;
        self.connection_error = None;
        self.state_output()
    }

    pub(crate) fn connection_established(&mut self) -> LocalOutput {
        self.controller.connection_established();
        self.connection = ServerConnectionState::Connected;
        self.connection_error = None;
        self.state_output()
    }

    pub(crate) fn connection_failed(&mut self, error: String) -> LocalOutput {
        self.controller.disconnect("device connection lost");
        self.connection = ServerConnectionState::Error;
        self.connection_error = Some(error);
        self.state_output()
    }

    pub(crate) fn disconnected(&mut self) -> LocalOutput {
        self.controller.disconnect("device disconnected");
        self.connection = ServerConnectionState::Disconnected;
        self.connection_error = None;
        self.state_output()
    }

    pub(crate) fn command(&mut self, command: ApiCommand) -> LocalOutput {
        let prepared = match self.controller.prepare_command(command) {
            Ok(prepared) => prepared,
            Err(error) => return Self::command_error(error),
        };
        if let Some(frame) = prepared.frame() {
            return LocalOutput {
                sends: vec![LocalSend {
                    frame,
                    completion: SendCompletion::Command(prepared),
                }],
                ..LocalOutput::default()
            };
        }
        self.command_succeeded(prepared)
    }

    pub(crate) fn resume(&self, config: TestConfiguration) -> LocalOutput {
        let prepared = match self.controller.prepare_resume(config) {
            Ok(prepared) => prepared,
            Err(error) => return Self::command_error(error),
        };
        let Some(frame) = prepared.frame() else {
            return Self::command_error("resume did not produce a protocol frame".to_owned());
        };
        LocalOutput {
            sends: vec![LocalSend {
                frame,
                completion: SendCompletion::Command(prepared),
            }],
            ..LocalOutput::default()
        }
    }

    pub(crate) fn finish_send(
        &mut self,
        send: LocalSend,
        result: Result<(), String>,
    ) -> LocalOutput {
        match result {
            Ok(()) => match send.completion {
                SendCompletion::Command(prepared) => self.command_succeeded(prepared),
                SendCompletion::TimerSync | SendCompletion::BestEffort => LocalOutput::default(),
            },
            Err(error) => self.send_failed(send, &error),
        }
    }

    fn command_succeeded(&mut self, prepared: PreparedCommand) -> LocalOutput {
        let mut output = LocalOutput::default();
        self.controller.commit_command(prepared, None);
        if prepared.kind() == CommandKind::Start {
            self.next_sequence = 0;
            output.events.push(BackendEvent::Snapshot(self.snapshot()));
        } else {
            output.events.push(BackendEvent::Update(self.state()));
        }
        output.events.push(BackendEvent::CommandSucceeded);
        output
    }

    pub(crate) fn safe_disconnect() -> LocalOutput {
        LocalOutput {
            sends: vec![
                LocalSend {
                    frame: OutboundFrame::Stop,
                    completion: SendCompletion::BestEffort,
                },
                LocalSend {
                    frame: OutboundFrame::Disconnect,
                    completion: SendCompletion::BestEffort,
                },
            ],
            ..LocalOutput::default()
        }
    }

    pub(crate) fn shutdown(&mut self) -> LocalOutput {
        if self.shutdown_started {
            return LocalOutput::default();
        }
        self.shutdown_started = true;
        Self::safe_disconnect()
    }

    pub(crate) fn tick(&mut self) -> LocalOutput {
        let mut output = LocalOutput::default();
        if let Some(minutes) = self.controller.next_timer_sync() {
            output.sends.push(LocalSend {
                frame: OutboundFrame::TimerSync(minutes),
                completion: SendCompletion::TimerSync,
            });
        }
        self.controller.update_elapsed();
        let elapsed = self.controller.test().elapsed_seconds;
        if elapsed != self.last_published_elapsed {
            self.last_published_elapsed = elapsed;
            output.events.push(BackendEvent::Update(self.state()));
        }
        output
    }

    pub(crate) fn frame(&mut self, frame: InboundFrame, raw_bytes: Vec<u8>) -> LocalOutput {
        let label = format!("{frame:?}");
        let mut output = match frame {
            InboundFrame::Firmware(report) => self.report(
                DeviceReport {
                    mode: report.device_mode,
                    state: if report.in_progress {
                        ReportState::Active
                    } else {
                        ReportState::InactiveUnknown
                    },
                    voltage_mv: report.voltage_mv,
                    current_ma: report.current_ma,
                    capacity_mah: report.milli_ampere_hours,
                    model: report.device_type,
                    firmware_version: Some(report.firmware_version),
                },
                false,
            ),
            InboundFrame::Charge(report) => self.report(
                DeviceReport {
                    mode: device::DeviceMode::ChargeConstantVoltage,
                    state: report.state.into(),
                    voltage_mv: report.voltage_mv,
                    current_ma: report.current_ma,
                    capacity_mah: report.milli_ampere_hours,
                    model: report.device_type,
                    firmware_version: None,
                },
                true,
            ),
            InboundFrame::DischargeConstantCurrent(report) => self.report(
                DeviceReport {
                    mode: device::DeviceMode::DischargeConstantCurrent,
                    state: report.state.into(),
                    voltage_mv: report.voltage_mv,
                    current_ma: report.current_ma,
                    capacity_mah: report.milli_ampere_hours,
                    model: report.device_type,
                    firmware_version: None,
                },
                true,
            ),
            InboundFrame::DischargeConstantPower(report) => self.report(
                DeviceReport {
                    mode: device::DeviceMode::DischargeConstantPower,
                    state: report.state.into(),
                    voltage_mv: report.voltage_mv,
                    current_ma: report.current_ma,
                    capacity_mah: report.milli_ampere_hours,
                    model: report.device_type,
                    firmware_version: None,
                },
                true,
            ),
        };
        output.events.insert(
            0,
            BackendEvent::Diagnostic(DiagnosticEvent {
                direction: DiagnosticDirection::In,
                label,
                raw_bytes,
            }),
        );
        output
    }

    fn report(&mut self, report: DeviceReport, sample_report: bool) -> LocalOutput {
        let (_, measurement) = self.controller.report(report);
        self.last_published_elapsed = self.controller.test().elapsed_seconds;
        let mut output = self.state_output();
        if sample_report && let Some(measurement) = measurement {
            output.events.push(BackendEvent::Sample(Sample {
                run_id: String::new(),
                sequence: self.next_sequence,
                timestamp_utc: String::new(),
                elapsed_seconds: measurement.elapsed_seconds,
                voltage_mv: measurement.voltage_mv,
                current_ma: measurement.current_ma,
                capacity_mah: measurement.capacity_mah,
                energy_wh: measurement.energy_wh,
                mode: measurement.mode,
            }));
            self.next_sequence = self.next_sequence.saturating_add(1);
        }
        output
    }

    fn state_output(&self) -> LocalOutput {
        LocalOutput {
            events: vec![BackendEvent::Update(self.state())],
            ..LocalOutput::default()
        }
    }

    fn state(&self) -> BackendState {
        BackendState {
            update: SnapshotUpdate {
                connection: self.connection.clone(),
                connection_error: self.connection_error.clone(),
                device: self.controller.device().clone(),
                test: self.controller.test().clone(),
                capabilities: self.controller.capabilities(),
            },
        }
    }

    fn snapshot(&self) -> AuthoritativeSnapshot {
        let state = self.state();
        AuthoritativeSnapshot {
            connection: state.update.connection,
            connection_error: state.update.connection_error,
            device: state.update.device,
            test: state.update.test,
            capabilities: state.update.capabilities,
            history: Vec::new(),
        }
    }

    fn command_error(error: String) -> LocalOutput {
        LocalOutput {
            events: vec![BackendEvent::CommandError(error)],
            ..LocalOutput::default()
        }
    }

    fn send_failed(&mut self, send: LocalSend, error: &str) -> LocalOutput {
        let message = format!("failed to send {:?}: {error}", send.frame);
        if !matches!(send.completion, SendCompletion::BestEffort) {
            match send.completion {
                SendCompletion::Command(prepared) => self
                    .controller
                    .command_write_failed(prepared.kind(), &message),
                SendCompletion::TimerSync => self.controller.disconnect(&message),
                SendCompletion::BestEffort => unreachable!(),
            }
            self.connection = ServerConnectionState::Error;
            self.connection_error = Some(message.clone());
            return LocalOutput {
                events: vec![
                    BackendEvent::Update(self.state()),
                    BackendEvent::CommandError(message),
                ],
                ..LocalOutput::default()
            };
        }
        Self::command_error(message)
    }

    #[cfg(test)]
    fn set_elapsed_for_test(&mut self, seconds: u64) {
        self.controller.set_elapsed_for_test(seconds);
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "backend tests should fail fast")]
mod tests {
    use super::*;
    use crate::core::TestState;
    use crate::device::DeviceMode;

    fn config() -> TestConfiguration {
        TestConfiguration::DischargeConstantCurrent {
            current_ma: 1000,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 0,
        }
    }

    fn report(state: ReportState, capacity_mah: u16) -> DeviceReport {
        DeviceReport {
            mode: DeviceMode::DischargeConstantCurrent,
            state,
            voltage_mv: 4000,
            current_ma: if state == ReportState::Active {
                1000
            } else {
                0
            },
            capacity_mah,
            model: "EBC-A20".to_owned(),
            firmware_version: None,
        }
    }

    fn connected_backend() -> LocalBackend {
        let mut backend = LocalBackend::default();
        backend.begin_connection();
        backend.connection_established();
        backend.report(report(ReportState::Idle, 0), true);
        backend
    }

    fn state(output: &LocalOutput) -> &BackendState {
        output
            .events
            .iter()
            .find_map(|event| match event {
                BackendEvent::Update(state) => Some(state),
                BackendEvent::Snapshot(snapshot) => {
                    panic!("expected update, got snapshot: {snapshot:?}")
                }
                _ => None,
            })
            .expect("semantic state event")
    }

    fn send_frames(output: &LocalOutput) -> Vec<OutboundFrame> {
        output.sends.iter().copied().map(LocalSend::frame).collect()
    }

    fn finish_success(backend: &mut LocalBackend, output: &LocalOutput) -> LocalOutput {
        assert_eq!(output.sends.len(), 1);
        backend.finish_send(output.sends[0], Ok(()))
    }

    fn finish_failure(backend: &mut LocalBackend, output: &LocalOutput) -> LocalOutput {
        assert_eq!(output.sends.len(), 1);
        backend.finish_send(output.sends[0], Err("injected write failure".to_owned()))
    }

    #[test]
    fn start_buffered_idle_active_stop_idle_is_semantic() {
        let mut backend = connected_backend();
        let request = backend.command(ApiCommand::Start(config()));
        assert!(matches!(
            send_frames(&request).as_slice(),
            [OutboundFrame::StartConstantCurrentDischarge(1000, 3000, 0)]
        ));
        let start = finish_success(&mut backend, &request);
        assert!(matches!(
            start.events.first(),
            Some(BackendEvent::Snapshot(snapshot))
                if snapshot.test.state == TestState::Starting
        ));

        let idle = backend.report(report(ReportState::Idle, 0), true);
        assert_eq!(state(&idle).update.test.state, TestState::Starting);
        assert!(
            !idle
                .events
                .iter()
                .any(|event| matches!(event, BackendEvent::Sample(_)))
        );

        let active = backend.report(report(ReportState::Active, 1), true);
        assert_eq!(state(&active).update.test.state, TestState::Running);
        assert!(
            active
                .events
                .iter()
                .any(|event| matches!(event, BackendEvent::Sample(_)))
        );

        let request = backend.command(ApiCommand::Stop);
        assert!(matches!(
            send_frames(&request).as_slice(),
            [OutboundFrame::Stop]
        ));
        let stop = finish_success(&mut backend, &request);
        assert_eq!(state(&stop).update.test.state, TestState::Stopping);
        let stopped = backend.report(report(ReportState::Idle, 1), true);
        assert_eq!(state(&stopped).update.test.state, TestState::Stopped);
    }

    #[test]
    fn timer_sync_is_backend_housekeeping_without_gui_polling() {
        let mut backend = connected_backend();
        let request = backend.command(ApiCommand::Start(config()));
        finish_success(&mut backend, &request);
        backend.report(report(ReportState::Active, 1), true);
        backend.set_elapsed_for_test(60);

        let tick = backend.tick();
        assert!(matches!(
            send_frames(&tick).as_slice(),
            [OutboundFrame::TimerSync(1)]
        ));
        assert!(backend.tick().sends.is_empty());
    }

    #[test]
    fn reports_are_processed_without_a_gui_event_loop() {
        let mut backend = connected_backend();
        let request = backend.command(ApiCommand::Start(config()));
        finish_success(&mut backend, &request);
        let output = backend.report(report(ReportState::Active, 5), true);

        assert_eq!(state(&output).update.test.state, TestState::Running);
        assert!(matches!(
            output.events.as_slice(),
            [BackendEvent::Update(_), BackendEvent::Sample(sample)]
                if sample.capacity_mah == 5
        ));
    }

    #[test]
    fn connection_gap_revokes_local_ownership() {
        let mut backend = connected_backend();
        let request = backend.command(ApiCommand::Start(config()));
        finish_success(&mut backend, &request);
        backend.report(report(ReportState::Active, 1), true);

        let output = backend.connection_failed("serial gap".to_owned());

        assert_eq!(
            state(&output).update.test.state,
            TestState::RecoveredUncertain
        );
        assert!(!state(&output).update.device.activity_known);
        assert!(backend.tick().sends.is_empty());
    }

    #[test]
    fn local_state_carries_controller_capabilities() {
        let mut backend = connected_backend();
        let output = backend.report(report(ReportState::Idle, 0), true);

        assert!(state(&output).update.capabilities.start);
        assert_eq!(
            state(&output).update.capabilities,
            backend.controller.capabilities()
        );
    }

    #[test]
    fn start_is_committed_only_after_send_and_failure_is_uncertain() {
        let mut backend = connected_backend();
        let request = backend.command(ApiCommand::Start(config()));

        assert_eq!(backend.controller.test().state, TestState::Idle);
        assert!(request.events.is_empty());

        let failed = finish_failure(&mut backend, &request);
        assert_eq!(
            state(&failed).update.test.state,
            TestState::RecoveredUncertain
        );
        assert!(
            state(&failed)
                .update
                .test
                .result
                .as_deref()
                .is_some_and(|reason| reason.contains("start outcome is unknown"))
        );
        assert!(!state(&failed).update.device.activity_known);
        assert!(matches!(
            failed.events.last(),
            Some(BackendEvent::CommandError(error)) if error.contains("injected write failure")
        ));
        assert!(
            !failed
                .events
                .iter()
                .any(|event| matches!(event, BackendEvent::CommandSucceeded))
        );
    }

    #[test]
    fn adjust_and_calibration_failures_do_not_commit_and_invalidate_transport_trust() {
        let commands = [
            ApiCommand::Adjust(TestConfiguration::DischargeConstantCurrent {
                current_ma: 1500,
                cutoff_voltage_mv: 3000,
                cutoff_time_min: 0,
            }),
            ApiCommand::Calibration(crate::core::CalibrationCommand::VoltageLow(4000)),
        ];

        for command in commands {
            let mut backend = connected_backend();
            if matches!(command, ApiCommand::Adjust(_)) {
                let start = backend.command(ApiCommand::Start(config()));
                finish_success(&mut backend, &start);
                backend.report(report(ReportState::Active, 1), true);
            }
            let request = backend.command(command);
            let failed = finish_failure(&mut backend, &request);

            assert_eq!(
                state(&failed).update.connection,
                ServerConnectionState::Error
            );
            assert!(!state(&failed).update.device.activity_known);
            if matches!(command, ApiCommand::Adjust(_)) {
                assert_eq!(state(&failed).update.test.config, Some(config()));
            }
            assert!(matches!(
                failed.events.last(),
                Some(BackendEvent::CommandError(_))
            ));
            assert!(
                !failed
                    .events
                    .iter()
                    .any(|event| matches!(event, BackendEvent::CommandSucceeded))
            );
        }
    }

    #[test]
    fn stop_write_failure_is_uncertain() {
        let mut backend = connected_backend();
        let start = backend.command(ApiCommand::Start(config()));
        finish_success(&mut backend, &start);
        backend.report(report(ReportState::Active, 1), true);

        let stop = backend.command(ApiCommand::Stop);
        let failed = finish_failure(&mut backend, &stop);

        assert_eq!(
            state(&failed).update.test.state,
            TestState::RecoveredUncertain
        );
        assert!(
            state(&failed)
                .update
                .test
                .result
                .as_deref()
                .is_some_and(|reason| reason.contains("stop outcome is unknown"))
        );
        assert!(!state(&failed).update.device.activity_known);
    }

    #[test]
    fn resume_write_failure_is_uncertain() {
        let mut backend = connected_backend();
        let start = backend.command(ApiCommand::Start(config()));
        finish_success(&mut backend, &start);
        backend.report(report(ReportState::Active, 1), true);
        let stop = backend.command(ApiCommand::Stop);
        finish_success(&mut backend, &stop);
        backend.report(report(ReportState::Idle, 1), true);

        let resume = backend.resume(config());
        let failed = finish_failure(&mut backend, &resume);
        assert_eq!(
            state(&failed).update.connection,
            ServerConnectionState::Error
        );
        assert_eq!(
            state(&failed).update.test.state,
            TestState::RecoveredUncertain
        );
        assert!(
            state(&failed)
                .update
                .test
                .result
                .as_deref()
                .is_some_and(|reason| reason.contains("resume outcome is unknown"))
        );
        assert!(!state(&failed).update.device.activity_known);
    }

    #[test]
    fn timer_sync_failure_revokes_freshness_and_ownership() {
        let mut backend = connected_backend();
        let start = backend.command(ApiCommand::Start(config()));
        finish_success(&mut backend, &start);
        backend.report(report(ReportState::Active, 1), true);
        backend.set_elapsed_for_test(60);
        let timer = backend.tick();
        let failed = finish_failure(&mut backend, &timer);

        assert_eq!(
            state(&failed).update.test.state,
            TestState::RecoveredUncertain
        );
        assert_eq!(
            state(&failed).update.connection,
            ServerConnectionState::Error
        );
        assert!(backend.tick().sends.is_empty());
    }

    #[test]
    fn successful_resume_still_enters_starting() {
        let mut backend = connected_backend();
        let start = backend.command(ApiCommand::Start(config()));
        finish_success(&mut backend, &start);
        backend.report(report(ReportState::Active, 1), true);
        let stop = backend.command(ApiCommand::Stop);
        finish_success(&mut backend, &stop);
        backend.report(report(ReportState::Idle, 1), true);

        let resume = backend.resume(config());
        let resumed = finish_success(&mut backend, &resume);

        assert_eq!(state(&resumed).update.test.state, TestState::Starting);
        assert!(matches!(
            resumed.events.last(),
            Some(BackendEvent::CommandSucceeded)
        ));
    }

    #[test]
    fn local_shutdown_sends_stop_then_disconnect_once() {
        let mut backend = connected_backend();
        assert!(matches!(
            send_frames(&backend.shutdown()).as_slice(),
            [OutboundFrame::Stop, OutboundFrame::Disconnect]
        ));
        assert!(backend.shutdown().sends.is_empty());
    }
}
