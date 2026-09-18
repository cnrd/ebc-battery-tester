use std::collections::BTreeSet;

use crate::backend::{
    BackendCommand, BackendConnectionStatus, BackendEvent, BackendState, DiagnosticDirection,
};
use crate::backend_client::BackendClient;
#[cfg(not(target_arch = "wasm32"))]
use crate::backend_client::BackendTarget;
use crate::core::{
    ApiCommand, AuthoritativeSnapshot, Capabilities, Sample, ServerConnectionState,
    TestConfiguration, TestState,
};
use crate::device::{self, ConnectionStatus};
use crate::export::{LogDirection, LogEntry};

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

/// Presentation state reduced from semantic backend events.
pub(crate) struct DeviceSession {
    pub(crate) available_devices: Vec<device::UsbDeviceInfo>,
    pub(crate) selected_device_index: Option<usize>,
    backend: BackendClient,
    pub(crate) status: ConnectionStatus,
    pub(crate) remote_status: BackendConnectionStatus,
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
    capabilities: Capabilities,
    elapsed_seconds: u64,
    remote_run_id: Option<String>,
    last_remote_sequence: Option<u64>,
}

impl Default for DeviceSession {
    fn default() -> Self {
        Self {
            available_devices: Vec::new(),
            selected_device_index: None,
            backend: BackendClient::default(),
            status: ConnectionStatus::Disconnected,
            remote_status: BackendConnectionStatus::NotUsed,
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
            capabilities: Capabilities::default(),
            elapsed_seconds: 0,
            remote_run_id: None,
            last_remote_sequence: None,
        }
    }
}

