use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use crate::core::{
    AuthoritativeSnapshot, Sample, ServerConnectionState, SnapshotUpdate, TestState,
};
use crate::device;
use crate::export::{LogDirection, LogEntry};
use crate::usb;
use device::{ConnectionStatus, DeviceEvent, OutboundFrame, RemoteConnectionStatus};
use futures::channel::mpsc;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransportMode {
    Direct,
    Remote,
}

const MAX_PRESENTATION_SAMPLES: usize = 5_000;
const COMPACTED_PRESENTATION_SAMPLES: usize = 4_000;

#[cfg(any(target_arch = "wasm32", test))]
pub(crate) fn select_wasm_transport(default: &str, switches: &str) -> TransportMode {
    let mut mode = if default.eq_ignore_ascii_case("remote") {
        TransportMode::Remote
    } else {
        TransportMode::Direct
    };
    for part in switches
        .trim_start_matches(['?', '#'])
        .split(['&', '?', '#'])
    {
        if part.eq_ignore_ascii_case("transport=remote") {
            mode = TransportMode::Remote;
        } else if part.eq_ignore_ascii_case("transport=webusb") {
            mode = TransportMode::Direct;
        }
    }
    mode
}

fn compact_samples(samples: &mut Vec<Sample>, limit: usize) {
    if samples.len() <= limit {
        return;
    }
    if limit < 2 {
        let last = samples.pop();
        samples.clear();
        samples.extend(last);
        return;
    }

    let bucket_count = (limit - 2) / 4;
    if bucket_count == 0 {
        let Some(last) = samples.pop() else {
            return;
        };
        samples.truncate(1);
        samples.push(last);
        return;
    }
    let interior_len = samples.len() - 2;
    let mut selected = BTreeSet::from([0, samples.len() - 1]);
    for bucket in 0..bucket_count {
        let start = 1 + interior_len * bucket / bucket_count;
        let end = 1 + interior_len * (bucket + 1) / bucket_count;
        if start >= end {
            continue;
        }
        let indices = start..end;
        selected.insert(
            indices
                .clone()
                .min_by_key(|index| samples[*index].voltage_mv)
                .unwrap_or(start),
        );
        selected.insert(
            indices
                .clone()
                .max_by_key(|index| samples[*index].voltage_mv)
                .unwrap_or(start),
        );
        selected.insert(
            indices
                .clone()
                .min_by_key(|index| samples[*index].current_ma)
                .unwrap_or(start),
        );
        selected.insert(
            indices
                .max_by_key(|index| samples[*index].current_ma)
                .unwrap_or(start),
        );
    }
    *samples = selected
        .into_iter()
        .map(|index| samples[index].clone())
        .collect();
}

fn timer_sync_eligible(
    transport_mode: TransportMode,
    test_state: &TestState,
    activity_known: bool,
    active: bool,
) -> bool {
    transport_mode == TransportMode::Direct
        && elapsed_projection_eligible(test_state, activity_known, active)
}

fn elapsed_projection_eligible(test_state: &TestState, activity_known: bool, active: bool) -> bool {
    *test_state == TestState::Running && activity_known && active
}

/// Live device connection, transport status, telemetry, and outgoing command dispatch.
pub(crate) struct DeviceSession {
    pub(crate) available_devices: Vec<device::UsbDeviceInfo>,
    pub(crate) selected_device_index: Option<usize>,
    pub(crate) cmd_tx: UnboundedSender<OutboundFrame>,
    event_rx: UnboundedReceiver<DeviceEvent>,
    pub(crate) event_tx: UnboundedSender<DeviceEvent>,
    pub(crate) status: ConnectionStatus,
    pub(crate) remote_status: RemoteConnectionStatus,
    pub(crate) firmware_version: Option<String>,
    pub(crate) model_name: Option<String>,
    pub(crate) live_voltage_mv: u16,
    pub(crate) live_current_ma: u16,
    pub(crate) live_milli_ampere_hours: u64,
    pub(crate) live_energy_wh: f64,
    pub(crate) samples: Vec<Sample>,
    pub(crate) current_device_mode: Option<device::DeviceMode>,
    pub(crate) activity_known: bool,
    pub(crate) mode_on: bool,
    pub(crate) test_state: TestState,
    pub(crate) log_entries: Vec<LogEntry>,
    pub(crate) command_error: Option<String>,
    transport_mode: TransportMode,
    remote_run_id: Option<String>,
    last_remote_sequence: Option<u64>,
    last_timer_sync_min: u64,
    mode_started_at: Option<Instant>,
    mode_accumulated: Duration,
}

