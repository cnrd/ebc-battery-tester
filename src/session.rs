use std::collections::BTreeSet;

use crate::controller::{ControllerMode, DeviceReport, ReportState, TestController};
use crate::core::{
    ApiCommand, AuthoritativeSnapshot, Sample, ServerConnectionState, SnapshotUpdate,
    TestConfiguration, TestState,
};
use crate::device;
use crate::export::{LogDirection, LogEntry};
use crate::transport::{DeviceEvent, EventSender, RemoteConnectionStatus, TransportCommand};
use crate::usb;
use device::{ConnectionStatus, OutboundFrame};
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

/// Live device connection, transport status, telemetry, and outgoing command dispatch.
pub(crate) struct DeviceSession {
    pub(crate) available_devices: Vec<device::UsbDeviceInfo>,
    pub(crate) selected_device_index: Option<usize>,
    pub(crate) cmd_tx: UnboundedSender<TransportCommand>,
    event_rx: UnboundedReceiver<DeviceEvent>,
    pub(crate) event_tx: EventSender,
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
    controller: TestController,
    remote_run_id: Option<String>,
    last_remote_sequence: Option<u64>,
}

impl Default for DeviceSession {
    fn default() -> Self {
        let (event_tx, event_rx) = mpsc::unbounded::<DeviceEvent>();
        Self {
            available_devices: Vec::new(),
            selected_device_index: None,
            cmd_tx: mpsc::unbounded::<TransportCommand>().0,
            event_rx,
            event_tx: EventSender::new(event_tx, || {}),
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
            controller: TestController::new(ControllerMode::Direct),
            remote_run_id: None,
            last_remote_sequence: None,
        }
    }
}

impl DeviceSession {
    pub(crate) fn new(ctx: &egui::Context) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded::<TransportCommand>();
        let (event_tx, event_rx) = mpsc::unbounded::<DeviceEvent>();
        let event_tx = EventSender::new(event_tx, {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        });
        let transport_mode = if usb::is_remote_transport() {
            TransportMode::Remote
        } else {
            TransportMode::Direct
        };
        usb::spawn_device_worker(cmd_rx, event_tx.clone());
        if transport_mode == TransportMode::Direct {
            usb::enumerate_devices(event_tx.clone());
        }
        Self {
            cmd_tx,
            event_rx,
            event_tx,
            transport_mode,
            controller: TestController::new(ControllerMode::Direct),
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
        self.controller.capabilities().start
    }

    pub(crate) fn can_resume(&self) -> bool {
        self.controller.capabilities().resume
    }

    pub(crate) fn can_calibrate(&self) -> bool {
        self.can_start()
    }

    pub(crate) fn can_adjust(&self) -> bool {
        self.controller.capabilities().adjust
    }

    pub(crate) fn show_stop_control(&self) -> bool {
        self.controller.capabilities().show_stop
    }

    pub(crate) fn can_stop(&self) -> bool {
        self.controller.capabilities().stop
    }

    pub(crate) fn can_control_device(&self) -> bool {
        self.status == ConnectionStatus::Connected
            && (!self.is_remote() || self.remote_status == RemoteConnectionStatus::Connected)
    }

    pub(crate) fn displayed_elapsed_secs(&self) -> f64 {
        self.controller.elapsed().as_secs_f64()
    }

    /// Direct transports synchronize the device timer. The remote server owns
    /// this responsibility independently of browser clients.
    pub(crate) fn send_timer_sync_if_needed(&mut self, ctx: &egui::Context) {
        if self.transport_mode == TransportMode::Direct
            && let Some(elapsed_mins) = self.controller.next_timer_sync()
        {
            self.send_protocol(OutboundFrame::TimerSync(elapsed_mins), ctx);
        }
    }