impl DeviceSession {
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn new(ctx: &egui::Context) -> Self {
        let backend = BackendClient::new(ctx);
        Self::with_backend(backend)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn new(
        ctx: &egui::Context,
        target: BackendTarget,
        remote_url: &str,
    ) -> Result<Self, String> {
        BackendClient::new(ctx, target, remote_url).map(Self::with_backend)
    }

    fn with_backend(backend: BackendClient) -> Self {
        let transport_mode = if backend.is_remote() {
            TransportMode::Remote
        } else {
            TransportMode::Direct
        };
        if transport_mode == TransportMode::Direct {
            backend.command(BackendCommand::RefreshDevices);
        }
        Self {
            backend,
            transport_mode,
            remote_status: if transport_mode == TransportMode::Remote {
                BackendConnectionStatus::Connecting
            } else {
                BackendConnectionStatus::NotUsed
            },
            ..Self::default()
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn switch_backend(
        &mut self,
        ctx: &egui::Context,
        target: BackendTarget,
        remote_url: &str,
    ) -> Result<(), String> {
        let replacement = Self::new(ctx, target, remote_url)?;
        *self = replacement;
        Ok(())
    }

    pub(crate) fn is_remote(&self) -> bool {
        self.transport_mode == TransportMode::Remote
    }

    pub(crate) fn has_live_voltage(&self) -> bool {
        self.activity_known && self.live_voltage_mv > 0
    }

    pub(crate) fn can_start(&self) -> bool {
        self.capabilities.start
    }

    pub(crate) fn can_resume(&self) -> bool {
        self.capabilities.resume
    }

    pub(crate) fn can_calibrate(&self) -> bool {
        self.capabilities.calibrate_voltage
    }

    pub(crate) fn can_calibrate_voltage(&self) -> bool {
        self.capabilities.calibrate_voltage
    }

    pub(crate) fn can_calibrate_current(&self) -> bool {
        self.capabilities.calibrate_current
    }

    pub(crate) fn can_confirm_calibration(&self) -> bool {
        self.capabilities.confirm_calibration
    }

    pub(crate) fn can_adjust(&self) -> bool {
        self.capabilities.adjust
    }

    pub(crate) fn show_stop_control(&self) -> bool {
        self.capabilities.show_stop
    }

    pub(crate) fn can_stop(&self) -> bool {
        self.capabilities.stop
    }

    pub(crate) fn can_control_device(&self) -> bool {
        self.status == ConnectionStatus::Connected
            && (!self.is_remote() || self.remote_status == BackendConnectionStatus::Connected)
    }

    pub(crate) fn displayed_elapsed_secs(&self) -> f64 {
        self.elapsed_seconds as f64
    }

    pub(crate) fn send_command(&mut self, command: ApiCommand) {
        if self.is_remote() && self.remote_status != BackendConnectionStatus::Connected {
            let error = format!("browser is disconnected; command was not sent: {command:?}");
            log::warn!("{error}");
            self.command_error = Some(error);
            return;
        }
        self.backend.command(BackendCommand::Api(command));
    }

    pub(crate) fn resume(&self, config: TestConfiguration) {
        if self.is_remote() && self.remote_status != BackendConnectionStatus::Connected {
            return;
        }
        self.backend.command(BackendCommand::Resume(config));
    }

    pub(crate) fn refresh_devices(&self) {
        self.backend.command(BackendCommand::RefreshDevices);
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn request_device_access(&self) {
        self.backend.request_device_access();
    }

    pub(crate) fn connect(&self, index: usize) {
        self.backend.command(BackendCommand::Connect(index));
    }

    pub(crate) fn disconnect_device(&self) {
        self.backend.command(BackendCommand::Disconnect);
    }

    pub(crate) fn shutdown(&self) {
        self.backend.shutdown();
    }

    fn apply_snapshot(&mut self, snapshot: AuthoritativeSnapshot) {
        self.apply_state(BackendState {
            update: crate::core::SnapshotUpdate::from(&snapshot),
        });
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

    fn apply_state(&mut self, state: BackendState) {
        let update = state.update;
        self.capabilities = update.capabilities;
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
        self.elapsed_seconds = update.test.elapsed_seconds;
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
        self.elapsed_seconds = sample.elapsed_seconds;
        self.last_remote_sequence = Some(sample.sequence);
        self.samples.push(sample);
        if self.samples.len() > MAX_PRESENTATION_SAMPLES {
            compact_samples(&mut self.samples, COMPACTED_PRESENTATION_SAMPLES);
        }
    }

    pub(crate) fn consume_events(&mut self, ctx: &egui::Context) {
        while let Some(event) = self.backend.try_event() {
            match event {
                BackendEvent::DevicesUpdated(devices) => {
                    self.available_devices = devices;
                    if self.available_devices.len() == 1 {
                        self.selected_device_index = Some(0);
                    } else if self
                        .selected_device_index
                        .is_some_and(|index| index >= self.available_devices.len())
                    {
                        self.selected_device_index = None;
                    }
                }
                BackendEvent::BackendConnectionChanged(status) => self.remote_status = status,
                BackendEvent::Snapshot(snapshot) => self.apply_snapshot(snapshot),
                BackendEvent::Update(state) => self.apply_state(state),
                BackendEvent::Sample(sample) => self.apply_sample(sample),
                BackendEvent::CommandSucceeded => self.command_error = None,
                BackendEvent::CommandError(error) => self.command_error = Some(error),
                BackendEvent::Diagnostic(event) => self.log_entries.push(LogEntry {
                    direction: match event.direction {
                        DiagnosticDirection::In => LogDirection::In,
                        DiagnosticDirection::Out => LogDirection::Out,
                    },
                    label: event.label,
                    timestamp: ctx.input(|input| input.time),
                    raw_bytes: event.raw_bytes,
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{DeviceState, TestStatus};

    #[test]
    fn semantic_snapshot_reconstructs_view_state() {
        let mut session = DeviceSession {
            transport_mode: TransportMode::Remote,
            ..DeviceSession::default()
        };
        session.apply_snapshot(AuthoritativeSnapshot {
            connection: ServerConnectionState::Connected,
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
            capabilities: Capabilities {
                adjust: true,
                ..Capabilities::default()
            },
            history: vec![sample("new-run", 0, 12)],
            ..AuthoritativeSnapshot::default()
        });
        assert_eq!(session.samples, vec![sample("new-run", 0, 12)]);
        assert_eq!(session.live_milli_ampere_hours, 100_005);
        assert!(session.mode_on);
        assert!(session.can_adjust());
    }

    #[test]
    fn remote_snapshot_uses_serialized_capabilities_without_reconstructing_policy() {
        let mut session = DeviceSession {
            transport_mode: TransportMode::Remote,
            ..DeviceSession::default()
        };
        session.apply_snapshot(AuthoritativeSnapshot {
            connection: ServerConnectionState::Connected,
            device: DeviceState {
                activity_known: true,
                active: false,
                voltage_mv: Some(4200),
                ..DeviceState::default()
            },
            test: TestStatus {
                state: TestState::Idle,
                ..TestStatus::default()
            },
            capabilities: Capabilities::default(),
            ..AuthoritativeSnapshot::default()
        });

        assert!(!session.can_start());
        assert!(!session.can_calibrate());
    }

    #[test]
    fn samples_are_deduplicated_and_new_runs_clear_history() {
        let mut session = DeviceSession::default();
        session.apply_sample(sample("run", 1, 1));
        session.apply_sample(sample("run", 1, 1));
        assert_eq!(session.samples.len(), 1);
        session.apply_sample(sample("new", 0, 2));
        assert_eq!(session.samples, vec![sample("new", 0, 2)]);
    }

    #[test]
    fn incremental_history_stays_bounded_and_preserves_extrema() {
        let mut session = DeviceSession::default();
        for sequence in 0..20_000 {
            let mut value = sample("run", sequence, sequence);
            if sequence == 123 {
                value.voltage_mv = 1;
            }
            if sequence == 456 {
                value.voltage_mv = u16::MAX;
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
    }

    fn sample(run_id: &str, sequence: u64, elapsed_seconds: u64) -> Sample {
        Sample {
            run_id: run_id.to_owned(),
            sequence,
            timestamp_utc: String::new(),
            elapsed_seconds,
            voltage_mv: 3900,
            current_ma: 1000,
            capacity_mah: 5,
            energy_wh: 0.005,
            mode: device::DeviceMode::DischargeConstantCurrent,
        }
    }
}