impl Default for DeviceSession {
    fn default() -> Self {
        Self {
            available_devices: Vec::new(),
            selected_device_index: None,
            cmd_tx: mpsc::unbounded::<OutboundFrame>().0,
            event_rx: mpsc::unbounded::<DeviceEvent>().1,
            event_tx: mpsc::unbounded::<DeviceEvent>().0,
            status: ConnectionStatus::Disconnected,
            remote_status: RemoteConnectionStatus::NotUsed,
            firmware_version: None,
            model_name: None,
            live_voltage_mv: 0,
            live_current_ma: 0,
            live_milli_ampere_hours: 0,
            live_energy_wh: 0.0,
            samples: Vec::new(),
            current_device_mode: None,
            activity_known: false,
            mode_on: false,
            test_state: TestState::Idle,
            log_entries: Vec::new(),
            command_error: None,
            transport_mode: TransportMode::Direct,
            remote_run_id: None,
            last_remote_sequence: None,
            last_timer_sync_min: 0,
            mode_started_at: None,
            mode_accumulated: Duration::ZERO,
        }
    }
}

impl DeviceSession {
    pub(crate) fn new(ctx: &egui::Context) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded::<OutboundFrame>();
        let (event_tx, event_rx) = mpsc::unbounded::<DeviceEvent>();
        let transport_mode = if usb::is_remote_transport() {
            TransportMode::Remote
        } else {
            TransportMode::Direct
        };
        usb::spawn_device_worker(ctx.clone(), cmd_rx, event_tx.clone());
        if transport_mode == TransportMode::Direct {
            usb::enumerate_devices(event_tx.clone());
        }
        Self {
            cmd_tx,
            event_rx,
            event_tx,
            transport_mode,
            remote_status: if transport_mode == TransportMode::Remote {
                RemoteConnectionStatus::Connecting
            } else {
                RemoteConnectionStatus::NotUsed
            },
            ..Default::default()
        }
    }

    pub(crate) fn is_remote(&self) -> bool {
        self.transport_mode == TransportMode::Remote
    }

    pub(crate) fn has_live_voltage(&self) -> bool {
        self.activity_known && self.live_voltage_mv > 0
    }

    pub(crate) fn can_start(&self) -> bool {
        self.has_live_voltage()
            && !self.mode_on
            && matches!(
                self.test_state,
                TestState::Idle | TestState::Stopped | TestState::Completed
            )
    }

    pub(crate) fn can_resume(&self) -> bool {
        self.has_live_voltage() && !self.mode_on && self.test_state == TestState::Stopped
    }

    pub(crate) fn can_calibrate(&self) -> bool {
        self.can_start()
    }

    pub(crate) fn show_stop_control(&self) -> bool {
        self.mode_on
            || matches!(
                self.test_state,
                TestState::Starting
                    | TestState::Running
                    | TestState::Stopping
                    | TestState::RecoveredUncertain
            )
    }

    pub(crate) fn can_stop(&self) -> bool {
        self.show_stop_control() && self.test_state != TestState::Stopping
    }

    pub(crate) fn can_control_device(&self) -> bool {
        self.status == ConnectionStatus::Connected
            && (!self.is_remote() || self.remote_status == RemoteConnectionStatus::Connected)
    }

    pub(crate) fn displayed_elapsed_secs(&self) -> f64 {
        self.elapsed().as_secs_f64()
    }

    fn elapsed(&self) -> Duration {
        self.mode_started_at
            .map_or(self.mode_accumulated, |started| {
                self.mode_accumulated + started.elapsed()
            })
    }

    /// The command is optimistic until an active device report confirms it.
    pub(crate) fn start_mode(&mut self) {
        if self.is_remote() {
            return;
        }
        self.mode_on = false;
        self.activity_known = false;
        self.test_state = TestState::Starting;
        self.mode_started_at = None;
        self.mode_accumulated = Duration::ZERO;
        self.last_timer_sync_min = 0;
        self.samples.clear();
    }

    pub(crate) fn continue_mode(&mut self) {
        if self.is_remote() {
            return;
        }
        self.mode_on = false;
        self.activity_known = false;
        self.test_state = TestState::Starting;
        self.mode_started_at = None;
    }

    pub(crate) fn stop_mode(&mut self) {
        if self.is_remote() {
            return;
        }
        self.freeze_timer();
        self.test_state = TestState::Stopping;
    }

    fn freeze_timer(&mut self) {
        if let Some(started) = self.mode_started_at.take() {
            self.mode_accumulated += started.elapsed();
        }
    }

    fn invalidate_direct_for_gap(&mut self) {
        self.freeze_timer();
        if self.mode_on
            || matches!(
                self.test_state,
                TestState::Starting
                    | TestState::Running
                    | TestState::Stopping
                    | TestState::RecoveredUncertain
            )
        {
            self.test_state = TestState::RecoveredUncertain;
        }
        self.mode_on = false;
        self.activity_known = false;
        self.current_device_mode = None;
    }

    /// Direct transports synchronize the device timer. The remote server owns
    /// this responsibility independently of browser clients.
    pub(crate) fn send_timer_sync_if_needed(&mut self, ctx: &egui::Context) {
        if let Some(elapsed_mins) = self.pending_timer_sync_minute() {
            self.last_timer_sync_min = elapsed_mins;
            self.send_cmd(OutboundFrame::TimerSync(elapsed_mins as u16), ctx);
        }
    }

    fn pending_timer_sync_minute(&self) -> Option<u64> {
        if !timer_sync_eligible(
            self.transport_mode,
            &self.test_state,
            self.activity_known,
            self.mode_on,
        ) {
            return None;
        }
        let elapsed_mins = self.elapsed().as_secs() / 60;
        (elapsed_mins > self.last_timer_sync_min
            && elapsed_mins <= u64::from(device::MAX_TIMER_SYNC_MINUTES))
        .then_some(elapsed_mins)
    }

    pub(crate) fn send_cmd(&mut self, frame: OutboundFrame, ctx: &egui::Context) {
        if self.is_remote() && self.remote_status != RemoteConnectionStatus::Connected {
            let error = format!("browser is disconnected; command was not sent: {frame:?}");
            log::warn!("{error}");
            self.command_error = Some(error);
            return;
        }
        let raw_bytes = if self.is_remote() {
            Vec::new()
        } else {
            <[u8; device::OUTBOUND_FRAME_SIZE]>::from(frame).to_vec()
        };
        self.log_entries.push(LogEntry {
            direction: LogDirection::Out,
            label: format!("{frame:?}"),
            timestamp: ctx.input(|input| input.time),
            raw_bytes,
        });
        self.cmd_tx.unbounded_send(frame).ok();
    }

    /// A remote browser never owns hardware lifetime, so closing or reloading
    /// it must not affect the backend test.
    pub(crate) fn shutdown(&self) {
        if self.is_remote() {
            return;
        }
        self.cmd_tx.unbounded_send(OutboundFrame::Stop).ok();
        self.cmd_tx.unbounded_send(OutboundFrame::Disconnect).ok();
    }

    fn update_timer(&mut self, elapsed_seconds: u64, running: bool) {
        self.mode_accumulated = Duration::from_secs(elapsed_seconds);
        self.mode_started_at = running.then(Instant::now);
        self.last_timer_sync_min = elapsed_seconds / 60;
    }

    fn apply_snapshot(&mut self, snapshot: AuthoritativeSnapshot) {
        self.apply_update(SnapshotUpdate::from(&snapshot));
        let mut history = snapshot.history;
        self.remote_run_id = history.last().map(|sample| sample.run_id.clone());
        if let Some(run_id) = &self.remote_run_id {
            history.retain(|sample| sample.run_id == *run_id);
        }
        compact_samples(&mut history, MAX_PRESENTATION_SAMPLES);
        self.last_remote_sequence = history.last().map(|sample| sample.sequence);
        self.samples = history;
        self.command_error = None;
    }

    fn apply_update(&mut self, update: SnapshotUpdate) {
        self.status = match update.connection {
            ServerConnectionState::Disconnected => ConnectionStatus::Disconnected,
            ServerConnectionState::Connecting => ConnectionStatus::Connecting,
            ServerConnectionState::Connected => ConnectionStatus::Connected,
            ServerConnectionState::Error => ConnectionStatus::Error(
                update
                    .connection_error
                    .unwrap_or_else(|| "backend device error".to_owned()),
            ),
        };
        self.firmware_version = update.device.firmware_version;
        self.model_name = update.device.model;
        self.live_voltage_mv = update.device.voltage_mv.unwrap_or(0);
        self.live_current_ma = update.device.current_ma.unwrap_or(0);
        self.live_milli_ampere_hours = update
            .test
            .capacity_mah
            .or_else(|| update.device.capacity_mah.map(u64::from))
            .unwrap_or(0);
        self.live_energy_wh = update.test.energy_wh;
        self.current_device_mode = update.device.mode;
        self.activity_known = update.device.activity_known;
        self.mode_on = update.device.active;
        self.test_state = update.test.state;
        let timer_running =
            elapsed_projection_eligible(&self.test_state, self.activity_known, self.mode_on);
        self.update_timer(update.test.elapsed_seconds, timer_running);
    }

    fn apply_sample(&mut self, sample: Sample) {
        if self.remote_run_id.as_deref() != Some(sample.run_id.as_str()) {
            self.samples.clear();
            self.remote_run_id = Some(sample.run_id.clone());
            self.last_remote_sequence = None;
        }
        if self
            .last_remote_sequence
            .is_some_and(|sequence| sample.sequence <= sequence)
        {
            return;
        }
        self.live_voltage_mv = sample.voltage_mv;
        self.live_current_ma = sample.current_ma;
        self.live_milli_ampere_hours = sample.capacity_mah;
        self.live_energy_wh = sample.energy_wh;
        self.current_device_mode = Some(sample.mode);
        self.last_remote_sequence = Some(sample.sequence);
        self.samples.push(sample);
        if self.samples.len() > MAX_PRESENTATION_SAMPLES {
            compact_samples(&mut self.samples, COMPACTED_PRESENTATION_SAMPLES);
        }
    }

    fn handle_firmware_report(&mut self, report: device::FirmwareReport) {
        self.current_device_mode = Some(report.device_mode);
        self.apply_direct_report_state(if report.in_progress {
            Some(device::ModeReportState::Active)
        } else {
            None
        });
        self.live_voltage_mv = report.voltage_mv;
        self.live_current_ma = report.current_ma;
        self.live_milli_ampere_hours = u64::from(report.milli_ampere_hours);
        self.live_energy_wh =
            report.voltage_mv as f64 * report.milli_ampere_hours as f64 / 1_000_000.0;
        self.firmware_version = Some(report.firmware_version);
        self.model_name = Some(report.device_type);
    }

    fn handle_measurement(
        &mut self,
        mode: device::DeviceMode,
        report_state: device::ModeReportState,
        voltage_mv: u16,
        current_ma: u16,
        capacity_mah: u16,
        model: String,
    ) {
        self.current_device_mode = Some(mode);
        self.apply_direct_report_state(Some(report_state));
        self.live_voltage_mv = voltage_mv;
        self.live_current_ma = current_ma;
        self.live_milli_ampere_hours = u64::from(capacity_mah);
        self.live_energy_wh = voltage_mv as f64 * capacity_mah as f64 / 1_000_000.0;
        self.model_name = Some(model);
        if self.mode_on {
            let sequence = self
                .samples
                .last()
                .map_or(0, |sample| sample.sequence.saturating_add(1));
            self.samples.push(Sample {
                run_id: String::new(),
                sequence,
                timestamp_utc: String::new(),
                elapsed_seconds: self.elapsed().as_secs(),
                voltage_mv,
                current_ma,
                capacity_mah: u64::from(capacity_mah),
                energy_wh: self.live_energy_wh,
                mode,
            });
            if self.samples.len() > MAX_PRESENTATION_SAMPLES {
                compact_samples(&mut self.samples, COMPACTED_PRESENTATION_SAMPLES);
            }
        }
    }

    fn apply_direct_report_state(&mut self, report_state: Option<device::ModeReportState>) {
        let active = report_state == Some(device::ModeReportState::Active);
        self.activity_known = true;
        self.mode_on = active;
        if active {
            if self.test_state == TestState::Starting {
                self.test_state = TestState::Running;
            }
            if self.test_state == TestState::Running && self.mode_started_at.is_none() {
                self.mode_started_at = Some(Instant::now());
            }
        } else {
            self.freeze_timer();
            self.test_state = match (self.test_state.clone(), report_state) {
                (TestState::Starting, _) => TestState::Starting,
                (TestState::Stopping | TestState::RecoveredUncertain, _)
                | (TestState::Running, Some(device::ModeReportState::Idle)) => TestState::Stopped,
                (TestState::Running, Some(device::ModeReportState::Finished)) => {
                    TestState::Completed
                }
                (state, _) => state,
            };
            if matches!(self.test_state, TestState::Stopped | TestState::Completed) {
                self.mode_on = false;
            }
        }
    }

    pub(crate) fn consume_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                DeviceEvent::StatusChanged(status) => {
                    if matches!(
                        status,
                        ConnectionStatus::Disconnected | ConnectionStatus::Error(_)
                    ) && !self.is_remote()
                    {
                        self.invalidate_direct_for_gap();
                    }
                    log::info!("Device status changed: {status:?}");
                    self.status = status;
                }
                DeviceEvent::DevicesUpdated(devices) => {
                    self.available_devices = devices;
                    if self.available_devices.len() == 1 {
                        self.selected_device_index = Some(0);
                    } else if let Some(selected_index) = self.selected_device_index
                        && selected_index >= self.available_devices.len()
                    {
                        self.selected_device_index = None;
                    }
                }
                DeviceEvent::Frame(frame, raw_bytes) => {
                    self.log_entries.push(LogEntry {
                        direction: LogDirection::In,
                        label: format!("{frame:?}"),
                        timestamp: ctx.input(|input| input.time),
                        raw_bytes,
                    });
                    match frame {
                        device::InboundFrame::Firmware(report) => {
                            self.handle_firmware_report(report);
                        }
                        device::InboundFrame::Charge(report) => self.handle_measurement(
                            device::DeviceMode::ChargeConstantVoltage,
                            report.state,
                            report.voltage_mv,
                            report.current_ma,
                            report.milli_ampere_hours,
                            report.device_type,
                        ),
                        device::InboundFrame::DischargeConstantCurrent(report) => self
                            .handle_measurement(
                                device::DeviceMode::DischargeConstantCurrent,
                                report.state,
                                report.voltage_mv,
                                report.current_ma,
                                report.milli_ampere_hours,
                                report.device_type,
                            ),
                        device::InboundFrame::DischargeConstantPower(report) => self
                            .handle_measurement(
                                device::DeviceMode::DischargeConstantPower,
                                report.state,
                                report.voltage_mv,
                                report.current_ma,
                                report.milli_ampere_hours,
                                report.device_type,
                            ),
                    }
                }
                DeviceEvent::RemoteConnectionChanged(status) => self.remote_status = status,
                DeviceEvent::Remote(event) => match event {
                    crate::core::WebSocketEvent::Snapshot(snapshot) => {
                        self.apply_snapshot(snapshot);
                    }
                    crate::core::WebSocketEvent::Update(update) => self.apply_update(update),
                    crate::core::WebSocketEvent::Sample(sample) => self.apply_sample(sample),
                },
                DeviceEvent::RemoteCommandSucceeded => self.command_error = None,
                DeviceEvent::RemoteCommandError(error) => self.command_error = Some(error),
            }
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "test setup and assertions should fail fast"
)]
mod tests {
    use super::*;
    use crate::core::{DeviceState, TestStatus};

