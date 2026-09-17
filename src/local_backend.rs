//! Local physical-test backend shared by native serial and browser `WebUSB` runners.

use crate::backend::{
    BackendCapabilities, BackendEvent, BackendState, DiagnosticDirection, DiagnosticEvent,
};
use crate::controller::{ControllerMode, DeviceReport, ReportState, TestController};
use crate::core::{
    ApiCommand, AuthoritativeSnapshot, Sample, ServerConnectionState, SnapshotUpdate,
    TestConfiguration,
};
use crate::device::{self, InboundFrame, OutboundFrame};

#[derive(Default)]
pub(crate) struct LocalOutput {
    pub frames: Vec<OutboundFrame>,
    pub events: Vec<BackendEvent>,
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
        let mut output = LocalOutput::default();
        if let Some(frame) = prepared.frame() {
            output.frames.push(frame);
        }
        self.controller.commit_command(prepared, None);
        if matches!(command, ApiCommand::Start(_)) {
            self.next_sequence = 0;
            output.events.push(BackendEvent::Snapshot {
                snapshot: self.snapshot(),
                capabilities: self.capabilities(),
            });
        } else {
            output.events.push(BackendEvent::Update(self.state()));
        }
        output.events.push(BackendEvent::CommandSucceeded);
        output
    }

    pub(crate) fn resume(&mut self, config: TestConfiguration) -> LocalOutput {
        let prepared = match self.controller.prepare_resume(config) {
            Ok(prepared) => prepared,
            Err(error) => return Self::command_error(error),
        };
        let mut output = LocalOutput::default();
        if let Some(frame) = prepared.frame() {
            output.frames.push(frame);
        }
        self.controller.commit_command(prepared, None);
        output.events.push(BackendEvent::Update(self.state()));
        output.events.push(BackendEvent::CommandSucceeded);
        output
    }

    pub(crate) fn safe_disconnect() -> LocalOutput {
        LocalOutput {
            frames: vec![OutboundFrame::Stop, OutboundFrame::Disconnect],
            ..LocalOutput::default()
        }
    }

    pub(crate) fn shutdown(&mut self) -> LocalOutput {
        if self.shutdown_started {
            return LocalOutput::default();
        }
        self.shutdown_started = true;
        LocalOutput {
            frames: vec![OutboundFrame::Stop, OutboundFrame::Disconnect],
            ..LocalOutput::default()
        }
    }

    pub(crate) fn tick(&mut self) -> LocalOutput {
        let mut output = LocalOutput::default();
        if let Some(minutes) = self.controller.next_timer_sync() {
            output.frames.push(OutboundFrame::TimerSync(minutes));
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
            },
            capabilities: self.capabilities(),
        }
    }

    fn snapshot(&self) -> AuthoritativeSnapshot {
        let state = self.state();
        AuthoritativeSnapshot {
            connection: state.update.connection,
            connection_error: state.update.connection_error,
            device: state.update.device,
            test: state.update.test,
            history: Vec::new(),
        }
    }

    fn capabilities(&self) -> BackendCapabilities {
        let capabilities = self.controller.capabilities();
        BackendCapabilities {
            start: capabilities.start,
            resume: capabilities.resume,
            stop: capabilities.stop,
            show_stop: capabilities.show_stop,
            adjust: capabilities.adjust,
            calibrate_voltage: capabilities.calibrate_voltage,
            calibrate_current: capabilities.calibrate_current,
            confirm_calibration: capabilities.confirm_calibration,
        }
    }

    fn command_error(error: String) -> LocalOutput {
        LocalOutput {
            events: vec![BackendEvent::CommandError(error)],
            ..LocalOutput::default()
        }
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
                BackendEvent::Snapshot { snapshot, .. } => {
                    panic!("expected update, got snapshot: {snapshot:?}")
                }
                _ => None,
            })
            .expect("semantic state event")
    }

    #[test]
    fn start_buffered_idle_active_stop_idle_is_semantic() {
        let mut backend = connected_backend();
        let start = backend.command(ApiCommand::Start(config()));
        assert!(matches!(
            start.frames.as_slice(),
            [OutboundFrame::StartConstantCurrentDischarge(1000, 3000, 0)]
        ));
        assert!(matches!(
            start.events.first(),
            Some(BackendEvent::Snapshot { snapshot, .. })
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

        let stop = backend.command(ApiCommand::Stop);
        assert!(matches!(stop.frames.as_slice(), [OutboundFrame::Stop]));
        assert_eq!(state(&stop).update.test.state, TestState::Stopping);
        let stopped = backend.report(report(ReportState::Idle, 1), true);
        assert_eq!(state(&stopped).update.test.state, TestState::Stopped);
    }

    #[test]
    fn timer_sync_is_backend_housekeeping_without_gui_polling() {
        let mut backend = connected_backend();
        backend.command(ApiCommand::Start(config()));
        backend.report(report(ReportState::Active, 1), true);
        backend.set_elapsed_for_test(60);

        let tick = backend.tick();
        assert!(matches!(
            tick.frames.as_slice(),
            [OutboundFrame::TimerSync(1)]
        ));
        assert!(backend.tick().frames.is_empty());
    }

    #[test]
    fn reports_are_processed_without_a_gui_event_loop() {
        let mut backend = connected_backend();
        backend.command(ApiCommand::Start(config()));
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
        backend.command(ApiCommand::Start(config()));
        backend.report(report(ReportState::Active, 1), true);

        let output = backend.connection_failed("serial gap".to_owned());

        assert_eq!(
            state(&output).update.test.state,
            TestState::RecoveredUncertain
        );
        assert!(!state(&output).update.device.activity_known);
        assert!(backend.tick().frames.is_empty());
    }

    #[test]
    fn local_shutdown_sends_stop_then_disconnect_once() {
        let mut backend = connected_backend();
        assert!(matches!(
            backend.shutdown().frames.as_slice(),
            [OutboundFrame::Stop, OutboundFrame::Disconnect]
        ));
        assert!(backend.shutdown().frames.is_empty());
    }
}
