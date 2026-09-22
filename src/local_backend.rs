//! Local physical-test backend shared by native serial and browser `WebUSB` runners.

use crate::backend::{BackendEvent, BackendState, DiagnosticDirection, DiagnosticEvent};
use crate::controller::{
    CommandKind, ControllerMode, DeviceReport, PreparedCommand, ReportState, TestController,
};
use crate::core::{
    ApiCommand, AuthoritativeSnapshot, CycleRecipe, CycleSample, CycleState, Sample,
    ServerConnectionState, SnapshotUpdate, TestConfiguration, cycle_presentation_history,
};
use crate::cycle::{CycleAction, CycleEngine};
use crate::device::{self, InboundFrame, OutboundFrame};
use std::time::Instant;

#[cfg(not(target_arch = "wasm32"))]
fn timestamp_utc() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(target_arch = "wasm32")]
fn timestamp_utc() -> String {
    js_sys::Date::new_0()
        .to_iso_string()
        .as_string()
        .unwrap_or_default()
}

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
    Command {
        prepared: PreparedCommand,
        cycle_action: Option<CycleAction>,
        notify_command: bool,
    },
    TimerSync,
    BestEffort,
}

pub(crate) struct LocalBackend {
    controller: TestController,
    cycle: CycleEngine,
    connection: ServerConnectionState,
    connection_error: Option<String>,
    next_sequence: u64,
    last_published_elapsed: u64,
    shutdown_started: bool,
    next_cycle_execution_id: u64,
    next_cycle_sequence: u64,
    cycle_history: Vec<CycleSample>,
}

impl Default for LocalBackend {
    fn default() -> Self {
        Self {
            controller: TestController::new(ControllerMode::Direct),
            cycle: CycleEngine::new(),
            connection: ServerConnectionState::Disconnected,
            connection_error: None,
            next_sequence: 0,
            last_published_elapsed: 0,
            shutdown_started: false,
            next_cycle_execution_id: 1,
            next_cycle_sequence: 0,
            cycle_history: Vec::new(),
        }
    }
}

impl LocalBackend {
    pub(crate) fn begin_connection(&mut self) -> LocalOutput {
        self.cycle
            .interrupt_for_gap("cycle interrupted by connection change");
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
        self.cycle
            .interrupt_for_gap(format!("cycle interrupted by connection failure: {error}"));
        self.controller.disconnect("device connection lost");
        self.connection = ServerConnectionState::Error;
        self.connection_error = Some(error);
        self.state_output()
    }

    pub(crate) fn disconnected(&mut self) -> LocalOutput {
        self.cycle
            .interrupt_for_gap("cycle interrupted because the device disconnected");
        self.controller.disconnect("device disconnected");
        self.connection = ServerConnectionState::Disconnected;
        self.connection_error = None;
        self.state_output()
    }

    pub(crate) fn command(&mut self, command: ApiCommand) -> LocalOutput {
        if command == ApiCommand::Stop
            && (self.cycle.owns_orchestration()
                || self.cycle.status().state == CycleState::Interrupted)
        {
            return self.stop_cycle();
        }
        if self.cycle.owns_orchestration()
            && matches!(
                command,
                ApiCommand::Start(_)
                    | ApiCommand::Resume
                    | ApiCommand::Adjust(_)
                    | ApiCommand::Calibration(_)
            )
        {
            return Self::command_error("the active cycle owns test orchestration".to_owned());
        }
        let prepared = match self.controller.prepare_command(command) {
            Ok(prepared) => prepared,
            Err(error) => return Self::command_error(error),
        };
        if let Some(frame) = prepared.frame() {
            return LocalOutput {
                sends: vec![LocalSend {
                    frame,
                    completion: SendCompletion::Command {
                        prepared,
                        cycle_action: None,
                        notify_command: true,
                    },
                }],
                ..LocalOutput::default()
            };
        }
        self.command_succeeded(prepared, None, true)
    }