    #[test]
    fn authoritative_snapshot_replaces_history_after_reconnect() {
        let mut session = DeviceSession {
            transport_mode: TransportMode::Remote,
            ..DeviceSession::default()
        };
        session.samples.push(sample("old-run", 0, 1));
        let snapshot = AuthoritativeSnapshot {
            connection: ServerConnectionState::Connected,
            connection_error: None,
            device: DeviceState {
                activity_known: true,
                active: true,
                voltage_mv: Some(3900),
                current_ma: Some(1000),
                capacity_mah: Some(5),
                mode: Some(device::DeviceMode::DischargeConstantCurrent),
                ..DeviceState::default()
            },
            test: TestStatus {
                state: TestState::Running,
                elapsed_seconds: 12,
                capacity_mah: Some(100_005),
                ..TestStatus::default()
            },
            history: vec![sample("new-run", 0, 12)],
        };

        session.apply_snapshot(snapshot);

        assert_eq!(session.samples, vec![sample("new-run", 0, 12)]);
        assert_eq!(session.live_voltage_mv, 3900);
        assert_eq!(session.live_milli_ampere_hours, 100_005);
        assert!(session.mode_on);
    }

    #[test]
    fn lightweight_updates_preserve_history_and_samples_are_deduplicated() {
        let mut session = DeviceSession {
            transport_mode: TransportMode::Remote,
            ..DeviceSession::default()
        };
        let existing = sample("run", 1, 1);
        session.apply_snapshot(AuthoritativeSnapshot {
            history: vec![existing.clone()],
            ..AuthoritativeSnapshot::default()
        });
        session.apply_update(SnapshotUpdate {
            connection: ServerConnectionState::Connected,
            device: DeviceState {
                voltage_mv: Some(3800),
                ..DeviceState::default()
            },
            ..SnapshotUpdate::default()
        });
        session.apply_sample(existing);

        assert_eq!(session.samples.len(), 1);
        assert_eq!(session.live_voltage_mv, 3800);
    }