    pub(crate) fn send_command(&mut self, command: ApiCommand, ctx: &egui::Context) {
        if self.is_remote() && self.remote_status != RemoteConnectionStatus::Connected {
            let error = format!("browser is disconnected; command was not sent: {command:?}");
            log::warn!("{error}");
            self.command_error = Some(error);
            return;
        }
        if self.is_remote() {
            self.log_command(format!("{command:?}"), Vec::new(), ctx);
            self.cmd_tx
                .unbounded_send(TransportCommand::Remote(command))
                .ok();
            return;
        }
        let prepared = match self.controller.prepare_command(command) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.command_error = Some(error);
                return;
            }
        };
        self.send_prepared(command, prepared, ctx);
    }

    pub(crate) fn resume(&mut self, config: TestConfiguration, ctx: &egui::Context) {
        if self.is_remote() {
            self.send_command(ApiCommand::Resume, ctx);
            return;
        }
        let prepared = match self.controller.prepare_resume(config) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.command_error = Some(error);
                return;
            }
        };
        self.send_prepared(ApiCommand::Resume, prepared, ctx);
    }

    fn send_prepared(
        &mut self,
        command: ApiCommand,
        prepared: crate::controller::PreparedCommand,
        ctx: &egui::Context,
    ) {
        if let Some(frame) = prepared.frame() {
            self.send_protocol(frame, ctx);
        }
        let starts_fresh = matches!(command, ApiCommand::Start(_));
        self.controller.commit_command(prepared, None);
        if starts_fresh {
            self.samples.clear();
        }
        self.sync_controller_view();
    }

    fn send_protocol(&mut self, frame: OutboundFrame, ctx: &egui::Context) {
        let raw_bytes = <[u8; device::OUTBOUND_FRAME_SIZE]>::from(frame).to_vec();
        self.log_command(format!("{frame:?}"), raw_bytes, ctx);
        self.cmd_tx
            .unbounded_send(TransportCommand::Protocol(frame))
            .ok();
    }

    fn log_command(&mut self, label: String, raw_bytes: Vec<u8>, ctx: &egui::Context) {
        self.log_entries.push(LogEntry {
            direction: LogDirection::Out,
            label,
            timestamp: ctx.input(|input| input.time),
            raw_bytes,
        });
    }

    pub(crate) fn connect(&mut self, index: usize, ctx: &egui::Context) {
        let command = if self.is_remote() {
            self.log_command(format!("{:?}", ApiCommand::Connect), Vec::new(), ctx);
            TransportCommand::Remote(ApiCommand::Connect)
        } else {
            self.log_command(
                format!("{:?}", OutboundFrame::Connect(index)),
                <[u8; device::OUTBOUND_FRAME_SIZE]>::from(OutboundFrame::Connect(index)).to_vec(),
                ctx,
            );
            TransportCommand::Connect(index)
        };
        self.cmd_tx.unbounded_send(command).ok();
    }

    pub(crate) fn disconnect_device(&mut self, ctx: &egui::Context) {
        if self.is_remote() {
            self.log_command(format!("{:?}", ApiCommand::Stop), Vec::new(), ctx);
            self.log_command(format!("{:?}", ApiCommand::Disconnect), Vec::new(), ctx);
            self.cmd_tx
                .unbounded_send(TransportCommand::Remote(ApiCommand::Stop))
                .ok();
            self.cmd_tx
                .unbounded_send(TransportCommand::Remote(ApiCommand::Disconnect))
                .ok();
        } else {
            for frame in [OutboundFrame::Stop, OutboundFrame::Disconnect] {
                self.log_command(
                    format!("{frame:?}"),
                    <[u8; device::OUTBOUND_FRAME_SIZE]>::from(frame).to_vec(),
                    ctx,
                );
            }
            self.cmd_tx
                .unbounded_send(TransportCommand::Protocol(OutboundFrame::Stop))
                .ok();
            self.cmd_tx
                .unbounded_send(TransportCommand::Disconnect)
                .ok();
        }
    }

    /// A remote browser never owns hardware lifetime, so closing or reloading
    /// it must not affect the backend test.
    pub(crate) fn shutdown(&self) {
        if self.is_remote() {
            return;
        }
        self.cmd_tx
            .unbounded_send(TransportCommand::Protocol(OutboundFrame::Stop))
            .ok();
        self.cmd_tx
            .unbounded_send(TransportCommand::Disconnect)
            .ok();
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
        let connected = update.connection == ServerConnectionState::Connected;
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
        self.controller
            .replace_authoritative(connected, update.device, update.test);
        self.sync_controller_view();
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
        self.controller.report(DeviceReport {
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
        });
        self.sync_controller_view();
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
        self.handle_direct_report(DeviceReport {
            mode,
            state: report_state.into(),
            voltage_mv,
            current_ma,
            capacity_mah,
            model,
            firmware_version: None,
        });
    }

    fn handle_direct_report(&mut self, report: DeviceReport) {
        let (_, measurement) = self.controller.report(report);
        self.sync_controller_view();
        if let Some(measurement) = measurement {
            let sequence = self
                .samples
                .last()
                .map_or(0, |sample| sample.sequence.saturating_add(1));
            self.samples.push(Sample {
                run_id: String::new(),
                sequence,
                timestamp_utc: String::new(),
                elapsed_seconds: measurement.elapsed_seconds,
                voltage_mv: measurement.voltage_mv,
                current_ma: measurement.current_ma,
                capacity_mah: measurement.capacity_mah,
                energy_wh: measurement.energy_wh,
                mode: measurement.mode,
            });
            if self.samples.len() > MAX_PRESENTATION_SAMPLES {
                compact_samples(&mut self.samples, COMPACTED_PRESENTATION_SAMPLES);
            }
        }
    }

    fn sync_controller_view(&mut self) {
        let device = self.controller.device();
        let test = self.controller.test();
        self.firmware_version.clone_from(&device.firmware_version);
        self.model_name.clone_from(&device.model);
        self.live_voltage_mv = device.voltage_mv.unwrap_or(0);
        self.live_current_ma = device.current_ma.unwrap_or(0);
        self.live_milli_ampere_hours = test
            .capacity_mah
            .or_else(|| device.capacity_mah.map(u64::from))
            .unwrap_or(0);
        self.live_energy_wh = test.energy_wh;
        self.current_device_mode = device.mode;
        self.activity_known = device.activity_known;
        self.mode_on = device.active;
        self.test_state.clone_from(&test.state);
    }

    pub(crate) fn consume_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                DeviceEvent::StatusChanged(status) => {
                    if !self.is_remote() {
                        match &status {
                            ConnectionStatus::Connecting => {
                                self.controller.begin_connection("connection changed");
                            }
                            ConnectionStatus::Connected => self.controller.connection_established(),
                            ConnectionStatus::Disconnected | ConnectionStatus::Error(_) => {
                                self.controller.disconnect("device connection lost");
                            }
                        }
                        self.sync_controller_view();
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
    fn firmware_identity_reports_do_not_add_presentation_samples() {
        let mut session = DeviceSession::default();
        session.handle_firmware_report(device::FirmwareReport {
            device_mode: device::DeviceMode::DischargeConstantCurrent,
            in_progress: true,
            current_ma: 1000,
            voltage_mv: 3900,
            milli_ampere_hours: 5,
            unknown: 0,
            firmware_version: "1.0".to_owned(),
            unknown1: 2988,
            unknown2: 2087,
            device_type: "EBC-A20".to_owned(),
        });

        assert!(session.samples.is_empty());
        assert_eq!(session.firmware_version.as_deref(), Some("1.0"));
        assert!(session.mode_on);
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
