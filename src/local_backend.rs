//! Local physical-test backend shared by native serial and browser `WebUSB` runners.

use crate::backend::{BackendEvent, BackendState, DiagnosticDirection, DiagnosticEvent};
use crate::controller::{
    CommandKind, ControllerMode, DeviceReport, PreparedCommand, REPORT_TIMEOUT_REASON, ReportState,
    TestController,
};
use crate::core::{
    ApiCommand, AuthoritativeSnapshot, CurrentRunMetadata, CycleRecipe, CycleRunContext,
    CycleSample, RenameRequest, Sample, SavedRecipeReference, ServerConnectionState,
    SnapshotUpdate, StartCycleRequest, StartTestRequest, TestConfiguration,
    cycle_presentation_history, normalize_optional_name,
};
use crate::cycle::{CycleAction, CycleEngine};
use crate::device::{self, InboundFrame, OutboundFrame};
use web_time::Instant;

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
        reset_history: bool,
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
    next_run_id: u64,
    current_run: CurrentRunMetadata,
    cycle_name: Option<String>,
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
            next_run_id: 1,
            current_run: CurrentRunMetadata::default(),
            cycle_name: None,
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
        self.command_at(command, Instant::now())
    }

    fn command_at(&mut self, command: ApiCommand, now: Instant) -> LocalOutput {
        let mut output = self.expire_report_freshness(now);
        output.extend(self.command_inner(command));
        output
    }

    fn command_inner(&mut self, command: ApiCommand) -> LocalOutput {
        let command = match command {
            ApiCommand::Start(config) => {
                return self.start_test(StartTestRequest { config, name: None });
            }
            command => command,
        };
        if command == ApiCommand::Stop && self.cycle.owns_orchestration() {
            return self.stop_cycle();
        }
        if self.cycle.owns_orchestration()
            && matches!(
                command,
                ApiCommand::Resume | ApiCommand::Adjust(_) | ApiCommand::Calibration(_)
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
                        reset_history: false,
                    },
                }],
                ..LocalOutput::default()
            };
        }
        self.command_succeeded(prepared, None, true)
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "backend dispatch transfers request ownership"
    )]
    pub(crate) fn start_test(&mut self, request: StartTestRequest) -> LocalOutput {
        let mut output = self.expire_report_freshness(Instant::now());
        output.extend(self.start_test_inner(&request));
        output
    }

    fn start_test_inner(&mut self, request: &StartTestRequest) -> LocalOutput {
        if self.cycle.owns_orchestration() {
            return Self::command_error("the active cycle owns test orchestration".to_owned());
        }
        let name = match normalize_optional_name(request.name.as_deref()) {
            Ok(name) => name,
            Err(error) => return Self::command_error(error.to_string()),
        };
        let prepared = match self
            .controller
            .prepare_command(ApiCommand::Start(request.config))
        {
            Ok(prepared) => prepared,
            Err(error) => return Self::command_error(error),
        };
        self.begin_physical_run(name, None);
        let Some(frame) = prepared.frame() else {
            return self.command_succeeded(prepared, None, true);
        };
        LocalOutput {
            sends: vec![LocalSend {
                frame,
                completion: SendCompletion::Command {
                    prepared,
                    cycle_action: None,
                    notify_command: true,
                    reset_history: true,
                },
            }],
            ..LocalOutput::default()
        }
    }

    pub(crate) fn resume(&mut self, config: TestConfiguration) -> LocalOutput {
        let mut output = self.expire_report_freshness(Instant::now());
        output.extend(self.resume_inner(config));
        output
    }

    fn resume_inner(&self, config: TestConfiguration) -> LocalOutput {
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
                    reset_history: false,
                },
            }],
            ..LocalOutput::default()
        }
    }

    pub(crate) fn start_cycle(&mut self, request: StartCycleRequest) -> LocalOutput {
        self.start_cycle_with_provenance(request, None)
    }

    pub(crate) fn start_saved_recipe(
        &mut self,
        recipe: CycleRecipe,
        reference: SavedRecipeReference,
        execution_name: Option<String>,
    ) -> LocalOutput {
        self.start_cycle_with_provenance(
            StartCycleRequest {
                recipe,
                name: execution_name,
            },
            Some(reference),
        )
    }

    fn start_cycle_with_provenance(
        &mut self,
        request: StartCycleRequest,
        saved_recipe: Option<SavedRecipeReference>,
    ) -> LocalOutput {
        let mut output = self.expire_report_freshness(Instant::now());
        output.extend(self.start_cycle_inner(request, saved_recipe));
        output
    }

    fn start_cycle_inner(
        &mut self,
        request: StartCycleRequest,
        saved_recipe: Option<SavedRecipeReference>,
    ) -> LocalOutput {
        if let Err(error) = request.recipe.validate() {
            return Self::command_error(error.to_string());
        }
        let name = match normalize_optional_name(request.name.as_deref()) {
            Ok(name) => name,
            Err(error) => return Self::command_error(error.to_string()),
        };
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
        let action = match self.cycle.start(
            request.recipe,
            execution_id,
            name.clone(),
            saved_recipe,
            Some(timestamp_utc()),
            Instant::now(),
        ) {
            Ok(action) => action,
            Err(error) => return Self::command_error(error.to_string()),
        };
        self.cycle_name = name;
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
        let mut output = self.expire_report_freshness(Instant::now());
        output.extend(self.stop_cycle_inner());
        output
    }

    fn stop_cycle_inner(&mut self) -> LocalOutput {
        let owned_orchestration = self.cycle.owns_orchestration();
        if let Some(action) = self.cycle.stop(self.controller.test()) {
            self.prepare_cycle_action(action, true)
        } else if owned_orchestration
            && self.cycle.pending_action().is_none()
            && self.controller.requires_stop_before_disconnect()
        {
            // Rest/Settling may have no owned physical run, but contradictory
            // live activity must still receive the operator's safety Stop.
            self.command_inner(ApiCommand::Stop)
        } else {
            let mut output = self.state_output();
            output.events.push(BackendEvent::CommandSucceeded);
            output
        }
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "backend command dispatch transfers request ownership"
    )]
    pub(crate) fn rename_run(&mut self, run_id: &str, request: RenameRequest) -> LocalOutput {
        if self.current_run.id.as_deref() != Some(run_id) {
            return Self::command_error("the requested run is not current".to_owned());
        }
        if self.current_run.cycle.is_some() {
            return Self::command_error(
                "cycle child runs cannot be renamed; rename the cycle instead".to_owned(),
            );
        }
        let name = match normalize_optional_name(request.name.as_deref()) {
            Ok(name) => name,
            Err(error) => return Self::command_error(error.to_string()),
        };
        self.current_run.name = name;
        self.success_without_frame()
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "backend command dispatch transfers request ownership"
    )]
    pub(crate) fn rename_cycle(
        &mut self,
        execution_id: &str,
        request: RenameRequest,
    ) -> LocalOutput {
        if self.cycle.status().execution_id.as_deref() != Some(execution_id) {
            return Self::command_error("the requested cycle is not current".to_owned());
        }
        self.cycle_name = match normalize_optional_name(request.name.as_deref()) {
            Ok(name) => name,
            Err(error) => return Self::command_error(error.to_string()),
        };
        self.success_without_frame()
    }

    fn success_without_frame(&self) -> LocalOutput {
        let mut output = self.state_output();
        output.events.push(BackendEvent::CommandSucceeded);
        output
    }

    fn begin_physical_run(&mut self, name: Option<String>, cycle: Option<CycleRunContext>) {
        let id = format!("local-run-{}", self.next_run_id);
        self.next_run_id = self.next_run_id.saturating_add(1);
        self.current_run = CurrentRunMetadata {
            id: Some(id),
            name,
            cycle,
        };
    }

    pub(crate) fn finish_send(
        &mut self,
        send: LocalSend,
        result: Result<(), String>,
    ) -> LocalOutput {
        self.finish_send_at(send, result, Instant::now())
    }

    fn finish_send_at(
        &mut self,
        send: LocalSend,
        result: Result<(), String>,
        now: Instant,
    ) -> LocalOutput {
        let mut output = self.expire_report_freshness(now);
        output.extend(self.finish_send_inner(send, result, now));
        output
    }

    fn finish_send_inner(
        &mut self,
        send: LocalSend,
        result: Result<(), String>,
        now: Instant,
    ) -> LocalOutput {
        match result {
            Ok(()) => match send.completion {
                SendCompletion::Command {
                    prepared,
                    cycle_action,
                    notify_command,
                    ..
                } => self.command_succeeded_at(prepared, cycle_action, notify_command, now),
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
        self.command_succeeded_at(prepared, cycle_action, notify_command, Instant::now())
    }

    fn command_succeeded_at(
        &mut self,
        prepared: PreparedCommand,
        cycle_action: Option<CycleAction>,
        notify_command: bool,
        now: Instant,
    ) -> LocalOutput {
        // Both written-frame acknowledgements and synchronous no-frame
        // completions must propagate expiry to orchestration at the same instant
        // used by the controller commit. Checking only before preparation leaves
        // a deadline-crossing race even without an await.
        let mut output = self.expire_report_freshness(now);
        if !self.controller.commit_command_at(prepared, None, now) {
            self.cycle.interrupt_for_gap(REPORT_TIMEOUT_REASON);
            output.extend(self.state_output());
            output
                .events
                .push(BackendEvent::CommandError(REPORT_TIMEOUT_REASON.to_owned()));
            return output;
        }
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

    /// Recheck at the wire boundary as a tab/process can pause after preparation.
    pub(crate) fn authorize_send(&mut self, send: LocalSend) -> (LocalOutput, bool) {
        self.authorize_send_at(send, Instant::now())
    }

    fn authorize_send_at(&mut self, send: LocalSend, now: Instant) -> (LocalOutput, bool) {
        let mut output = self.expire_report_freshness(now);
        let authorization = match send.completion {
            SendCompletion::Command { prepared, .. } => {
                self.controller.validate_prepared_at(prepared, now)
            }
            SendCompletion::TimerSync if !self.controller.timer_sync_authorized_at(now) => {
                Err(REPORT_TIMEOUT_REASON.to_owned())
            }
            SendCompletion::TimerSync | SendCompletion::BestEffort => Ok(()),
        };
        if let Err(error) = authorization {
            output.events.push(BackendEvent::CommandError(error));
            return (output, false);
        }
        (output, true)
    }

    fn prepare_cycle_action(&mut self, action: CycleAction, notify_command: bool) -> LocalOutput {
        let reset_history = matches!(action, CycleAction::Start(_));
        let command = match action {
            CycleAction::Start(config) => ApiCommand::Start(config),
            CycleAction::Stop => ApiCommand::Stop,
        };
        let prepared = match self.controller.prepare_command(command) {
            Ok(prepared) => prepared,
            Err(error) => {
                if !matches!(action, CycleAction::Start(_))
                    || self.cycle.status().result.as_deref() != Some(REPORT_TIMEOUT_REASON)
                {
                    self.cycle.on_action_failed(error.clone());
                }
                let mut output = self.state_output();
                output.events.push(BackendEvent::CommandError(error));
                return output;
            }
        };
        if matches!(action, CycleAction::Start(_)) {
            let status = self.cycle.status();
            let cycle = status
                .execution_id
                .clone()
                .map(|execution_id| CycleRunContext {
                    execution_id,
                    repeat_index: status.repeat_index,
                    step_index: status.step_index,
                });
            self.begin_physical_run(None, cycle);
        }
        if let Some(frame) = prepared.frame() {
            return LocalOutput {
                sends: vec![LocalSend {
                    frame,
                    completion: SendCompletion::Command {
                        prepared,
                        cycle_action: Some(action),
                        notify_command,
                        reset_history,
                    },
                }],
                events: vec![if reset_history {
                    BackendEvent::Snapshot(self.snapshot())
                } else {
                    BackendEvent::Update(self.state())
                }],
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
        self.tick_at(Instant::now())
    }

    fn expire_report_freshness(&mut self, now: Instant) -> LocalOutput {
        if !self
            .cycle
            .expire_report_freshness(&mut self.controller, now)
        {
            return LocalOutput::default();
        }
        self.connection_error = Some(REPORT_TIMEOUT_REASON.to_owned());
        self.state_output()
    }

    fn tick_at(&mut self, now: Instant) -> LocalOutput {
        let mut output = self.expire_report_freshness(now);
        let previous_cycle = self.cycle.status().clone();
        if let Some(minutes) = self.controller.next_timer_sync_at(now) {
            output.sends.push(LocalSend {
                frame: OutboundFrame::TimerSync(minutes),
                completion: SendCompletion::TimerSync,
            });
        }
        self.controller.update_elapsed_at(now);
        if let Some(action) = self.cycle.tick(now) {
            output.extend(self.prepare_cycle_action(action, false));
        }
        let elapsed = self.controller.test().elapsed_seconds;
        if elapsed != self.last_published_elapsed || self.cycle.status() != &previous_cycle {
            self.last_published_elapsed = elapsed;
            output.events.push(BackendEvent::Update(self.state()));
        }
        output
    }

    #[cfg(test)]
    pub(crate) fn frame(&mut self, frame: InboundFrame, raw_bytes: Vec<u8>) -> LocalOutput {
        self.frame_received_at(frame, raw_bytes, Instant::now())
    }

    pub(crate) fn frame_received_at(
        &mut self,
        frame: InboundFrame,
        raw_bytes: Vec<u8>,
        received_at: Instant,
    ) -> LocalOutput {
        self.frame_received_at_time(frame, raw_bytes, received_at, Instant::now())
    }

    fn frame_received_at_time(
        &mut self,
        frame: InboundFrame,
        raw_bytes: Vec<u8>,
        received_at: Instant,
        now: Instant,
    ) -> LocalOutput {
        let label = format!("{frame:?}");
        let mut output = match frame {
            InboundFrame::Firmware(report) => self.report_received_at(
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
                received_at,
                now,
            ),
            InboundFrame::Charge(report) => self.report_received_at(
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
                received_at,
                now,
            ),
            InboundFrame::DischargeConstantCurrent(report) => self.report_received_at(
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
                received_at,
                now,
            ),
            InboundFrame::DischargeConstantPower(report) => self.report_received_at(
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
                received_at,
                now,
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

    #[cfg(test)]
    fn report(&mut self, report: DeviceReport, sample_report: bool) -> LocalOutput {
        self.report_at(report, sample_report, Instant::now())
    }

    #[cfg(test)]
    fn report_at(
        &mut self,
        report: DeviceReport,
        sample_report: bool,
        now: Instant,
    ) -> LocalOutput {
        self.process_report_at(report, sample_report, now, now, false)
    }

    fn report_received_at(
        &mut self,
        report: DeviceReport,
        sample_report: bool,
        received_at: Instant,
        now: Instant,
    ) -> LocalOutput {
        self.process_report_at(report, sample_report, received_at, now, true)
    }

    fn process_report_at(
        &mut self,
        report: DeviceReport,
        sample_report: bool,
        received_at: Instant,
        now: Instant,
        receipt_fenced: bool,
    ) -> LocalOutput {
        let mut output = self.expire_report_freshness(now);
        let mode = report.mode;
        let voltage_mv = report.voltage_mv;
        let current_ma = report.current_ma;
        let device_capacity_mah = report.capacity_mah;
        let (outcome, measurement) = if receipt_fenced {
            self.controller.report_received_at(report, received_at, now)
        } else {
            self.controller.report_at(report, now)
        };
        if !outcome.accepted {
            return output;
        }
        if self.connection_error.as_deref() == Some(REPORT_TIMEOUT_REASON) {
            self.connection_error = None;
        }
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
                run_id: self.current_run.id.clone().unwrap_or_default(),
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
        let mut cycle = self.cycle.status().clone();
        cycle.name.clone_from(&self.cycle_name);
        BackendState {
            update: SnapshotUpdate {
                connection: self.connection.clone(),
                connection_error: self.connection_error.clone(),
                device: self.controller.device().clone(),
                test: self.controller.test().clone(),
                cycle,
                capabilities,
                current_run: self.current_run.clone(),
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
            current_run: state.update.current_run,
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
            let mut reset_history = false;
            match send.completion {
                SendCompletion::Command {
                    prepared,
                    cycle_action,
                    reset_history: command_resets_history,
                    ..
                } => {
                    reset_history = command_resets_history;
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
            let state = if reset_history {
                BackendEvent::Snapshot(self.snapshot())
            } else {
                BackendEvent::Update(self.state())
            };
            return LocalOutput {
                events: vec![state, BackendEvent::CommandError(message)],
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
    use crate::core::{CycleRecipe, CycleState, CycleStep, CycleStepCompletion, TestState};
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

    fn cycle_request(recipe: CycleRecipe) -> StartCycleRequest {
        StartCycleRequest { recipe, name: None }
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

    fn snapshot(output: &LocalOutput) -> &AuthoritativeSnapshot {
        output
            .events
            .iter()
            .find_map(|event| match event {
                BackendEvent::Snapshot(snapshot) => Some(snapshot),
                _ => None,
            })
            .expect("authoritative snapshot event")
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

    fn start_request(name: Option<&str>) -> StartTestRequest {
        StartTestRequest {
            config: config(),
            name: name.map(str::to_owned),
        }
    }

    #[test]
    fn interrupted_cycle_allows_stop_retries_and_a_later_manual_stop() {
        use crate::controller::REPORT_FRESHNESS_TIMEOUT;
        let mut backend = connected_backend();
        let start = backend.start_cycle(cycle_request(recipe(vec![device_step(100)])));
        finish_success(&mut backend, &start);
        let t0 = Instant::now();
        backend.report_at(report(ReportState::Active, 1), true, t0);
        backend.tick_at(t0 + REPORT_FRESHNESS_TIMEOUT);
        let interrupted = backend.cycle.status().clone();
        let stop = backend.command(ApiCommand::Stop);
        assert!(matches!(
            send_frames(&stop).as_slice(),
            [OutboundFrame::Stop]
        ));
        finish_success(&mut backend, &stop);
        backend.report_at(report(ReportState::Active, 2), true, t0);
        backend.tick_at(t0 + REPORT_FRESHNESS_TIMEOUT);
        let retry = backend.command(ApiCommand::Stop);
        assert!(matches!(
            send_frames(&retry).as_slice(),
            [OutboundFrame::Stop]
        ));
        finish_success(&mut backend, &retry);
        // The cycle-specific public backend boundary must allow retries too.
        let retry = backend.stop_cycle();
        assert!(matches!(
            send_frames(&retry).as_slice(),
            [OutboundFrame::Stop]
        ));
        finish_success(&mut backend, &retry);
        backend.report(report(ReportState::Idle, 2), true);
        let start = backend.start_test(start_request(Some("new manual")));
        finish_success(&mut backend, &start);
        assert!(backend.current_run.cycle.is_none());
        backend.report(report(ReportState::Active, 1), true);
        let stop = backend.command(ApiCommand::Stop);
        assert!(matches!(
            send_frames(&stop).as_slice(),
            [OutboundFrame::Stop]
        ));
        finish_success(&mut backend, &stop);
        assert_eq!(backend.controller.test().state, TestState::Stopping);
        assert_eq!(backend.cycle.status(), &interrupted);
    }

    #[test]
    fn manual_start_is_guarded_and_defensive_stop_writes_during_rest() {
        let mut backend = connected_backend();
        backend.start_cycle(cycle_request(recipe(vec![CycleStep::Rest {
            duration_seconds: 30,
        }])));
        let before = backend.snapshot();
        for output in [
            backend.start_test(start_request(Some("competing"))),
            backend.command(ApiCommand::Start(config())),
        ] {
            assert!(output.sends.is_empty());
            assert!(
                output
                    .events
                    .iter()
                    .any(|event| matches!(event, BackendEvent::CommandError(_)))
            );
        }
        assert_eq!(backend.snapshot(), before);
        // Contradictory backend state must not let Rest skip physical Stop.
        let prepared = backend
            .controller
            .prepare_command(ApiCommand::Start(config()))
            .expect("Start");
        backend.controller.commit_command(prepared, None);
        backend.controller.report(report(ReportState::Active, 1));
        let output = backend.command(ApiCommand::Stop);
        assert!(matches!(
            send_frames(&output).as_slice(),
            [OutboundFrame::Stop]
        ));
        finish_success(&mut backend, &output);
        assert_eq!(backend.cycle.status().state, CycleState::Stopping);
        assert_eq!(backend.controller.test().state, TestState::Stopping);
    }

    #[test]
    fn stop_during_rest_also_stops_live_activity_without_owned_test_state() {
        let mut backend = connected_backend();
        backend.start_cycle(cycle_request(recipe(vec![CycleStep::Rest {
            duration_seconds: 30,
        }])));
        backend.report(report(ReportState::Active, 1), true);
        assert_eq!(backend.controller.test().state, TestState::Idle);
        assert!(backend.controller.device().active);
        assert_eq!(backend.cycle.status().state, CycleState::Resting);
        let stop = backend.command(ApiCommand::Stop);
        assert!(matches!(
            send_frames(&stop).as_slice(),
            [OutboundFrame::Stop]
        ));
        finish_success(&mut backend, &stop);
        assert_eq!(backend.controller.test().state, TestState::Stopping);
        assert_eq!(backend.cycle.status().state, CycleState::Stopped);
        assert!(!backend.controller.is_running_owned());
    }

    #[test]
    fn silent_stop_success_failure_and_late_ack_preserve_real_retries() {
        use crate::controller::REPORT_FRESHNESS_TIMEOUT;
        for fail in [false, true] {
            let mut backend = connected_backend();
            let start = backend.command(ApiCommand::Start(config()));
            finish_success(&mut backend, &start);
            let t0 = Instant::now();
            backend.report_at(report(ReportState::Active, 1), true, t0);
            backend.tick_at(t0 + REPORT_FRESHNESS_TIMEOUT);
            let stop = backend.command(ApiCommand::Stop);
            assert!(matches!(
                send_frames(&stop).as_slice(),
                [OutboundFrame::Stop]
            ));
            if fail {
                finish_failure(&mut backend, &stop);
                assert!(backend.command(ApiCommand::Stop).sends.is_empty());
                backend.begin_connection();
                backend.connection_established();
            } else {
                finish_success(&mut backend, &stop);
            }
            backend.tick_at(t0 + web_time::Duration::from_secs(100));
            assert!(!backend.controller.device().activity_known);
            assert!(backend.snapshot().capabilities.stop);
            let retry = backend.command(ApiCommand::Stop);
            assert!(matches!(
                send_frames(&retry).as_slice(),
                [OutboundFrame::Stop]
            ));
            let late = t0 + web_time::Duration::from_secs(101);
            assert!(backend.authorize_send_at(retry.sends[0], late).1);
            backend.finish_send_at(retry.sends[0], Ok(()), late);
            assert!(!backend.controller.is_running_owned());
            assert!(!backend.controller.device().activity_known);
            let retry = backend.command(ApiCommand::Stop);
            assert!(matches!(
                send_frames(&retry).as_slice(),
                [OutboundFrame::Stop]
            ));
            finish_success(&mut backend, &retry);
            backend.report_at(report(ReportState::Idle, 1), true, late);
            assert_eq!(backend.controller.test().state, TestState::Stopped);
        }
    }

    #[test]
    fn still_active_report_after_stop_allows_a_real_retry_without_reclaiming_metrics() {
        let mut backend = connected_backend();
        let start = backend.start_cycle(cycle_request(recipe(vec![device_step(100)])));
        finish_success(&mut backend, &start);
        backend.report(report(ReportState::Active, 1), true);
        let stop = backend.command(ApiCommand::Stop);
        finish_success(&mut backend, &stop);
        assert!(backend.command(ApiCommand::Stop).sends.is_empty());
        let metrics = backend.controller.test().clone();
        backend.report(report(ReportState::Active, 2), true);
        assert!(!backend.controller.is_running_owned());
        assert_eq!(backend.controller.test().capacity_mah, metrics.capacity_mah);
        assert_eq!(backend.controller.test().energy_wh, metrics.energy_wh);
        assert!(backend.snapshot().capabilities.stop);
        let retry = backend.command(ApiCommand::Stop);
        assert!(matches!(
            send_frames(&retry).as_slice(),
            [OutboundFrame::Stop]
        ));
        finish_success(&mut backend, &retry);
        assert_eq!(backend.cycle.status().state, CycleState::Stopping);
    }

    fn receive_bytes(backend: &mut LocalBackend, mut bytes: Vec<u8>) -> LocalOutput {
        let mut output = LocalOutput::default();
        for (frame, raw) in device::process_buffer(&mut bytes) {
            output.extend(backend.frame(frame, raw));
        }
        output
    }

    fn parsed_report(command: u8) -> (InboundFrame, Vec<u8>) {
        let mut bytes = vec![
            0xfa, command, 0, 10, 0x10, 0xa0, 0, 30, 0, 0, 0, 10, 1, 0x3c, 0, 0, 9, 0, 0xf8,
        ];
        bytes[17] = bytes[1..17].iter().fold(0, |sum, byte| sum ^ byte);
        device::process_buffer(&mut bytes)
            .pop()
            .expect("parsed report")
    }

    #[test]
    fn receive_queue_expiry_discards_normal_firmware_and_settling_reports_at_equality() {
        use crate::controller::REPORT_FRESHNESS_TIMEOUT;
        for command in [0, 10, 20, 0x64, 0x6e] {
            let mut backend = connected_backend();
            let start = backend.start_cycle(cycle_request(recipe(vec![
                device_step(100),
                device_step(100),
            ])));
            finish_success(&mut backend, &start);
            let received_at = Instant::now();
            backend.report_at(report(ReportState::Active, 1), true, received_at);
            let now = received_at + REPORT_FRESHNESS_TIMEOUT;
            for _ in 0..3 {
                let (frame, raw) = parsed_report(command);
                let output = backend.frame_received_at_time(frame, raw, received_at, now);
                assert!(output.sends.is_empty());
                assert!(!output.events.iter().any(|event| matches!(
                    event,
                    BackendEvent::Sample(_) | BackendEvent::CycleSample(_)
                )));
                assert!(!backend.controller.device().activity_known);
                assert_eq!(backend.cycle.status().state, CycleState::Interrupted);
                assert!(!backend.controller.capabilities_at(now).start);
            }
            assert!(
                backend
                    .tick_at(now + web_time::Duration::from_secs(60))
                    .sends
                    .is_empty()
            );
        }
    }

    #[test]
    fn parsed_firmware_refreshes_observation_without_sampling_and_mode_mismatch_revokes_ownership()
    {
        let mut backend = connected_backend();
        let start = backend.start_cycle(cycle_request(recipe(vec![device_step(100)])));
        finish_success(&mut backend, &start);
        let (frame, raw) = parsed_report(0x6e);
        let output = backend.frame(frame, raw);
        assert!(backend.controller.is_running_owned());
        assert!(backend.controller.device().activity_known);
        assert!(!output.events.iter().any(|event| matches!(
            event,
            BackendEvent::Sample(_) | BackendEvent::CycleSample(_)
        )));
        let before = backend.controller.test().clone();
        let (frame, raw) = parsed_report(0x6f);
        let output = backend.frame(frame, raw);
        assert_eq!(
            backend.controller.test().state,
            TestState::RecoveredUncertain
        );
        assert!(backend.controller.device().active);
        assert_eq!(backend.controller.test().capacity_mah, before.capacity_mah);
        assert_eq!(backend.cycle.status().state, CycleState::Interrupted);
        assert!(!output.events.iter().any(|event| matches!(
            event,
            BackendEvent::Sample(_) | BackendEvent::CycleSample(_)
        )));
        let mut now = Instant::now();
        for _ in 0..13 {
            now += web_time::Duration::from_secs(5);
            let (frame, raw) = parsed_report(11);
            let output = backend.frame_received_at_time(frame, raw, now, now);
            assert!(
                !output
                    .events
                    .iter()
                    .any(|event| matches!(event, BackendEvent::Sample(_)))
            );
            assert!(backend.tick_at(now).sends.is_empty());
        }
        let (frame, raw) = parsed_report(10);
        backend.frame_received_at_time(frame, raw, now, now);
        assert!(!backend.controller.is_running_owned());
        assert_eq!(backend.controller.test().capacity_mah, before.capacity_mah);
        assert_eq!(backend.controller.test().energy_wh, before.energy_wh);
        assert!(matches!(
            send_frames(&backend.command(ApiCommand::Stop)).as_slice(),
            [OutboundFrame::Stop]
        ));
    }

    #[test]
    fn real_stop_from_replaced_connection_is_rejected_at_authorization_and_completion() {
        let mut backend = connected_backend();
        let start = backend.start_test(start_request(None));
        finish_success(&mut backend, &start);
        backend.report(report(ReportState::Active, 1), true);
        let old = backend.command(ApiCommand::Stop).sends[0];
        assert!(matches!(old.frame(), OutboundFrame::Stop));
        backend.begin_connection();
        backend.connection_established();
        // Unknown Stop is eligible on the replacement, isolating connection
        // generation from freshness/authority rejection of state-dependent Start.
        assert!(backend.controller.capabilities().stop);
        assert!(!backend.authorize_send(old).1);
        let completion = backend.finish_send(old, Ok(()));
        assert!(
            completion
                .events
                .iter()
                .any(|event| matches!(event, BackendEvent::CommandError(_)))
        );
        assert!(!backend.controller.is_stopping());
    }

    #[test]
    fn corrupt_payload_and_firmware_cannot_settle_sample_or_refresh_local_authority() {
        use crate::controller::REPORT_FRESHNESS_TIMEOUT;
        let mut backend = connected_backend();
        let start = backend.start_cycle(cycle_request(recipe(vec![
            device_step(100),
            device_step(100),
        ])));
        finish_success(&mut backend, &start);
        backend.report(observed_report(ReportState::Active, 4000, 80, 1), true);
        backend.report(observed_report(ReportState::Finished, 4000, 80, 1), true);
        let before = backend.snapshot();
        assert_eq!(before.cycle.state, CycleState::Settling);
        let mut corrupt = vec![
            0xfa, 0, 0, 8, 0x10, 0xa0, 0, 1, 0, 0, 0, 10, 1, 0x3c, 0, 0, 9, 0x87, 0xf8,
        ];
        corrupt[3] ^= 8;
        let output = receive_bytes(&mut backend, corrupt.clone());
        assert!(output.sends.is_empty() && output.events.is_empty());
        for command in [0x64, 0x6e] {
            let mut firmware = corrupt.clone();
            firmware[1] = command; // checksum is invalid for either firmware report
            let output = receive_bytes(&mut backend, firmware);
            assert!(output.sends.is_empty() && output.events.is_empty());
        }
        assert_eq!(backend.snapshot(), before);
        let mut valid = corrupt.clone();
        valid[17] ^= 8; // correct checksum for zero current
        let output = receive_bytes(&mut backend, valid);
        assert!(matches!(
            send_frames(&output).as_slice(),
            [OutboundFrame::StartConstantCurrentDischarge(..)]
        ));
        assert_eq!(backend.cycle.status().step_index, 1);
        assert_eq!(
            backend
                .cycle_history
                .last()
                .expect("boundary sample")
                .cycle_state,
            CycleState::Settling
        );
        let old = Instant::now()
            .checked_sub(REPORT_FRESHNESS_TIMEOUT)
            .expect("old report");
        backend.report_at(report(ReportState::Idle, 1), true, old);
        receive_bytes(&mut backend, corrupt);
        backend.tick();
        assert!(!backend.controller.device().activity_known);
        assert_eq!(backend.cycle.status().state, CycleState::Interrupted);
    }

    #[test]
    fn silent_rest_is_interrupted_before_expiry_start_and_never_recovers() {
        use crate::controller::REPORT_FRESHNESS_TIMEOUT;
        let mut backend = connected_backend();
        let output = backend.start_cycle(cycle_request(recipe(vec![
            CycleStep::Rest {
                duration_seconds: REPORT_FRESHNESS_TIMEOUT.as_secs(),
            },
            device_step(100),
        ])));
        assert!(output.sends.is_empty());
        let t0 = Instant::now();
        backend.report_at(report(ReportState::Idle, 0), true, t0);
        let expired = backend.tick_at(t0 + REPORT_FRESHNESS_TIMEOUT);
        assert!(expired.sends.is_empty());
        assert_eq!(state(&expired).update.cycle.state, CycleState::Interrupted);
        assert!(!state(&expired).update.device.activity_known);
        assert_eq!(
            state(&expired).update.connection,
            ServerConnectionState::Connected
        );
        assert!(
            backend
                .tick_at(t0 + web_time::Duration::from_secs(65))
                .events
                .is_empty()
        );
        let recovered = backend.report_at(
            report(ReportState::Idle, 0),
            true,
            t0 + web_time::Duration::from_secs(66),
        );
        assert!(recovered.sends.is_empty());
        assert_eq!(backend.cycle.status().state, CycleState::Interrupted);
        assert!(
            backend
                .tick_at(t0 + web_time::Duration::from_secs(67))
                .sends
                .is_empty()
        );
    }

    #[test]
    fn local_command_expiry_denies_intent_but_preserves_stop_and_disconnect() {
        use crate::controller::REPORT_FRESHNESS_TIMEOUT;
        let t0 = Instant::now();
        let mut backend = connected_backend();
        backend.report_at(report(ReportState::Idle, 0), true, t0);
        let start = backend.command(ApiCommand::Start(config()));
        finish_success(&mut backend, &start);
        backend.report_at(report(ReportState::Active, 1), true, t0);
        let adjust =
            backend.command_at(ApiCommand::Adjust(config()), t0 + REPORT_FRESHNESS_TIMEOUT);
        assert!(adjust.sends.is_empty());
        assert!(
            adjust
                .events
                .iter()
                .any(|event| matches!(event, BackendEvent::CommandError(_)))
        );
        assert!(!backend.controller.is_running_owned());
        let stop = backend.command_at(ApiCommand::Stop, t0 + REPORT_FRESHNESS_TIMEOUT);
        assert!(matches!(
            send_frames(&stop).as_slice(),
            [OutboundFrame::Stop]
        ));
        let disconnect = backend.request_disconnect();
        assert!(matches!(
            send_frames(&disconnect).as_slice(),
            [OutboundFrame::Stop, OutboundFrame::Disconnect]
        ));
        assert!(
            backend
                .command_at(ApiCommand::Start(config()), t0 + REPORT_FRESHNESS_TIMEOUT)
                .sends
                .is_empty()
        );
        assert!(
            backend
                .command_at(ApiCommand::Resume, t0 + REPORT_FRESHNESS_TIMEOUT)
                .sends
                .is_empty()
        );
        assert!(
            backend
                .command_at(
                    ApiCommand::Calibration(crate::core::CalibrationCommand::VoltageLow(1000)),
                    t0 + REPORT_FRESHNESS_TIMEOUT
                )
                .sends
                .is_empty()
        );
        assert!(
            backend
                .start_cycle(cycle_request(recipe(vec![device_step(100)])))
                .sends
                .is_empty()
        );
    }

    #[test]
    fn local_first_recovered_report_interrupts_rest_before_it_can_advance() {
        use crate::controller::REPORT_FRESHNESS_TIMEOUT;
        let mut backend = connected_backend();
        backend.start_cycle(cycle_request(recipe(vec![
            CycleStep::Rest {
                duration_seconds: 65,
            },
            device_step(100),
        ])));
        let t0 = Instant::now();
        backend.report_at(report(ReportState::Idle, 0), true, t0);
        let output = backend.report_at(
            report(ReportState::Idle, 0),
            true,
            t0 + REPORT_FRESHNESS_TIMEOUT,
        );
        assert!(output.sends.is_empty());
        assert_eq!(backend.cycle.status().state, CycleState::Interrupted);
        assert!(
            backend
                .tick_at(t0 + web_time::Duration::from_secs(66))
                .sends
                .is_empty()
        );
    }

    #[test]
    fn queued_start_is_rejected_at_wire_boundary_even_after_report_recovery() {
        use crate::controller::REPORT_FRESHNESS_TIMEOUT;
        let mut backend = connected_backend();
        let t0 = Instant::now();
        backend.report_at(report(ReportState::Idle, 0), true, t0);
        let output = backend.start_cycle(cycle_request(recipe(vec![device_step(100)])));
        let send = output.sends[0];
        let (expired, allowed) = backend.authorize_send_at(send, t0 + REPORT_FRESHNESS_TIMEOUT);
        assert!(!allowed);
        assert!(expired.sends.is_empty());
        assert_eq!(backend.cycle.status().state, CycleState::Interrupted);
        let rejected = backend.prepare_cycle_action(CycleAction::Start(config()), false);
        assert!(rejected.sends.is_empty());
        assert_eq!(
            backend.cycle.status().result.as_deref(),
            Some(REPORT_TIMEOUT_REASON)
        );
        backend.report_at(
            report(ReportState::Idle, 0),
            true,
            t0 + REPORT_FRESHNESS_TIMEOUT,
        );
        assert!(
            !backend
                .authorize_send_at(send, t0 + REPORT_FRESHNESS_TIMEOUT)
                .1
        );
    }

    #[test]
    fn late_successful_start_ack_is_uncertain_and_cannot_reclaim_ownership() {
        use crate::controller::REPORT_FRESHNESS_TIMEOUT;
        let mut backend = connected_backend();
        let t0 = Instant::now();
        backend.report_at(report(ReportState::Idle, 0), true, t0);
        let output = backend.command(ApiCommand::Start(config()));
        let completed =
            backend.finish_send_at(output.sends[0], Ok(()), t0 + REPORT_FRESHNESS_TIMEOUT);
        assert_eq!(
            backend.controller.test().state,
            TestState::RecoveredUncertain
        );
        assert!(
            !completed
                .events
                .iter()
                .any(|event| matches!(event, BackendEvent::CommandSucceeded))
        );
        let recovered = backend.report_at(
            report(ReportState::Active, 10),
            true,
            t0 + REPORT_FRESHNESS_TIMEOUT,
        );
        assert!(!backend.controller.is_running_owned());
        assert!(
            !recovered
                .events
                .iter()
                .any(|event| matches!(event, BackendEvent::Sample(_)))
        );
        assert_eq!(backend.connection, ServerConnectionState::Connected);
        assert!(matches!(
            send_frames(&backend.command(ApiCommand::Stop)).as_slice(),
            [OutboundFrame::Stop]
        ));
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "keep the complete deadline race and safety retry sequence together"
    )]
    fn no_frame_repeated_stop_completion_crossing_deadline_interrupts_cycle() {
        use crate::controller::{PhysicalState, REPORT_FRESHNESS_TIMEOUT};
        use web_time::Duration;

        for lateness in [Duration::ZERO, Duration::from_nanos(1)] {
            let mut backend = connected_backend();
            let start = backend.start_cycle(cycle_request(recipe(vec![device_step(100)])));
            let t0 = Instant::now();
            backend.finish_send_at(start.sends[0], Ok(()), t0);
            backend.report_at(report(ReportState::Active, 0), true, t0);
            assert_eq!(backend.cycle.status().state, CycleState::RunningStep);

            let action = backend
                .cycle
                .stop(backend.controller.test())
                .expect("first Stop");
            let first = backend.prepare_cycle_action(action, true);
            assert!(matches!(
                send_frames(&first).as_slice(),
                [OutboundFrame::Stop]
            ));
            assert!(backend.authorize_send_at(first.sends[0], t0).1);
            backend.finish_send_at(first.sends[0], Ok(()), t0);
            assert_eq!(backend.cycle.status().state, CycleState::Stopping);
            assert_eq!(backend.controller.test().state, TestState::Stopping);

            let boundary = t0 + REPORT_FRESHNESS_TIMEOUT;
            let before = t0 + REPORT_FRESHNESS_TIMEOUT.saturating_sub(Duration::from_nanos(1));
            let action = backend
                .cycle
                .stop(backend.controller.test())
                .expect("repeated Stop");
            let prepared = backend
                .controller
                .prepare_command_at(ApiCommand::Stop, before)
                .expect("fresh Stop");
            assert!(prepared.frame().is_none());
            assert!(
                backend
                    .controller
                    .validate_prepared_at(prepared, before)
                    .is_ok()
            );
            assert!(backend.controller.device().activity_known);
            let completed =
                backend.command_succeeded_at(prepared, Some(action), true, boundary + lateness);
            assert!(completed.sends.is_empty(), "no second Stop was written");
            assert_eq!(backend.cycle.status().state, CycleState::Interrupted);
            assert_eq!(
                backend.cycle.status().result.as_deref(),
                Some(REPORT_TIMEOUT_REASON)
            );
            assert_eq!(
                backend.controller.test().state,
                TestState::RecoveredUncertain
            );
            assert_eq!(
                backend.controller.test().result.as_deref(),
                Some(REPORT_TIMEOUT_REASON)
            );
            assert_eq!(backend.controller.physical_state(), PhysicalState::Unknown);
            assert!(!backend.controller.device().activity_known);
            assert!(!backend.controller.is_running_owned());
            assert_eq!(backend.connection, ServerConnectionState::Connected);
            assert!(
                completed
                    .events
                    .iter()
                    .any(|event| matches!(event, BackendEvent::CommandError(_)))
            );
            assert!(
                !completed
                    .events
                    .iter()
                    .any(|event| matches!(event, BackendEvent::CommandSucceeded))
            );

            // Repeated completion and later ticks cannot consume or erase the interruption.
            let test = backend.controller.test().clone();
            backend.command_succeeded_at(prepared, Some(action), true, boundary + lateness);
            assert_eq!(backend.controller.test(), &test);
            assert!(
                backend
                    .tick_at(boundary + Duration::from_secs(1))
                    .sends
                    .is_empty()
            );
            assert_eq!(backend.cycle.status().state, CycleState::Interrupted);
            for command in [
                ApiCommand::Start(config()),
                ApiCommand::Adjust(config()),
                ApiCommand::Resume,
            ] {
                assert!(
                    backend
                        .controller
                        .prepare_command_at(command, boundary)
                        .is_err()
                );
            }

            // A new explicit safety intent must really reach the transport.
            let retry_action = backend
                .cycle
                .stop(backend.controller.test())
                .expect("safety Stop");
            let retry = backend.prepare_cycle_action(retry_action, true);
            assert!(matches!(
                send_frames(&retry).as_slice(),
                [OutboundFrame::Stop]
            ));
            assert!(
                backend
                    .authorize_send_at(retry.sends[0], boundary + lateness)
                    .1
            );
            backend.finish_send_at(retry.sends[0], Ok(()), boundary + lateness);
            assert_eq!(backend.controller.test().state, TestState::Stopping);
            assert!(!backend.controller.device().activity_known);
            assert_eq!(backend.cycle.status().state, CycleState::Interrupted);
            assert_eq!(
                backend.cycle.status().result.as_deref(),
                Some(REPORT_TIMEOUT_REASON)
            );
            for state in [ReportState::Active, ReportState::Idle] {
                backend.report_at(report(state, 0), true, boundary + Duration::from_secs(2));
                assert!(!backend.controller.is_running_owned());
                assert!(
                    backend
                        .tick_at(boundary + Duration::from_secs(3))
                        .sends
                        .is_empty()
                );
                assert_eq!(backend.cycle.status().state, CycleState::Interrupted);
                assert_eq!(
                    backend.cycle.status().result.as_deref(),
                    Some(REPORT_TIMEOUT_REASON)
                );
            }
            assert!(
                send_frames(&backend.request_disconnect())
                    .iter()
                    .any(|frame| matches!(frame, OutboundFrame::Disconnect))
            );
        }
    }

    #[test]
    fn fresh_no_frame_repeated_stop_completion_remains_a_no_op() {
        use crate::controller::REPORT_FRESHNESS_TIMEOUT;
        let mut backend = connected_backend();
        let start = backend.start_cycle(cycle_request(recipe(vec![device_step(100)])));
        let t0 = Instant::now();
        backend.finish_send_at(start.sends[0], Ok(()), t0);
        backend.report_at(report(ReportState::Active, 0), true, t0);
        let first = backend.stop_cycle();
        assert!(matches!(
            send_frames(&first).as_slice(),
            [OutboundFrame::Stop]
        ));
        backend.finish_send_at(first.sends[0], Ok(()), t0);
        let before =
            t0 + REPORT_FRESHNESS_TIMEOUT.saturating_sub(web_time::Duration::from_nanos(1));
        let action = backend
            .cycle
            .stop(backend.controller.test())
            .expect("repeated Stop");
        let prepared = backend
            .controller
            .prepare_command_at(ApiCommand::Stop, before)
            .expect("Stop");
        assert!(prepared.frame().is_none());
        let completed = backend.command_succeeded_at(prepared, Some(action), true, before);
        assert!(completed.sends.is_empty());
        assert!(
            completed
                .events
                .iter()
                .any(|event| matches!(event, BackendEvent::CommandSucceeded))
        );
        assert_eq!(backend.controller.test().state, TestState::Stopping);
        assert!(backend.controller.device().activity_known);
        assert_eq!(backend.cycle.status().state, CycleState::Stopping);
    }

    #[test]
    fn queued_explicit_stop_remains_authorized_at_wire_and_ack_after_expiry() {
        use crate::controller::REPORT_FRESHNESS_TIMEOUT;
        let mut backend = connected_backend();
        let t0 = Instant::now();
        backend.report_at(report(ReportState::Idle, 0), true, t0);
        let start = backend.command(ApiCommand::Start(config()));
        finish_success(&mut backend, &start);
        backend.report_at(report(ReportState::Active, 0), true, t0);
        let stop = backend.command(ApiCommand::Stop).sends[0];
        let boundary = t0 + REPORT_FRESHNESS_TIMEOUT;
        backend.tick_at(boundary);
        assert_eq!(
            backend.controller.test().state,
            TestState::RecoveredUncertain
        );
        let (_, allowed) = backend.authorize_send_at(stop, boundary);
        assert!(allowed);
        let ack = backend.finish_send_at(stop, Ok(()), boundary);
        assert!(
            !ack.events
                .iter()
                .any(|event| matches!(event, BackendEvent::CommandError(_)))
        );
        assert_eq!(backend.controller.test().state, TestState::Stopping);
        assert!(!backend.controller.is_running_owned());
        assert!(!backend.controller.device().activity_known);
        assert_eq!(backend.connection, ServerConnectionState::Connected);
    }

    #[test]
    fn due_and_queued_timer_sync_are_suppressed_by_report_age() {
        use crate::controller::REPORT_FRESHNESS_TIMEOUT;
        let mut backend = connected_backend();
        let t0 = Instant::now();
        backend.report_at(report(ReportState::Idle, 0), true, t0);
        let output = backend.command(ApiCommand::Start(config()));
        finish_success(&mut backend, &output);
        backend.report_at(report(ReportState::Active, 0), true, t0);
        backend.set_elapsed_for_test(60);
        let due = backend.tick_at(t0);
        assert!(matches!(
            send_frames(&due).as_slice(),
            [OutboundFrame::TimerSync(1)]
        ));
        assert!(
            !backend
                .authorize_send_at(due.sends[0], t0 + REPORT_FRESHNESS_TIMEOUT)
                .1
        );
        assert!(
            backend
                .tick_at(t0 + web_time::Duration::from_secs(65))
                .sends
                .is_empty()
        );
        backend.report_at(
            report(ReportState::Active, 2),
            true,
            t0 + web_time::Duration::from_secs(66),
        );
        assert!(
            backend
                .tick_at(t0 + web_time::Duration::from_secs(66))
                .sends
                .is_empty()
        );
    }

    #[test]
    fn firmware_observation_is_fresh_and_can_settle_without_sampling() {
        use crate::controller::REPORT_FRESHNESS_TIMEOUT;
        let mut backend = connected_backend();
        let start = backend.start_cycle(cycle_request(recipe(vec![
            device_step(100),
            CycleStep::Rest {
                duration_seconds: 65,
            },
        ])));
        finish_success(&mut backend, &start);
        let t0 = Instant::now();
        backend.report_at(report(ReportState::Active, 0), true, t0);
        let mut firmware = report(ReportState::Active, 1);
        firmware.firmware_version = Some("3.0.2".to_owned());
        let observed = backend.report_at(
            firmware.clone(),
            false,
            t0 + web_time::Duration::from_secs(9),
        );
        assert!(!observed.events.iter().any(|event| matches!(
            event,
            BackendEvent::Sample(_) | BackendEvent::CycleSample(_)
        )));
        assert!(
            backend
                .tick_at(t0 + REPORT_FRESHNESS_TIMEOUT)
                .sends
                .is_empty()
        );
        assert!(backend.controller.device().activity_known);
        backend.report_at(
            report(ReportState::Finished, 1),
            true,
            t0 + web_time::Duration::from_secs(10),
        );
        assert_eq!(backend.cycle.status().state, CycleState::Settling);
        firmware.state = ReportState::InactiveUnknown;
        firmware.current_ma = 0;
        let settled = backend.report_at(firmware, false, t0 + web_time::Duration::from_secs(11));
        assert!(settled.sends.is_empty());
        assert!(!settled.events.iter().any(|event| matches!(
            event,
            BackendEvent::Sample(_) | BackendEvent::CycleSample(_)
        )));
        assert_eq!(backend.cycle.status().state, CycleState::Resting);
        let expired = backend.tick_at(t0 + web_time::Duration::from_secs(21));
        assert!(expired.sends.is_empty());
        assert_eq!(backend.cycle.status().state, CycleState::Interrupted);
    }

    #[test]
    fn settling_silence_interrupts_before_recovered_zero_current_can_advance() {
        use crate::controller::REPORT_FRESHNESS_TIMEOUT;
        let mut backend = connected_backend();
        let start = backend.start_cycle(cycle_request(recipe(vec![
            device_step(100),
            device_step(200),
        ])));
        finish_success(&mut backend, &start);
        let t0 = Instant::now();
        backend.report_at(report(ReportState::Active, 0), true, t0);
        backend.report_at(report(ReportState::Finished, 1), true, t0);
        assert_eq!(backend.cycle.status().state, CycleState::Settling);
        let recovered = backend.report_at(
            report(ReportState::Idle, 1),
            true,
            t0 + REPORT_FRESHNESS_TIMEOUT,
        );
        assert!(recovered.sends.is_empty());
        assert_eq!(backend.cycle.status().state, CycleState::Interrupted);
        assert_eq!(backend.cycle.status().step_index, 0);
    }

    #[test]
    fn named_manual_start_and_rename_manage_metadata_without_extra_frames() {
        let mut backend = connected_backend();
        let request = backend.start_test(start_request(Some("  first run  ")));
        assert_eq!(request.sends.len(), 1);
        let started = finish_success(&mut backend, &request);
        let snapshot = started
            .events
            .iter()
            .find_map(|event| match event {
                BackendEvent::Snapshot(snapshot) => Some(snapshot),
                _ => None,
            })
            .expect("start snapshot");
        assert_eq!(snapshot.current_run.id.as_deref(), Some("local-run-1"));
        assert_eq!(snapshot.current_run.name.as_deref(), Some("first run"));
        assert!(snapshot.current_run.cycle.is_none());

        let renamed = backend.rename_run(
            "local-run-1",
            RenameRequest {
                name: Some("  renamed  ".to_owned()),
            },
        );
        assert!(renamed.sends.is_empty());
        assert_eq!(
            state(&renamed).update.current_run.name.as_deref(),
            Some("renamed")
        );
        assert!(matches!(
            renamed.events.last(),
            Some(BackendEvent::CommandSucceeded)
        ));

        let cleared = backend.rename_run("local-run-1", RenameRequest::default());
        assert!(cleared.sends.is_empty());
        assert_eq!(state(&cleared).update.current_run.name, None);
    }

    #[test]
    fn named_cycle_has_unnamed_contextual_child_and_rename_has_no_frame() {
        let mut backend = connected_backend();
        let started = backend.start_cycle(StartCycleRequest {
            recipe: recipe(vec![device_step(1000)]),
            name: Some("  capacity cycle  ".to_owned()),
        });
        assert_eq!(started.sends.len(), 1);
        let snapshot = snapshot(&started);
        assert_eq!(snapshot.cycle.name.as_deref(), Some("capacity cycle"));
        assert_eq!(snapshot.current_run.id.as_deref(), Some("local-run-1"));
        assert_eq!(snapshot.current_run.name, None);
        let context = snapshot
            .current_run
            .cycle
            .as_ref()
            .expect("cycle child context");
        assert_eq!(context.execution_id, "local-cycle-1");
        assert_eq!(context.repeat_index, 0);
        assert_eq!(context.step_index, 0);

        let renamed = backend.rename_cycle(
            "local-cycle-1",
            RenameRequest {
                name: Some("second name".to_owned()),
            },
        );
        assert!(renamed.sends.is_empty());
        assert_eq!(
            state(&renamed).update.cycle.name.as_deref(),
            Some("second name")
        );
        let rejected = backend.rename_run("local-run-1", RenameRequest::default());
        assert!(rejected.sends.is_empty());
        assert!(matches!(
            rejected.events.as_slice(),
            [BackendEvent::CommandError(error)] if error.contains("cycle child")
        ));
    }

    #[test]
    fn local_saved_recipe_start_captures_provenance_and_keeps_child_unnamed() {
        let mut backend = connected_backend();
        let recipe = recipe(vec![device_step(1000)]);
        let reference = SavedRecipeReference {
            id: "local-recipe-7".to_owned(),
            name: "Capacity α".to_owned(),
            revision: 3,
        };

        let started = backend.start_saved_recipe(
            recipe.clone(),
            reference.clone(),
            Some(" Cell 4 ".to_owned()),
        );
        assert_eq!(started.sends.len(), 1);
        let snapshot = snapshot(&started);
        assert_eq!(snapshot.cycle.recipe.as_ref(), Some(&recipe));
        assert_eq!(snapshot.cycle.saved_recipe.as_ref(), Some(&reference));
        assert_eq!(snapshot.cycle.name.as_deref(), Some("Cell 4"));
        assert_eq!(snapshot.current_run.name, None);
        assert_eq!(
            snapshot
                .current_run
                .cycle
                .as_ref()
                .map(|context| context.execution_id.as_str()),
            snapshot.cycle.execution_id.as_deref()
        );
    }

    #[test]
    fn naming_does_not_change_ordinary_command_frames_or_metadata() {
        let mut backend = connected_backend();
        let start = backend.start_test(start_request(Some("stable")));
        finish_success(&mut backend, &start);
        backend.report(report(ReportState::Active, 1), true);
        let before = backend.current_run.clone();

        let adjust = backend.command(ApiCommand::Adjust(config_with_current(1500)));
        assert!(matches!(
            send_frames(&adjust).as_slice(),
            [OutboundFrame::AdjustConstantCurrentDischarge(1500, 3000, 0)]
        ));
        let adjusted = finish_success(&mut backend, &adjust);
        assert_eq!(state(&adjusted).update.current_run, before);

        let stop = backend.command(ApiCommand::Stop);
        assert!(matches!(
            send_frames(&stop).as_slice(),
            [OutboundFrame::Stop]
        ));
        let stopped = finish_success(&mut backend, &stop);
        assert_eq!(state(&stopped).update.current_run, before);
        backend.report(report(ReportState::Idle, 1), true);

        let resume = backend.resume(config_with_current(1500));
        assert!(matches!(
            send_frames(&resume).as_slice(),
            [OutboundFrame::ContinueConstantCurrentDischarge(
                1500, 3000, 0
            )]
        ));
        let resumed = finish_success(&mut backend, &resume);
        assert_eq!(state(&resumed).update.current_run, before);
    }

    #[test]
    fn failed_new_start_never_attaches_its_name_to_the_previous_run() {
        let mut backend = connected_backend();
        let first = backend.start_test(start_request(Some("first")));
        finish_success(&mut backend, &first);
        backend.report(report(ReportState::Active, 1), true);
        backend.report(report(ReportState::Finished, 1), true);
        backend.report(report(ReportState::Idle, 1), true);

        let second = backend.start_test(start_request(Some("attempted second")));
        assert_eq!(second.sends.len(), 1);
        let failed = finish_failure(&mut backend, &second);
        let snapshot = failed
            .events
            .iter()
            .find_map(|event| match event {
                BackendEvent::Snapshot(snapshot) => Some(snapshot),
                _ => None,
            })
            .expect("failed named start snapshot");
        assert_eq!(snapshot.current_run.id.as_deref(), Some("local-run-2"));
        assert_eq!(
            snapshot.current_run.name.as_deref(),
            Some("attempted second")
        );
        assert!(snapshot.history.is_empty());
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
        let snapshot = snapshot(&failed);
        assert_eq!(snapshot.test.state, TestState::RecoveredUncertain);
        assert!(
            snapshot
                .test
                .result
                .as_deref()
                .is_some_and(|reason| reason.contains("start outcome is unknown"))
        );
        assert!(!snapshot.device.activity_known);
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
        let first = backend.start_cycle(cycle_request(recipe(vec![
            device_step(1000),
            device_step(1500),
        ])));
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
        let first = backend.start_cycle(cycle_request(recipe(vec![
            device_step(1000),
            CycleStep::Rest {
                duration_seconds: 1,
            },
            device_step(1500),
        ])));
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
        let first = backend.start_cycle(cycle_request(CycleRecipe {
            steps: vec![device_step(1000)],
            repeat_count: 2,
        }));
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
        let started = backend.start_cycle(cycle_request(recipe(vec![CycleStep::Rest {
            duration_seconds: 0,
        }])));
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
        let start = backend.start_cycle(cycle_request(recipe(vec![device_step(1000)])));
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
        let started = backend.start_cycle(cycle_request(recipe(vec![CycleStep::Rest {
            duration_seconds: 60,
        }])));
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
        let start = backend.start_cycle(cycle_request(recipe(vec![device_step(1000)])));
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
        let start = backend.start_cycle(cycle_request(recipe(vec![device_step(1000)])));
        assert!(matches!(
            start.events.as_slice(),
            [BackendEvent::Snapshot(snapshot)] if snapshot.history.is_empty()
        ));

        let failed = finish_failure(&mut backend, &start);
        let snapshot = failed
            .events
            .iter()
            .find_map(|event| match event {
                BackendEvent::Snapshot(snapshot) => Some(snapshot),
                _ => None,
            })
            .expect("failed cycle child start snapshot");
        assert_eq!(snapshot.cycle.state, CycleState::Interrupted);
        assert_eq!(snapshot.test.state, TestState::RecoveredUncertain);
        assert!(snapshot.history.is_empty());
        assert!(matches!(
            failed.events.last(),
            Some(BackendEvent::CommandError(error)) if error.contains("write failure")
        ));
    }

    #[test]
    fn interrupted_safety_stop_retries_after_reconnect() {
        let mut backend = connected_backend();
        let start = backend.start_cycle(cycle_request(recipe(vec![device_step(1000)])));
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
                None,
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