    #[test]
    fn sample_watermark_ignores_old_events_without_scanning_history() {
        let mut session = DeviceSession {
            transport_mode: TransportMode::Remote,
            ..DeviceSession::default()
        };
        let history: Vec<Sample> = (0..50_000)
            .map(|index| sample("run", index, index))
            .collect();
        session.apply_snapshot(AuthoritativeSnapshot {
            history,
            ..AuthoritativeSnapshot::default()
        });
        let original_len = session.samples.len();
        let last = session
            .samples
            .last()
            .expect("history has a last sample")
            .clone();

        session.apply_sample(sample("run", 1, 1));
        session.apply_sample(last.clone());
        assert_eq!(session.samples.len(), original_len);

        session.apply_sample(sample("run", 50_000, 50_000));
        assert_eq!(session.samples.len(), original_len + 1);
    }

    #[test]
    fn new_run_sample_clears_stale_history() {
        let mut session = DeviceSession {
            transport_mode: TransportMode::Remote,
            ..DeviceSession::default()
        };
        session.apply_sample(sample("old", 4, 4));
        session.apply_update(SnapshotUpdate::default());
        assert_eq!(session.samples.len(), 1);

        session.apply_sample(sample("new", 0, 0));
        assert_eq!(session.samples, vec![sample("new", 0, 0)]);
    }

