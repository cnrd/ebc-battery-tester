//! GUI-independent physical test lifecycle, metrics, and command policy.

use std::time::{Duration, Instant};

use crate::core::{
    ApiCommand, CalibrationCommand, Capabilities, DeviceState, Sample, TestConfiguration,
    TestState, TestStatus,
};
use crate::device::{self, DeviceMode, ModeReportState, OutboundFrame};

const CAPACITY_MODULUS_MAH: u64 = 57_600;
const CAPACITY_WRAP_HIGH_WATER: u16 = 43_200;
const CAPACITY_WRAP_LOW_WATER: u16 = 14_400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControllerMode {
    Direct,
    Server,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhysicalState {
    Unknown,
    Active,
    Inactive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportState {
    Idle,
    Active,
    Finished,
    InactiveUnknown,
}

impl From<ModeReportState> for ReportState {
    fn from(state: ModeReportState) -> Self {
        match state {
            ModeReportState::Idle => Self::Idle,
            ModeReportState::Active => Self::Active,
            ModeReportState::Finished => Self::Finished,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeviceReport {
    pub mode: DeviceMode,
    pub state: ReportState,
    pub voltage_mv: u16,
    pub current_ma: u16,
    pub capacity_mah: u16,
    pub model: String,
    pub firmware_version: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Measurement {
    pub elapsed_seconds: u64,
    pub voltage_mv: u16,
    pub current_ma: u16,
    pub capacity_mah: u64,
    pub energy_wh: f64,
    pub mode: DeviceMode,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReportOutcome {
    pub transitioned_to_inactive: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lifecycle {
    Idle,
    RecoveredUncertain,
    Starting,
    RunningOwned,
    Stopping,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandKind {
    Start,
    Stop,
    Adjust,
    Resume,
    Calibration,
}

#[derive(Clone, Copy, Debug)]
pub struct PreparedCommand {
    command: ApiCommand,
    frame: Option<OutboundFrame>,
    kind: CommandKind,
}

impl PreparedCommand {
    pub fn frame(&self) -> Option<OutboundFrame> {
        self.frame
    }

    pub fn kind(&self) -> CommandKind {
        self.kind
    }
}

#[derive(Default)]
struct CalibrationStaging {
    references: [bool; 4],
}

impl CalibrationStaging {
    fn complete(&self) -> bool {
        self.references.iter().all(|staged| *staged)
    }
}

struct TestClock {
    running_since: Option<Instant>,
    accumulated: Duration,
    last_sync_minute: u64,
}

impl TestClock {
    fn new(elapsed_seconds: u64) -> Self {
        Self {
            running_since: None,
            accumulated: Duration::from_secs(elapsed_seconds),
            last_sync_minute: elapsed_seconds / 60,
        }
    }

    fn start_fresh(&mut self) {
        self.running_since = None;
        self.accumulated = Duration::ZERO;
        self.last_sync_minute = 0;
    }

    fn resume(&mut self) {
        if self.running_since.is_none() {
            self.running_since = Some(Instant::now());
        }
    }

    fn stop(&mut self) {
        if let Some(started) = self.running_since.take() {
            self.accumulated += started.elapsed();
        }
    }

    fn replace(&mut self, elapsed_seconds: u64, running: bool) {
        self.accumulated = Duration::from_secs(elapsed_seconds);
        self.running_since = running.then(Instant::now);
        self.last_sync_minute = elapsed_seconds / 60;
    }

    fn elapsed(&self) -> Duration {
        self.running_since.map_or(self.accumulated, |started| {
            self.accumulated + started.elapsed()
        })
    }

    fn next_timer_sync(&mut self) -> Option<u16> {
        let minute = self.elapsed().as_secs() / 60;
        if self.running_since.is_some() && minute > self.last_sync_minute {
            self.last_sync_minute = minute;
            (minute <= u64::from(device::MAX_TIMER_SYNC_MINUTES)).then_some(minute as u16)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy)]
struct EnergyReading {
    elapsed_seconds: f64,
    power_w: f64,
}

struct EnergyAccumulator {
    energy_wh: f64,
    previous: Option<EnergyReading>,
}

impl EnergyAccumulator {
    fn from_state(test: &TestStatus, last_sample: Option<&Sample>) -> Self {
        let previous = last_sample.map(|sample| EnergyReading {
            elapsed_seconds: sample.elapsed_seconds as f64,
            power_w: sample.voltage_mv as f64 * sample.current_ma as f64 / 1_000_000.0,
        });
        Self {
            energy_wh: test.energy_wh,
            previous,
        }
    }

    fn reset(&mut self) {
        self.energy_wh = 0.0;
        self.previous = None;
    }

    fn break_gap(&mut self) {
        self.previous = None;
    }

    fn add(&mut self, elapsed_seconds: f64, voltage_mv: u16, current_ma: u16) -> f64 {
        let power_w = voltage_mv as f64 * current_ma as f64 / 1_000_000.0;
        if let Some(previous) = self.previous {
            let delta_seconds = elapsed_seconds - previous.elapsed_seconds;
            if delta_seconds > 0.0 {
                self.energy_wh += (previous.power_w + power_w) * 0.5 * delta_seconds / 3600.0;
            }
        }
        self.previous = Some(EnergyReading {
            elapsed_seconds,
            power_w,
        });
        self.energy_wh
    }
}

struct CapacityAccumulator {
    capacity_mah: u64,
    previous_raw: Option<u16>,
}

impl CapacityAccumulator {
    fn from_state(device: &DeviceState, test: &TestStatus, last_sample: Option<&Sample>) -> Self {
        let persisted_capacity = test.capacity_mah.unwrap_or(0);
        let capacity_mah = last_sample.map_or(persisted_capacity, |sample| {
            sample.capacity_mah.max(persisted_capacity)
        });
        let previous_raw = last_sample.map_or(device.capacity_mah, |sample| {
            u16::try_from(sample.capacity_mah % CAPACITY_MODULUS_MAH).ok()
        });
        Self {
            capacity_mah,
            previous_raw,
        }
    }

    fn reset(&mut self) {
        self.capacity_mah = 0;
        self.previous_raw = None;
    }

    fn observe(&mut self, raw: u16, allow_wrap: bool) -> u64 {
        let Some(previous) = self.previous_raw else {
            self.previous_raw = Some(raw);
            self.capacity_mah = self.capacity_mah.max(u64::from(raw));
            return self.capacity_mah;
        };
        if raw >= previous {
            self.capacity_mah = self.capacity_mah.saturating_add(u64::from(raw - previous));
            self.previous_raw = Some(raw);
        } else if previous >= CAPACITY_WRAP_HIGH_WATER && raw <= CAPACITY_WRAP_LOW_WATER {
            if allow_wrap {
                self.capacity_mah = self
                    .capacity_mah
                    .saturating_add(CAPACITY_MODULUS_MAH - u64::from(previous) + u64::from(raw));
            }
            self.previous_raw = Some(raw);
        }
        self.capacity_mah
    }
}

pub struct TestController {
    mode: ControllerMode,
    connected: bool,
    device: DeviceState,
    test: TestStatus,
    lifecycle: Lifecycle,
    physical: PhysicalState,
    connection_generation: u64,
    report_generation: Option<u64>,
    calibration_staging: CalibrationStaging,
    clock: TestClock,
    energy: EnergyAccumulator,
    capacity: CapacityAccumulator,
}

impl TestController {
    pub fn new(mode: ControllerMode) -> Self {
        Self::from_state(mode, DeviceState::default(), TestStatus::default(), None)
    }

    pub fn from_state(
        mode: ControllerMode,
        device: DeviceState,
        test: TestStatus,
        last_sample: Option<&Sample>,
    ) -> Self {
        let lifecycle = if test.state == TestState::RecoveredUncertain {
            Lifecycle::RecoveredUncertain
        } else if test.state == TestState::Starting {
            Lifecycle::Starting
        } else if test.state == TestState::Stopping {
            Lifecycle::Stopping
        } else if test.state == TestState::Running && device.active {
            Lifecycle::RunningOwned
        } else {
            Lifecycle::Idle
        };
        Self {
            mode,
            connected: false,
            clock: TestClock::new(test.elapsed_seconds),
            energy: EnergyAccumulator::from_state(&test, last_sample),
            capacity: CapacityAccumulator::from_state(&device, &test, last_sample),
            device,
            test,
            lifecycle,
            physical: PhysicalState::Unknown,
            connection_generation: 0,
            report_generation: None,
            calibration_staging: CalibrationStaging::default(),
        }
    }

    pub fn device(&self) -> &DeviceState {
        &self.device
    }

    pub fn test(&self) -> &TestStatus {
        &self.test
    }

    pub fn set_device_identity(
        &mut self,
        model: Option<String>,
        firmware_version: Option<String>,
        voltage_mv: Option<u16>,
    ) {
        self.device.model = model;
        self.device.firmware_version = firmware_version;
        self.device.voltage_mv = voltage_mv;
    }

    pub fn set_current_ma(&mut self, current_ma: u16) {
        self.device.current_ma = Some(current_ma);
    }

    pub fn elapsed(&self) -> Duration {
        self.clock.elapsed()
    }

    pub fn physical_state(&self) -> PhysicalState {
        self.physical
    }

    pub fn is_starting(&self) -> bool {
        self.lifecycle == Lifecycle::Starting
    }

    pub fn is_stopping(&self) -> bool {
        self.lifecycle == Lifecycle::Stopping
    }

    pub fn is_running_owned(&self) -> bool {
        self.lifecycle == Lifecycle::RunningOwned
    }

    pub fn begin_connection(&mut self, reason: &str) {
        self.invalidate_for_gap(reason);
        self.connection_generation = self.connection_generation.wrapping_add(1);
        self.connected = false;
    }

    pub fn connection_established(&mut self) {
        self.connected = true;
    }

    pub fn disconnect(&mut self, reason: &str) {
        self.connected = false;
        self.invalidate_for_gap(reason);
    }

    pub fn invalidate_for_gap(&mut self, reason: &str) {
        let contradiction = self.lifecycle == Lifecycle::Idle
            && (self.physical == PhysicalState::Active || self.device.active);
        if matches!(
            self.lifecycle,
            Lifecycle::Starting
                | Lifecycle::RunningOwned
                | Lifecycle::Stopping
                | Lifecycle::RecoveredUncertain
        ) || contradiction
        {
            self.lifecycle = Lifecycle::RecoveredUncertain;
            self.test.state = TestState::RecoveredUncertain;
            self.test.result = Some(reason.to_owned());
        }
        self.physical = PhysicalState::Unknown;
        self.report_generation = None;
        self.device.activity_known = false;
        self.device.active = false;
        self.calibration_staging = CalibrationStaging::default();
        self.clock.stop();
        self.energy.break_gap();
        self.update_elapsed();
    }

    pub fn replace_authoritative(
        &mut self,
        connected: bool,
        device: DeviceState,
        test: TestStatus,
    ) {
        self.connected = connected;
        self.physical = if !device.activity_known {
            PhysicalState::Unknown
        } else if device.active {
            PhysicalState::Active
        } else {
            PhysicalState::Inactive
        };
        self.report_generation = device.activity_known.then_some(self.connection_generation);
        self.lifecycle = match test.state {
            TestState::Starting => Lifecycle::Starting,
            TestState::Running if device.active => Lifecycle::RunningOwned,
            TestState::Stopping => Lifecycle::Stopping,
            TestState::RecoveredUncertain => Lifecycle::RecoveredUncertain,
            _ => Lifecycle::Idle,
        };
        let running = test.state == TestState::Running && device.activity_known && device.active;
        self.clock.replace(test.elapsed_seconds, running);
        self.device = device;
        self.test = test;
    }

    pub fn capabilities(&self) -> Capabilities {
        let fresh_inactive = self.has_fresh_report(PhysicalState::Inactive);
        let fresh_active = self.has_fresh_report(PhysicalState::Active);
        let live_voltage = self.device.voltage_mv.unwrap_or(0) > 0;
        let start = self.lifecycle == Lifecycle::Idle
            && fresh_inactive
            && live_voltage
            && matches!(
                self.test.state,
                TestState::Idle | TestState::Stopped | TestState::Completed
            );
        let resume = self.lifecycle == Lifecycle::Idle
            && fresh_inactive
            && live_voltage
            && self.test.state == TestState::Stopped;
        let show_stop = self.device.active
            || matches!(
                self.test.state,
                TestState::Starting
                    | TestState::Running
                    | TestState::Stopping
                    | TestState::RecoveredUncertain
            );
        let stop = show_stop && self.lifecycle != Lifecycle::Stopping && self.connected;
        let adjust = self.lifecycle == Lifecycle::RunningOwned
            && fresh_active
            && self.test.state == TestState::Running
            && self.device.mode == Some(DeviceMode::DischargeConstantCurrent);
        let calibration_state_allowed = !matches!(
            self.lifecycle,
            Lifecycle::RecoveredUncertain | Lifecycle::Starting | Lifecycle::Stopping
        );
        let calibrate_voltage = self.connected
            && self.report_generation == Some(self.connection_generation)
            && self.device.activity_known
            && calibration_state_allowed
            && live_voltage;
        let calibrate_current = calibrate_voltage
            && self.lifecycle == Lifecycle::RunningOwned
            && fresh_active
            && self.device.mode == Some(DeviceMode::DischargeConstantCurrent);
        Capabilities {
            start,
            resume,
            stop,
            show_stop,
            adjust,
            calibrate_voltage,
            calibrate_current,
            confirm_calibration: calibrate_voltage
                && (self.mode == ControllerMode::Direct || self.calibration_staging.complete()),
        }
    }

    /// Validates a semantic command and returns its optional protocol action.
    ///
    /// # Errors
    /// Returns an error when the current connection, report freshness, lifecycle,
    /// configuration, or calibration state does not permit the command.
    #[expect(clippy::too_many_lines)]
    pub fn prepare_command(&self, command: ApiCommand) -> Result<PreparedCommand, String> {
        if !self.connected {
            return Err("device is not connected".to_owned());
        }
        let (frame, kind) = match command {
            ApiCommand::Connect | ApiCommand::Disconnect => {
                return Err("connection commands are transport-owned".to_owned());
            }
            ApiCommand::Start(config) => {
                if !self.capabilities().start {
                    return Err(
                        "start requires a fresh current-connection inactive report".to_owned()
                    );
                }
                config.validate().map_err(|error| error.to_string())?;
                (Some(test_frame(&config, false)), CommandKind::Start)
            }
            ApiCommand::Adjust(config) => {
                if !self.capabilities().adjust {
                    return Err(
                        "adjustment requires a confirmed backend-owned running test".to_owned()
                    );
                }
                config.validate().map_err(|error| error.to_string())?;
                let TestConfiguration::DischargeConstantCurrent {
                    current_ma,
                    cutoff_voltage_mv,
                    cutoff_time_min,
                } = config
                else {
                    return Err("only constant-current discharge can be adjusted".to_owned());
                };
                (
                    Some(OutboundFrame::AdjustConstantCurrentDischarge(
                        current_ma,
                        cutoff_voltage_mv,
                        cutoff_time_min,
                    )),
                    CommandKind::Adjust,
                )
            }
            ApiCommand::Stop => {
                if self.lifecycle == Lifecycle::Stopping {
                    (None, CommandKind::Stop)
                } else {
                    if !self.capabilities().stop {
                        return Err("there is no active or uncertain test to stop".to_owned());
                    }
                    (Some(OutboundFrame::Stop), CommandKind::Stop)
                }
            }
            ApiCommand::Resume => {
                let config = self
                    .test
                    .config
                    .ok_or_else(|| "there is no test configuration to resume".to_owned())?;
                return self.prepare_resume(config);
            }
            ApiCommand::Calibration(calibration) => {
                calibration.validate().map_err(|error| error.to_string())?;
                let capabilities = self.capabilities();
                let frame = match calibration {
                    CalibrationCommand::VoltageLow(value)
                    | CalibrationCommand::VoltageHigh(value)
                        if !capabilities.calibrate_voltage =>
                    {
                        return Err(
                            "device must provide fresh live voltage before calibration".to_owned()
                        );
                    }
                    CalibrationCommand::CurrentLow(_) | CalibrationCommand::CurrentHigh(_)
                        if !capabilities.calibrate_current =>
                    {
                        return Err(
                            "constant-current discharge must be active for current calibration"
                                .to_owned(),
                        );
                    }
                    CalibrationCommand::Confirm if !capabilities.confirm_calibration => {
                        return Err(
                            "all four calibration references must be staged on this connection"
                                .to_owned(),
                        );
                    }
                    CalibrationCommand::VoltageLow(value) => {
                        OutboundFrame::CalibrateVoltageLow(value)
                    }
                    CalibrationCommand::VoltageHigh(value) => {
                        OutboundFrame::CalibrateVoltageHigh(value)
                    }
                    CalibrationCommand::CurrentLow(value) => {
                        OutboundFrame::CalibrateCurrentLow(value)
                    }
                    CalibrationCommand::CurrentHigh(value) => {
                        OutboundFrame::CalibrateCurrentHigh(value)
                    }
                    CalibrationCommand::Confirm => OutboundFrame::CalibrateConfirm,
                };
                (Some(frame), CommandKind::Calibration)
            }
        };
        Ok(PreparedCommand {
            command,
            frame,
            kind,
        })
    }

    /// Validates a resume using the supplied direct-client settings.
    ///
    /// # Errors
    /// Returns an error unless the current test is confirmed stopped and the
    /// supplied configuration is valid.
    pub fn prepare_resume(&self, config: TestConfiguration) -> Result<PreparedCommand, String> {
        if !self.capabilities().resume {
            return Err("resume requires a confirmed inactive stopped test".to_owned());
        }
        config.validate().map_err(|error| error.to_string())?;
        Ok(PreparedCommand {
            command: ApiCommand::Resume,
            frame: Some(test_frame(&config, true)),
            kind: CommandKind::Resume,
        })
    }

    pub fn commit_command(&mut self, prepared: PreparedCommand, started_at_utc: Option<String>) {
        match prepared.command {
            ApiCommand::Start(config) => {
                self.clock.start_fresh();
                self.energy.reset();
                self.capacity.reset();
                self.lifecycle = Lifecycle::Starting;
                self.physical = PhysicalState::Unknown;
                self.report_generation = None;
                self.device.activity_known = false;
                self.device.active = false;
                self.device.mode = Some(config.mode());
                self.test.state = TestState::Starting;
                self.test.config = Some(config);
                self.test.started_at_utc = started_at_utc;
                self.test.elapsed_seconds = 0;
                self.test.result = None;
                self.test.capacity_mah = None;
                self.test.energy_wh = 0.0;
            }
            ApiCommand::Stop if self.lifecycle != Lifecycle::Stopping => {
                self.clock.stop();
                self.lifecycle = Lifecycle::Stopping;
                self.test.state = TestState::Stopping;
                self.test.result = None;
                self.energy.break_gap();
                self.update_elapsed();
            }
            ApiCommand::Adjust(config) => self.test.config = Some(config),
            ApiCommand::Resume => {
                self.lifecycle = Lifecycle::Starting;
                self.physical = PhysicalState::Unknown;
                self.report_generation = None;
                self.device.activity_known = false;
                self.device.active = false;
                self.test.state = TestState::Starting;
                self.test.result = None;
            }
            ApiCommand::Calibration(command) => match command {
                CalibrationCommand::VoltageLow(_) => {
                    self.calibration_staging.references[0] = true;
                }
                CalibrationCommand::VoltageHigh(_) => {
                    self.calibration_staging.references[1] = true;
                }
                CalibrationCommand::CurrentLow(_) => {
                    self.calibration_staging.references[2] = true;
                }
                CalibrationCommand::CurrentHigh(_) => {
                    self.calibration_staging.references[3] = true;
                }
                CalibrationCommand::Confirm => {
                    self.calibration_staging = CalibrationStaging::default();
                }
            },
            ApiCommand::Connect | ApiCommand::Disconnect | ApiCommand::Stop => {}
        }
    }

    pub fn start_write_failed(&mut self) {
        self.lifecycle = Lifecycle::RecoveredUncertain;
        self.test.state = TestState::RecoveredUncertain;
        self.test.result = Some("start outcome is unknown after a write failure".to_owned());
    }

    #[expect(clippy::too_many_lines)]
    pub fn report(&mut self, report: DeviceReport) -> (ReportOutcome, Option<Measurement>) {
        let previous_lifecycle = self.lifecycle;
        let active = report.state == ReportState::Active;
        self.report_generation = Some(self.connection_generation);
        self.physical = if active {
            PhysicalState::Active
        } else {
            PhysicalState::Inactive
        };
        self.device.mode = Some(report.mode);
        self.device.activity_known = true;
        self.device.active = active;
        self.device.voltage_mv = Some(report.voltage_mv);
        self.device.current_ma = Some(report.current_ma);
        self.device.capacity_mah = Some(report.capacity_mah);
        self.device.model = Some(report.model);
        if report.firmware_version.is_some() {
            self.device.firmware_version = report.firmware_version;
        }

        let mut outcome = ReportOutcome::default();
        let measurement = if active {
            let owns_metrics = match previous_lifecycle {
                Lifecycle::Starting => {
                    self.lifecycle = Lifecycle::RunningOwned;
                    self.test.state = TestState::Running;
                    self.test.result = None;
                    self.clock.resume();
                    true
                }
                Lifecycle::RunningOwned => {
                    self.clock.resume();
                    true
                }
                Lifecycle::Idle | Lifecycle::RecoveredUncertain
                    if self.mode == ControllerMode::Server =>
                {
                    self.lifecycle = Lifecycle::RecoveredUncertain;
                    self.test.state = TestState::RecoveredUncertain;
                    self.test.result =
                        Some("hardware reports an active test not owned by this server".to_owned());
                    self.clock.stop();
                    self.energy.break_gap();
                    false
                }
                Lifecycle::Stopping | Lifecycle::Idle | Lifecycle::RecoveredUncertain => {
                    self.clock.stop();
                    self.energy.break_gap();
                    false
                }
            };
            let elapsed = self.clock.elapsed();
            let (capacity_mah, energy_wh) = if self.mode == ControllerMode::Server {
                let energy_wh = if owns_metrics {
                    self.energy
                        .add(elapsed.as_secs_f64(), report.voltage_mv, report.current_ma)
                } else {
                    self.test.energy_wh
                };
                (
                    self.capacity.observe(report.capacity_mah, owns_metrics),
                    energy_wh,
                )
            } else {
                (
                    u64::from(report.capacity_mah),
                    report.voltage_mv as f64 * report.capacity_mah as f64 / 1_000_000.0,
                )
            };
            self.test.capacity_mah = Some(capacity_mah);
            self.test.energy_wh = energy_wh;
            Some(Measurement {
                elapsed_seconds: elapsed.as_secs(),
                voltage_mv: report.voltage_mv,
                current_ma: report.current_ma,
                capacity_mah,
                energy_wh,
                mode: report.mode,
            })
        } else {
            self.clock.stop();
            self.energy.break_gap();
            self.update_elapsed();
            if previous_lifecycle != Lifecycle::Starting {
                self.test.capacity_mah = Some(if self.mode == ControllerMode::Server {
                    self.capacity.observe(
                        report.capacity_mah,
                        previous_lifecycle == Lifecycle::RunningOwned,
                    )
                } else {
                    u64::from(report.capacity_mah)
                });
                if self.mode == ControllerMode::Direct {
                    self.test.energy_wh =
                        report.voltage_mv as f64 * report.capacity_mah as f64 / 1_000_000.0;
                }
            }
            match previous_lifecycle {
                Lifecycle::RecoveredUncertain => {
                    self.lifecycle = Lifecycle::Idle;
                    self.test.state = TestState::Stopped;
                    self.test.result =
                        Some("recovered previous test; hardware reports inactive".to_owned());
                    outcome.transitioned_to_inactive = true;
                }
                Lifecycle::Starting | Lifecycle::Idle => {}
                Lifecycle::RunningOwned => match report.state {
                    ReportState::Finished => {
                        self.lifecycle = Lifecycle::Idle;
                        self.test.state = TestState::Completed;
                        self.test.result = Some("device reported test complete".to_owned());
                        outcome.transitioned_to_inactive = true;
                    }
                    ReportState::Idle => {
                        self.lifecycle = Lifecycle::Idle;
                        self.test.state = TestState::Stopped;
                        self.test.result = Some("device reported test idle".to_owned());
                        outcome.transitioned_to_inactive = true;
                    }
                    ReportState::InactiveUnknown => {}
                    ReportState::Active => unreachable!(),
                },
                Lifecycle::Stopping => {
                    self.lifecycle = Lifecycle::Idle;
                    self.test.state = TestState::Stopped;
                    self.test.result = Some("stop confirmed by hardware".to_owned());
                    outcome.transitioned_to_inactive = true;
                }
            }
            None
        };
        self.update_elapsed();
        (outcome, measurement)
    }

    pub fn next_timer_sync(&mut self) -> Option<u16> {
        if self.lifecycle == Lifecycle::RunningOwned
            && self.has_fresh_report(PhysicalState::Active)
            && self.test.state == TestState::Running
        {
            self.clock.next_timer_sync()
        } else {
            None
        }
    }

    pub fn update_elapsed(&mut self) {
        self.test.elapsed_seconds = self.clock.elapsed().as_secs();
    }

    #[cfg(all(test, feature = "gui"))]
    pub(crate) fn set_elapsed_for_test(&mut self, seconds: u64) {
        self.clock.accumulated = Duration::from_secs(seconds);
    }

    fn has_fresh_report(&self, physical: PhysicalState) -> bool {
        self.connected
            && self.report_generation == Some(self.connection_generation)
            && self.physical == physical
            && self.device.activity_known
    }
}

pub fn test_frame(config: &TestConfiguration, resume: bool) -> OutboundFrame {
    match (*config, resume) {
        (
            TestConfiguration::DischargeConstantCurrent {
                current_ma,
                cutoff_voltage_mv,
                cutoff_time_min,
            },
            false,
        ) => OutboundFrame::StartConstantCurrentDischarge(
            current_ma,
            cutoff_voltage_mv,
            cutoff_time_min,
        ),
        (
            TestConfiguration::DischargeConstantCurrent {
                current_ma,
                cutoff_voltage_mv,
                cutoff_time_min,
            },
            true,
        ) => OutboundFrame::ContinueConstantCurrentDischarge(
            current_ma,
            cutoff_voltage_mv,
            cutoff_time_min,
        ),
        (
            TestConfiguration::DischargeConstantPower {
                power_w,
                cutoff_voltage_mv,
                cutoff_time_min,
            },
            false,
        ) => {
            OutboundFrame::StartConstantPowerDischarge(power_w, cutoff_voltage_mv, cutoff_time_min)
        }
        (
            TestConfiguration::DischargeConstantPower {
                power_w,
                cutoff_voltage_mv,
                cutoff_time_min,
            },
            true,
        ) => OutboundFrame::ContinueConstantPowerDischarge(
            power_w,
            cutoff_voltage_mv,
            cutoff_time_min,
        ),
        (
            TestConfiguration::ChargeConstantVoltage {
                current_ma,
                voltage_mv,
                cutoff_current_ma,
            },
            false,
        ) => OutboundFrame::StartConstantVoltageCharge(current_ma, voltage_mv, cutoff_current_ma),
        (
            TestConfiguration::ChargeConstantVoltage {
                current_ma,
                voltage_mv,
                cutoff_current_ma,
            },
            true,
        ) => {
            OutboundFrame::ContinueConstantVoltageCharge(current_ma, voltage_mv, cutoff_current_ma)
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "controller tests should fail fast")]
mod tests {
    use super::*;

    fn controller() -> TestController {
        let mut controller = TestController::new(ControllerMode::Server);
        controller.begin_connection("connect");
        controller.connection_established();
        controller.report(report(ReportState::Idle, 0));
        controller
    }

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

    fn commit(controller: &mut TestController, command: ApiCommand) {
        let prepared = controller
            .prepare_command(command)
            .expect("prepare command");
        controller.commit_command(prepared, None);
    }

    #[test]
    fn start_and_resume_ignore_buffered_inactive_until_active() {
        let mut controller = controller();
        commit(&mut controller, ApiCommand::Start(config()));
        controller.report(report(ReportState::Idle, 50));
        controller.report(report(ReportState::InactiveUnknown, 50));
        assert_eq!(controller.test.state, TestState::Starting);
        assert_eq!(controller.test.capacity_mah, None);
        controller.report(report(ReportState::Active, 1));
        assert_eq!(controller.test.state, TestState::Running);

        commit(&mut controller, ApiCommand::Stop);
        controller.report(report(ReportState::Idle, 1));
        commit(&mut controller, ApiCommand::Resume);
        controller.report(report(ReportState::Idle, 1));
        assert_eq!(controller.test.state, TestState::Starting);
        controller.report(report(ReportState::Active, 2));
        assert_eq!(controller.test.state, TestState::Running);
    }

    #[test]
    fn normal_report_states_preserve_terminal_meaning() {
        for (state, expected) in [
            (ReportState::Idle, TestState::Stopped),
            (ReportState::Finished, TestState::Completed),
        ] {
            let mut controller = controller();
            commit(&mut controller, ApiCommand::Start(config()));
            controller.report(report(ReportState::Active, 1));
            controller.report(report(state, 2));
            assert_eq!(controller.test.state, expected);
        }

        let mut controller = controller();
        commit(&mut controller, ApiCommand::Start(config()));
        controller.report(report(ReportState::Active, 1));
        controller.report(report(ReportState::InactiveUnknown, 2));
        assert_eq!(controller.test.state, TestState::Running);
    }

    #[test]
    fn observation_gap_revokes_ownership_and_timer_sync() {
        let mut controller = controller();
        commit(&mut controller, ApiCommand::Start(config()));
        assert_eq!(controller.next_timer_sync(), None);
        controller.report(report(ReportState::Active, 1));
        controller.clock.accumulated = Duration::from_secs(60);
        assert_eq!(controller.next_timer_sync(), Some(1));
        controller.invalidate_for_gap("gap");
        assert_eq!(controller.test.state, TestState::RecoveredUncertain);
        assert_eq!(controller.next_timer_sync(), None);
    }

    #[test]
    fn timer_sync_stops_at_canonical_bound() {
        let mut controller = controller();
        commit(&mut controller, ApiCommand::Start(config()));
        controller.report(report(ReportState::Active, 1));
        controller.clock.accumulated =
            Duration::from_secs(u64::from(device::MAX_TIMER_SYNC_MINUTES) * 60);
        assert_eq!(
            controller.next_timer_sync(),
            Some(device::MAX_TIMER_SYNC_MINUTES)
        );
        controller.clock.accumulated += Duration::from_secs(60);
        assert_eq!(controller.next_timer_sync(), None);
    }

    #[test]
    fn capacity_wraps_only_with_owned_metrics_and_ignores_stale_values() {
        let mut controller = controller();
        commit(&mut controller, ApiCommand::Start(config()));
        controller.report(report(ReportState::Active, 57_590));
        controller.report(report(ReportState::Active, 5));
        assert_eq!(controller.test.capacity_mah, Some(57_605));
        controller.report(report(ReportState::Active, 4));
        assert_eq!(controller.test.capacity_mah, Some(57_605));
    }

    #[test]
    fn energy_integration_is_deterministic_and_uses_trapezoids() {
        let mut energy = EnergyAccumulator {
            energy_wh: 0.0,
            previous: None,
        };
        assert!((energy.add(0.0, 10_000, 1_000) - 0.0).abs() < f64::EPSILON);
        assert!((energy.add(3600.0, 10_000, 2_000) - 15.0).abs() < f64::EPSILON);
        assert!((energy.add(3600.0, 10_000, 3_000) - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn capacity_supports_multiple_wraps_and_uncertain_recovery() {
        let mut capacity = CapacityAccumulator {
            capacity_mah: 0,
            previous_raw: None,
        };
        capacity.observe(0, true);
        for _ in 0..5 {
            capacity.observe(57_599, true);
            capacity.observe(0, true);
        }
        assert_eq!(capacity.capacity_mah, 288_000);

        let mut uncertain = CapacityAccumulator {
            capacity_mah: 57_599,
            previous_raw: Some(57_599),
        };
        assert_eq!(uncertain.observe(2, false), 57_599);
        assert_eq!(uncertain.observe(12, false), 57_609);
    }

    #[test]
    fn reconnect_requires_a_new_report_before_commands() {
        let mut controller = controller();
        assert!(controller.capabilities().start);
        assert!(
            controller
                .prepare_command(ApiCommand::Calibration(CalibrationCommand::VoltageLow(
                    1000
                )))
                .is_ok()
        );

        controller.begin_connection("reconnect");
        controller.connection_established();
        assert!(!controller.capabilities().start);
        assert!(
            controller
                .prepare_command(ApiCommand::Calibration(CalibrationCommand::VoltageLow(
                    1000
                )))
                .is_err()
        );
    }

    #[test]
    fn recovered_active_stays_uncertain_until_inactive() {
        let mut controller = controller();
        commit(&mut controller, ApiCommand::Start(config()));
        controller.report(report(ReportState::Active, 1));
        controller.invalidate_for_gap("serial gap");
        controller.begin_connection("reconnect");
        controller.connection_established();

        controller.report(report(ReportState::Active, 2));
        assert_eq!(controller.test.state, TestState::RecoveredUncertain);
        assert_eq!(controller.next_timer_sync(), None);
        assert!(
            controller
                .prepare_command(ApiCommand::Start(config()))
                .is_err()
        );

        controller.report(report(ReportState::Idle, 2));
        assert_eq!(controller.test.state, TestState::Stopped);
        assert!(controller.capabilities().start);
    }

    #[test]
    fn server_confirmation_requires_all_staged_calibration_values() {
        let mut controller = controller();
        for command in [
            CalibrationCommand::VoltageLow(1000),
            CalibrationCommand::VoltageHigh(4000),
        ] {
            commit(&mut controller, ApiCommand::Calibration(command));
        }
        assert!(
            controller
                .prepare_command(ApiCommand::Calibration(CalibrationCommand::Confirm))
                .is_err()
        );

        commit(&mut controller, ApiCommand::Start(config()));
        controller.report(report(ReportState::Active, 1));
        for command in [
            CalibrationCommand::CurrentLow(500),
            CalibrationCommand::CurrentHigh(2000),
        ] {
            commit(&mut controller, ApiCommand::Calibration(command));
        }
        assert!(
            controller
                .prepare_command(ApiCommand::Calibration(CalibrationCommand::Confirm))
                .is_ok()
        );
    }

    #[test]
    fn direct_resume_uses_the_supplied_current_settings() {
        let mut controller = TestController::new(ControllerMode::Direct);
        controller.begin_connection("connect");
        controller.connection_established();
        controller.report(report(ReportState::Idle, 0));
        commit(&mut controller, ApiCommand::Start(config()));
        controller.report(report(ReportState::Active, 1));
        commit(&mut controller, ApiCommand::Stop);
        controller.report(report(ReportState::Idle, 1));

        let updated = TestConfiguration::DischargeConstantPower {
            power_w: 25,
            cutoff_voltage_mv: 3200,
            cutoff_time_min: 30,
        };
        let prepared = controller
            .prepare_resume(updated)
            .expect("prepare resume with current form settings");
        let actual: [u8; device::OUTBOUND_FRAME_SIZE] =
            prepared.frame().expect("resume frame").into();
        let expected: [u8; device::OUTBOUND_FRAME_SIZE] = test_frame(&updated, true).into();
        assert_eq!(actual, expected);
    }
}