    pub(crate) fn resume(&self, config: TestConfiguration) -> LocalOutput {
        if self.cycle.owns_orchestration() {
            return Self::command_error("the active cycle owns test orchestration".to_owned());
        }
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
                completion: SendCompletion::Command {
                    prepared,
                    cycle_action: None,
                    notify_command: true,
                },
            }],
            ..LocalOutput::default()
        }
    }

    pub(crate) fn start_cycle(&mut self, recipe: CycleRecipe) -> LocalOutput {
        if let Err(error) = recipe.validate() {
            return Self::command_error(error.to_string());
        }
        if self.cycle.is_executing() {
            return Self::command_error("a cycle is already active".to_owned());
        }
        if !self.controller.capabilities().start {
            return Self::command_error(
                "cycle start requires a fresh current-connection inactive report".to_owned(),
            );
        }
        if self.controller.device().current_ma != Some(0) {
            return Self::command_error(
                "cycle start requires confirmed zero device current".to_owned(),
            );
        }
        let Some(next_id) = self.next_cycle_execution_id.checked_add(1) else {
            return Self::command_error("local cycle execution IDs are exhausted".to_owned());
        };
        let execution_id = format!("local-cycle-{}", self.next_cycle_execution_id);
        self.next_cycle_execution_id = next_id;
        let action = match self.cycle.start(recipe, execution_id, None, Instant::now()) {
            Ok(action) => action,
            Err(error) => return Self::command_error(error.to_string()),
        };
        self.next_cycle_sequence = 0;
        self.cycle_history.clear();
        if let Some(action) = action {
            self.prepare_cycle_action(action, true)
        } else {
            let mut output = self.state_output();
            output.events.push(BackendEvent::CommandSucceeded);
            output
        }
    }

    pub(crate) fn stop_cycle(&mut self) -> LocalOutput {
        if let Some(action) = self.cycle.stop(self.controller.test()) {
            self.prepare_cycle_action(action, true)
        } else {
            let mut output = self.state_output();
            output.events.push(BackendEvent::CommandSucceeded);
            output
        }
    }

    pub(crate) fn finish_send(
        &mut self,
        send: LocalSend,
        result: Result<(), String>,
    ) -> LocalOutput {
        match result {
            Ok(()) => match send.completion {
                SendCompletion::Command {
                    prepared,
                    cycle_action,
                    notify_command,
                } => self.command_succeeded(prepared, cycle_action, notify_command),
                SendCompletion::TimerSync | SendCompletion::BestEffort => LocalOutput::default(),
            },
            Err(error) => self.send_failed(send, &error),
        }
    }

    fn command_succeeded(
        &mut self,
        prepared: PreparedCommand,
        cycle_action: Option<CycleAction>,
        notify_command: bool,
    ) -> LocalOutput {
        let mut output = LocalOutput::default();
        self.controller.commit_command(prepared, None);
        if let Some(action) = cycle_action {
            self.cycle
                .on_action_committed(&action, self.controller.test());
        }
        if prepared.kind() == CommandKind::Start {
            self.next_sequence = 0;
            output.events.push(BackendEvent::Snapshot(self.snapshot()));
        } else {
            output.events.push(BackendEvent::Update(self.state()));
        }
        if notify_command {
            output.events.push(BackendEvent::CommandSucceeded);
        }
        output
    }

    fn prepare_cycle_action(&mut self, action: CycleAction, notify_command: bool) -> LocalOutput {
        let command = match action {
            CycleAction::Start(config) => ApiCommand::Start(config),
            CycleAction::Stop => ApiCommand::Stop,
        };
        let prepared = match self.controller.prepare_command(command) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.cycle.on_action_failed(error.clone());
                let mut output = self.state_output();
                output.events.push(BackendEvent::CommandError(error));
                return output;
            }
        };
        if let Some(frame) = prepared.frame() {
            return LocalOutput {
                sends: vec![LocalSend {
                    frame,
                    completion: SendCompletion::Command {
                        prepared,
                        cycle_action: Some(action),
                        notify_command,
                    },
                }],
                events: vec![BackendEvent::Update(self.state())],
            };
        }
        self.command_succeeded(prepared, Some(action), notify_command)
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
        self.cycle
            .interrupt("cycle interrupted by local backend shutdown");
        let mut output = Self::safe_disconnect();
        output.events.push(BackendEvent::Update(self.state()));
        output
    }

    pub(crate) fn request_disconnect(&mut self) -> LocalOutput {
        self.cycle
            .interrupt("cycle interrupted by explicit device disconnect");
        let mut output = Self::safe_disconnect();
        output.events.push(BackendEvent::Update(self.state()));
        output
    }

    pub(crate) fn tick(&mut self) -> LocalOutput {
        let mut output = LocalOutput::default();
        let previous_cycle = self.cycle.status().clone();
        if let Some(minutes) = self.controller.next_timer_sync() {
            output.sends.push(LocalSend {
                frame: OutboundFrame::TimerSync(minutes),
                completion: SendCompletion::TimerSync,
            });
        }
        self.controller.update_elapsed();
        if let Some(action) = self.cycle.tick(Instant::now()) {
            output.extend(self.prepare_cycle_action(action, false));
        }
        let elapsed = self.controller.test().elapsed_seconds;
        if elapsed != self.last_published_elapsed || self.cycle.status() != &previous_cycle {
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
        let now = Instant::now();
        let mode = report.mode;
        let voltage_mv = report.voltage_mv;
        let current_ma = report.current_ma;
        let device_capacity_mah = report.capacity_mah;
        let (_, measurement) = self.controller.report(report);
        let cycle_sample = if sample_report && self.cycle.is_executing() {
            let status = self.cycle.status();
            status.execution_id.clone().map(|execution_id| CycleSample {
                execution_id,
                sequence: self.next_cycle_sequence,
                timestamp_utc: timestamp_utc(),
                elapsed_milliseconds: u64::try_from(self.cycle.elapsed(now).as_millis())
                    .unwrap_or(u64::MAX),
                repeat_index: status.repeat_index,
                step_index: status.step_index,
                cycle_state: status.state,
                test_state: self.controller.test().state.clone(),
                mode,
                activity_known: self.controller.device().activity_known,
                active: self.controller.device().active,
                voltage_mv,
                current_ma,
                device_capacity_mah,
                test_capacity_mah: self.controller.test().capacity_mah,
                test_energy_wh: self.controller.test().energy_wh,
            })
        } else {
            None
        };
        if let Some(sample) = &cycle_sample {
            self.cycle_history.push(sample.clone());
            if self.cycle_history.len() > 5_000 {
                self.cycle_history = cycle_presentation_history(&self.cycle_history, 4_000);
            }
            self.next_cycle_sequence = self.next_cycle_sequence.saturating_add(1);
        }
        let mut output = LocalOutput::default();
        if let Some(sample) = cycle_sample {
            output.events.push(BackendEvent::CycleSample(sample));
        }
        let cycle_action = self.cycle.on_physical_state(
            now,
            self.controller.device(),
            self.controller.test(),
            true,
        );
        self.last_published_elapsed = self.controller.test().elapsed_seconds;
        output.extend(self.state_output());
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
        if let Some(action) = cycle_action {
            output.extend(self.prepare_cycle_action(action, false));
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
        let mut capabilities = self.controller.capabilities();
        if self.cycle.owns_orchestration() {
            capabilities.start = false;
            capabilities.resume = false;
            capabilities.stop = true;
            capabilities.show_stop = true;
            capabilities.adjust = false;
            capabilities.calibrate_voltage = false;
            capabilities.calibrate_current = false;
            capabilities.confirm_calibration = false;
        }
        BackendState {
            update: SnapshotUpdate {
                connection: self.connection.clone(),
                connection_error: self.connection_error.clone(),
                device: self.controller.device().clone(),
                test: self.controller.test().clone(),
                cycle: self.cycle.status().clone(),
                capabilities,
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
            cycle: state.update.cycle,
            capabilities: state.update.capabilities,
            history: Vec::new(),
            cycle_history: self.cycle_history.clone(),
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
                SendCompletion::Command {
                    prepared,
                    cycle_action,
                    ..
                } => {
                    self.controller
                        .command_write_failed(prepared.kind(), &message);
                    if cycle_action.is_some() {
                        self.cycle.on_action_failed(message.clone());
                    } else {
                        self.cycle.interrupt_for_gap(message.clone());
                    }
                }
                SendCompletion::TimerSync => {
                    self.controller.disconnect(&message);
                    self.cycle.interrupt_for_gap(message.clone());
                }
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

impl LocalOutput {
    fn extend(&mut self, mut other: Self) {
        self.sends.append(&mut other.sends);
        self.events.append(&mut other.events);
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "backend tests should fail fast")]
mod tests {
    use super::*;
    use crate::core::{CycleState, CycleStep, CycleStepCompletion, TestState};
    use crate::device::DeviceMode;

    fn config() -> TestConfiguration {
        TestConfiguration::DischargeConstantCurrent {
            current_ma: 1000,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 0,
        }
    }

    fn config_with_current(current_ma: u16) -> TestConfiguration {
        TestConfiguration::DischargeConstantCurrent {
            current_ma,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 0,
        }
    }

    fn device_step(current_ma: u16) -> CycleStep {
        CycleStep::Device {
            config: config_with_current(current_ma),
            completion: CycleStepCompletion::Hardware,
        }
    }

    fn recipe(steps: Vec<CycleStep>) -> CycleRecipe {
        CycleRecipe {
            steps,
            repeat_count: 1,
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

    fn observed_report(
        state: ReportState,
        voltage_mv: u16,
        current_ma: u16,
        capacity_mah: u16,
    ) -> DeviceReport {
        DeviceReport {
            mode: DeviceMode::DischargeConstantCurrent,
            state,
            voltage_mv,
            current_ma,
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

    #[test]
    fn cycle_progresses_between_device_steps_without_gui_commands() {
        let mut backend = connected_backend();
        let first = backend.start_cycle(recipe(vec![device_step(1000), device_step(1500)]));
        assert!(matches!(
            send_frames(&first).as_slice(),
            [OutboundFrame::StartConstantCurrentDischarge(1000, 3000, 0)]
        ));
        finish_success(&mut backend, &first);
        backend.report(report(ReportState::Active, 1), true);

        let settling = backend.report(report(ReportState::Finished, 2), true);
        assert_eq!(state(&settling).update.cycle.state, CycleState::Settling);
        assert!(settling.sends.is_empty());
        let second = backend.report(report(ReportState::Idle, 2), true);
        assert!(matches!(
            send_frames(&second).as_slice(),
            [OutboundFrame::StartConstantCurrentDischarge(1500, 3000, 0)]
        ));

        let committed = finish_success(&mut backend, &second);
        assert!(matches!(
            committed.events.first(),
            Some(BackendEvent::Snapshot(snapshot))
                if snapshot.cycle.step_index == 1 && snapshot.test.elapsed_seconds == 0
        ));
    }

    #[test]
    fn cycle_samples_span_settling_rest_and_the_next_physical_run() {
        let mut backend = connected_backend();
        let first = backend.start_cycle(recipe(vec![
            device_step(1000),
            CycleStep::Rest {
                duration_seconds: 1,
            },
            device_step(1500),
        ]));
        finish_success(&mut backend, &first);

        backend.report(observed_report(ReportState::Active, 4000, 1000, 1), true);
        backend.report(observed_report(ReportState::Active, 3990, 1000, 2), true);
        backend.report(observed_report(ReportState::Finished, 3940, 1000, 3), true);
        backend.report(observed_report(ReportState::Idle, 3950, 1000, 3), true);
        let zero = backend.report(observed_report(ReportState::Idle, 3970, 0, 3), true);
        assert!(matches!(
            zero.events.as_slice(),
            [BackendEvent::CycleSample(_), BackendEvent::Update(_)]
        ));
        assert_eq!(state(&zero).update.cycle.state, CycleState::Resting);
        let rest = backend.report(observed_report(ReportState::Idle, 3980, 0, 3), true);
        assert!(rest.events.iter().any(|event| matches!(
            event,
            BackendEvent::CycleSample(sample)
                if sample.cycle_state == CycleState::Resting
                    && sample.step_index == 1
                    && sample.voltage_mv == 3980
                    && sample.current_ma == 0
        )));
        assert!(
            !rest
                .events
                .iter()
                .any(|event| matches!(event, BackendEvent::Sample(_)))
        );

        let before_firmware = backend.cycle_history.len();
        backend.report(
            observed_report(ReportState::InactiveUnknown, 3980, 0, 3),
            false,
        );
        assert_eq!(backend.cycle_history.len(), before_firmware);

        let action = backend
            .cycle
            .tick(Instant::now() + std::time::Duration::from_secs(2))
            .expect("rest advances");
        let second = backend.prepare_cycle_action(action, false);
        finish_success(&mut backend, &second);
        backend.report(observed_report(ReportState::Active, 3980, 1500, 1), true);

        assert_eq!(
            backend
                .cycle_history
                .iter()
                .map(|sample| sample.sequence)
                .collect::<Vec<_>>(),
            (0..7).collect::<Vec<_>>()
        );
        assert!(matches!(
            &backend.cycle_history[2],
            CycleSample {
                cycle_state: CycleState::RunningStep,
                test_state: TestState::Completed,
                current_ma: 1000,
                ..
            }
        ));
        assert!(matches!(
            &backend.cycle_history[3],
            CycleSample {
                cycle_state: CycleState::Settling,
                active: false,
                current_ma: 1000,
                ..
            }
        ));
        assert!(matches!(
            &backend.cycle_history[4],
            CycleSample {
                cycle_state: CycleState::Settling,
                active: false,
                current_ma: 0,
                step_index: 0,
                ..
            }
        ));
        assert_eq!(backend.cycle_history[6].step_index, 2);
        assert_eq!(backend.next_sequence, 1);
    }

    #[test]
    fn repeat_boundary_report_keeps_the_previous_cycle_context() {
        let mut backend = connected_backend();
        let first = backend.start_cycle(CycleRecipe {
            steps: vec![device_step(1000)],
            repeat_count: 2,
        });
        finish_success(&mut backend, &first);
        backend.report(observed_report(ReportState::Active, 4000, 1000, 1), true);
        backend.report(observed_report(ReportState::Finished, 3940, 1000, 2), true);
        backend.report(observed_report(ReportState::Idle, 3960, 1000, 2), true);
        let boundary = backend.report(observed_report(ReportState::Idle, 3980, 0, 2), true);
        assert!(matches!(
            send_frames(&boundary).as_slice(),
            [OutboundFrame::StartConstantCurrentDischarge(1000, 3000, 0)]
        ));
        let boundary_sample = backend.cycle_history.last().expect("boundary sample");
        assert_eq!(boundary_sample.repeat_index, 0);
        assert_eq!(boundary_sample.step_index, 0);
        assert_eq!(boundary_sample.cycle_state, CycleState::Settling);
        assert_eq!(boundary_sample.current_ma, 0);
        let boundary_sequence = boundary_sample.sequence;

        finish_success(&mut backend, &boundary);
        backend.report(observed_report(ReportState::Active, 3980, 1000, 1), true);
        let next = backend.cycle_history.last().expect("next repeat sample");
        assert_eq!(next.repeat_index, 1);
        assert_eq!(next.step_index, 0);
        assert_eq!(next.sequence, boundary_sequence + 1);
    }

    #[test]
    fn cycle_rejects_zero_duration_rest() {
        let mut backend = connected_backend();
        let started = backend.start_cycle(recipe(vec![CycleStep::Rest {
            duration_seconds: 0,
        }]));
        assert!(started.sends.is_empty());
        assert!(matches!(
            started.events.as_slice(),
            [BackendEvent::CommandError(error)] if error.contains("must be at least 1")
        ));
        assert_eq!(backend.cycle.status().state, CycleState::Idle);
    }

    #[test]
    fn connection_gap_interrupts_active_cycle() {
        let mut backend = connected_backend();
        let start = backend.start_cycle(recipe(vec![device_step(1000)]));
        finish_success(&mut backend, &start);
        backend.report(report(ReportState::Active, 1), true);

        let failed = backend.connection_failed("serial gap".to_owned());
        assert_eq!(state(&failed).update.cycle.state, CycleState::Interrupted);
        assert!(
            state(&failed)
                .update
                .cycle
                .result
                .as_deref()
                .is_some_and(|reason| reason.contains("connection failure"))
        );
    }

    #[test]
    fn cycle_rejects_manual_orchestration_commands() {
        let mut backend = connected_backend();
        let started = backend.start_cycle(recipe(vec![CycleStep::Rest {
            duration_seconds: 60,
        }]));
        assert!(started.sends.is_empty());

        for command in [
            ApiCommand::Start(config()),
            ApiCommand::Resume,
            ApiCommand::Adjust(config()),
            ApiCommand::Calibration(crate::core::CalibrationCommand::VoltageLow(4000)),
        ] {
            let rejected = backend.command(command);
            assert!(matches!(
                rejected.events.as_slice(),
                [BackendEvent::CommandError(error)] if error.contains("cycle owns")
            ));
            assert!(rejected.sends.is_empty());
        }
    }

    #[test]
    fn stopping_active_cycle_waits_for_inactive_report() {
        let mut backend = connected_backend();
        let start = backend.start_cycle(recipe(vec![device_step(1000)]));
        finish_success(&mut backend, &start);
        backend.report(report(ReportState::Active, 1), true);

        let stop = backend.stop_cycle();
        assert!(matches!(
            send_frames(&stop).as_slice(),
            [OutboundFrame::Stop]
        ));
        let stopping = finish_success(&mut backend, &stop);
        assert_eq!(state(&stopping).update.cycle.state, CycleState::Stopping);
        let stopped = backend.report(report(ReportState::Idle, 1), true);
        assert_eq!(state(&stopped).update.cycle.state, CycleState::Stopped);
    }

    #[test]
    fn cycle_start_write_failure_interrupts_without_commit() {
        let mut backend = connected_backend();
        let start = backend.start_cycle(recipe(vec![device_step(1000)]));

        let failed = finish_failure(&mut backend, &start);
        assert_eq!(state(&failed).update.cycle.state, CycleState::Interrupted);
        assert_eq!(
            state(&failed).update.test.state,
            TestState::RecoveredUncertain
        );
        assert!(matches!(
            failed.events.last(),
            Some(BackendEvent::CommandError(error)) if error.contains("write failure")
        ));
    }

    #[test]
    fn interrupted_safety_stop_retries_after_reconnect() {
        let mut backend = connected_backend();
        let start = backend.start_cycle(recipe(vec![device_step(1000)]));
        finish_success(&mut backend, &start);
        backend.report(report(ReportState::Active, 1), true);
        backend.connection_failed("serial gap".to_owned());
        backend.begin_connection();
        backend.connection_established();
        backend.report(report(ReportState::Active, 2), true);

        let first_stop = backend.stop_cycle();
        assert!(matches!(
            send_frames(&first_stop).as_slice(),
            [OutboundFrame::Stop]
        ));
        assert!(backend.stop_cycle().sends.is_empty());
        let failed = finish_failure(&mut backend, &first_stop);
        assert_eq!(state(&failed).update.cycle.state, CycleState::Interrupted);

        backend.begin_connection();
        backend.connection_established();
        backend.report(report(ReportState::Active, 3), true);
        let retry = backend.stop_cycle();
        assert!(matches!(
            send_frames(&retry).as_slice(),
            [OutboundFrame::Stop]
        ));
        finish_success(&mut backend, &retry);
        assert!(backend.stop_cycle().sends.is_empty());
        assert_eq!(backend.cycle.status().state, CycleState::Interrupted);
    }

    #[test]
    fn cycle_start_prepare_rejection_interrupts_engine() {
        let mut backend = connected_backend();
        let action = backend
            .cycle
            .start(
                recipe(vec![device_step(1000)]),
                "test-cycle".to_owned(),
                None,
                Instant::now(),
            )
            .expect("valid cycle")
            .expect("device action");
        backend.controller.begin_connection("injected stale report");

        let rejected = backend.prepare_cycle_action(action, true);
        assert_eq!(state(&rejected).update.cycle.state, CycleState::Interrupted);
        assert!(matches!(
            rejected.events.last(),
            Some(BackendEvent::CommandError(error)) if error.contains("not connected")
        ));
        assert!(rejected.sends.is_empty());
    }
}