    #[test]
    fn elapsed_projection_requires_known_active_running_state() {
        assert!(elapsed_projection_eligible(&TestState::Running, true, true));
        assert!(!elapsed_projection_eligible(
            &TestState::RecoveredUncertain,
            true,
            true
        ));
        assert!(!elapsed_projection_eligible(
            &TestState::Running,
            false,
            true
        ));
        assert!(!elapsed_projection_eligible(
            &TestState::Running,
            true,
            false
        ));
    }

    #[test]
    fn direct_timer_sync_waits_for_confirmed_active_report() {
        let mut session = DeviceSession::default();
        session.start_mode();
        session.mode_accumulated = Duration::from_secs(60);

        assert!(!session.activity_known);
        assert_eq!(session.pending_timer_sync_minute(), None);

        session.handle_measurement(
            device::DeviceMode::DischargeConstantCurrent,
            device::ModeReportState::Active,
            3900,
            1000,
            5,
            "EBC-A20".to_owned(),
        );

        assert!(session.activity_known);
        assert_eq!(session.pending_timer_sync_minute(), Some(1));

        session.handle_measurement(
            device::DeviceMode::DischargeConstantCurrent,
            device::ModeReportState::Finished,
            3900,
            0,
            5,
            "EBC-A20".to_owned(),
        );
        assert_eq!(session.pending_timer_sync_minute(), None);
        assert!(session.mode_started_at.is_none());
    }

    #[test]
    fn direct_timer_sync_stops_at_canonical_base240_maximum() {
        let mut session = DeviceSession {
            activity_known: true,
            mode_on: true,
            test_state: TestState::Running,
            mode_started_at: None,
            mode_accumulated: Duration::from_secs(u64::from(device::MAX_TIMER_SYNC_MINUTES) * 60),
            ..DeviceSession::default()
        };
        assert_eq!(
            session.pending_timer_sync_minute(),
            Some(u64::from(device::MAX_TIMER_SYNC_MINUTES))
        );

        session.mode_accumulated += Duration::from_secs(60);
        assert_eq!(session.pending_timer_sync_minute(), None);
    }

    #[test]
    fn direct_start_ignores_buffered_idle_until_active_report() {
        let mut session = DeviceSession::default();
        session.start_mode();

        session.apply_direct_report_state(Some(device::ModeReportState::Idle));
        session.apply_direct_report_state(None);
        assert_eq!(session.test_state, TestState::Starting);
        assert!(session.mode_started_at.is_none());

        session.apply_direct_report_state(Some(device::ModeReportState::Active));
        assert_eq!(session.test_state, TestState::Running);
        assert!(session.mode_started_at.is_some());
    }

    #[test]
    fn direct_mode_reports_distinguish_idle_finished_and_ambiguous_inactive() {
        for (report_state, expected) in [
            (device::ModeReportState::Idle, TestState::Stopped),
            (device::ModeReportState::Finished, TestState::Completed),
        ] {
            let mut session = DeviceSession {
                activity_known: true,
                mode_on: true,
                test_state: TestState::Running,
                mode_started_at: Some(Instant::now()),
                ..DeviceSession::default()
            };
            session.apply_direct_report_state(None);
            assert_eq!(session.test_state, TestState::Running);
            session.apply_direct_report_state(Some(report_state));
            assert_eq!(session.test_state, expected);
        }
    }

    #[test]
    fn controls_require_fresh_authoritative_activity() {
        let mut session = DeviceSession {
            live_voltage_mv: 3900,
            ..DeviceSession::default()
        };
        assert!(!session.can_start());
        assert!(!session.can_calibrate());

        session.activity_known = true;
        assert!(session.can_start());
        assert!(session.can_calibrate());

        session.start_mode();
        assert_eq!(session.test_state, TestState::Starting);
        assert!(session.show_stop_control());
        assert!(session.can_stop());
        assert!(!session.can_start());
        assert!(!session.can_calibrate());

        session.test_state = TestState::Stopping;
        assert!(!session.can_stop());
    }

    #[test]
    fn direct_connection_gap_never_reclaims_running_state() {
        let mut session = DeviceSession {
            activity_known: true,
            mode_on: true,
            test_state: TestState::Running,
            mode_started_at: Some(Instant::now()),
            ..DeviceSession::default()
        };

        session.invalidate_direct_for_gap();
        assert_eq!(session.test_state, TestState::RecoveredUncertain);
        assert!(!session.activity_known);
        assert!(!session.mode_on);
        assert!(session.mode_started_at.is_none());

        session.apply_direct_report_state(Some(device::ModeReportState::Active));
        assert_eq!(session.test_state, TestState::RecoveredUncertain);
        assert!(session.mode_started_at.is_none());

        session.apply_direct_report_state(Some(device::ModeReportState::Idle));
        assert_eq!(session.test_state, TestState::Stopped);
    }

    #[test]
    fn incremental_history_stays_bounded_and_preserves_extrema() {
        let mut session = DeviceSession {
            transport_mode: TransportMode::Remote,
            ..DeviceSession::default()
        };
        for sequence in 0..20_000 {
            let mut value = sample("run", sequence, sequence);
            if sequence == 123 {
                value.voltage_mv = 1;
            }
            if sequence == 456 {
                value.voltage_mv = u16::MAX;
            }
            if sequence == 789 {
                value.current_ma = 0;
            }
            if sequence == 1_234 {
                value.current_ma = u16::MAX;
            }
            session.apply_sample(value);
            assert!(session.samples.len() <= MAX_PRESENTATION_SAMPLES);
        }

        assert_eq!(
            session.samples.first().map(|sample| sample.sequence),
            Some(0)
        );
        assert_eq!(
            session.samples.last().map(|sample| sample.sequence),
            Some(19_999)
        );
        assert!(session.samples.iter().any(|sample| sample.voltage_mv == 1));
        assert!(
            session
                .samples
                .iter()
                .any(|sample| sample.voltage_mv == u16::MAX)
        );
        assert!(session.samples.iter().any(|sample| sample.current_ma == 0));
        assert!(
            session
                .samples
                .iter()
                .any(|sample| sample.current_ma == u16::MAX)
        );
    }

    #[test]
    fn transport_default_and_explicit_overrides_are_pure() {
        assert_eq!(select_wasm_transport("webusb", ""), TransportMode::Direct);
        assert_eq!(select_wasm_transport("remote", ""), TransportMode::Remote);
        assert_eq!(
            select_wasm_transport("webusb", "?transport=remote"),
            TransportMode::Remote
        );
        assert_eq!(
            select_wasm_transport("remote", "?transport=webusb"),
            TransportMode::Direct
        );
        assert_eq!(
            select_wasm_transport("remote", "?transport=remote#transport=webusb"),
            TransportMode::Direct
        );
    }

    fn sample(run_id: &str, sequence: u64, elapsed_seconds: u64) -> Sample {
        Sample {
            run_id: run_id.to_owned(),
            sequence,
            timestamp_utc: format!("2026-01-01T00:00:{sequence:05}.000Z"),
            elapsed_seconds,
            voltage_mv: 3900,
            current_ma: 1000,
            capacity_mah: 5,
            energy_wh: 0.005,
            mode: device::DeviceMode::DischargeConstantCurrent,
        }
    }
}
