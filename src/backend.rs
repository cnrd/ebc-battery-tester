//! Semantic command and event contract between GUI clients and backends.

use std::sync::Arc;

use futures::channel::mpsc::UnboundedSender;

use crate::core::{
    ApiCommand, AuthoritativeSnapshot, Sample, SnapshotUpdate, TestConfiguration, TestState,
};
use crate::device::{DeviceMode, UsbDeviceInfo};

#[derive(Clone, Copy, Debug)]
pub(crate) enum BackendCommand {
    RefreshDevices,
    Connect(usize),
    Disconnect,
    Api(ApiCommand),
    Resume(TestConfiguration),
    Shutdown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BackendCapabilities {
    pub start: bool,
    pub resume: bool,
    pub stop: bool,
    pub show_stop: bool,
    pub adjust: bool,
    pub calibrate_voltage: bool,
    pub calibrate_current: bool,
    pub confirm_calibration: bool,
}

impl BackendCapabilities {
    #[cfg_attr(
        all(not(target_arch = "wasm32"), not(test)),
        expect(
            dead_code,
            reason = "remote state projection is used by the WASM client"
        )
    )]
    pub(crate) fn from_remote(update: &SnapshotUpdate) -> Self {
        let connected = update.connection == crate::core::ServerConnectionState::Connected;
        let fresh = connected && update.device.activity_known;
        let inactive = fresh && !update.device.active;
        let active = fresh && update.device.active;
        let live_voltage = update.device.voltage_mv.unwrap_or(0) > 0;
        let idle = matches!(
            update.test.state,
            TestState::Idle | TestState::Stopped | TestState::Completed
        );
        let show_stop = update.device.active
            || matches!(
                update.test.state,
                TestState::Starting
                    | TestState::Running
                    | TestState::Stopping
                    | TestState::RecoveredUncertain
            );
        let calibration_state_allowed = !matches!(
            update.test.state,
            TestState::RecoveredUncertain | TestState::Starting | TestState::Stopping
        );
        let calibrate_voltage = fresh && calibration_state_allowed && live_voltage;
        Self {
            start: inactive && live_voltage && idle,
            resume: inactive && live_voltage && update.test.state == TestState::Stopped,
            stop: connected && show_stop && update.test.state != TestState::Stopping,
            show_stop,
            adjust: active
                && update.test.state == TestState::Running
                && update.device.mode == Some(DeviceMode::DischargeConstantCurrent),
            calibrate_voltage,
            calibrate_current: calibrate_voltage
                && active
                && update.test.state == TestState::Running
                && update.device.mode == Some(DeviceMode::DischargeConstantCurrent),
            // The server remains authoritative for calibration staging.
            confirm_calibration: calibrate_voltage,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BackendState {
    pub update: SnapshotUpdate,
    pub capabilities: BackendCapabilities,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiagnosticDirection {
    In,
    Out,
}

#[derive(Clone, Debug)]
pub(crate) struct DiagnosticEvent {
    pub direction: DiagnosticDirection,
    pub label: String,
    pub raw_bytes: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    all(not(target_arch = "wasm32"), not(test)),
    expect(
        dead_code,
        reason = "remote connection states are produced by the WASM client"
    )
)]
pub(crate) enum BackendConnectionStatus {
    #[default]
    NotUsed,
    Connecting,
    Connected,
    Reconnecting,
    Error(String),
}

#[derive(Clone, Debug)]
#[cfg_attr(
    all(not(target_arch = "wasm32"), not(test)),
    expect(
        dead_code,
        reason = "remote connection events are produced by the WASM client"
    )
)]
pub(crate) enum BackendEvent {
    DevicesUpdated(Vec<UsbDeviceInfo>),
    BackendConnectionChanged(BackendConnectionStatus),
    Snapshot {
        snapshot: AuthoritativeSnapshot,
        capabilities: BackendCapabilities,
    },
    Update(BackendState),
    Sample(Sample),
    CommandSucceeded,
    CommandError(String),
    Diagnostic(DiagnosticEvent),
}

#[derive(Clone)]
pub(crate) struct BackendEventSender {
    sender: UnboundedSender<BackendEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl BackendEventSender {
    pub(crate) fn new(
        sender: UnboundedSender<BackendEvent>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            sender,
            wake: Arc::new(wake),
        }
    }

    pub(crate) fn send(&self, event: BackendEvent) {
        self.sender.unbounded_send(event).ok();
        (self.wake)();
    }
}

#[cfg_attr(
    all(not(target_arch = "wasm32"), not(test)),
    expect(
        dead_code,
        reason = "remote API mapping is consumed by the WASM client"
    )
)]
pub(crate) fn remote_api_commands(command: BackendCommand) -> Vec<ApiCommand> {
    match command {
        BackendCommand::Connect(_) => vec![ApiCommand::Connect],
        BackendCommand::Disconnect => vec![ApiCommand::Stop, ApiCommand::Disconnect],
        BackendCommand::Api(command) => vec![command],
        BackendCommand::Resume(_) => vec![ApiCommand::Resume],
        BackendCommand::RefreshDevices | BackendCommand::Shutdown => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_shutdown_does_not_issue_a_server_command() {
        let statuses = [
            BackendConnectionStatus::Reconnecting,
            BackendConnectionStatus::Error("offline".to_owned()),
        ];
        assert_eq!(statuses.len(), 2);
        assert!(matches!(
            BackendEvent::BackendConnectionChanged(statuses[0].clone()),
            BackendEvent::BackendConnectionChanged(BackendConnectionStatus::Reconnecting)
        ));
        assert!(remote_api_commands(BackendCommand::Shutdown).is_empty());
        assert!(matches!(
            remote_api_commands(BackendCommand::Disconnect).as_slice(),
            [ApiCommand::Stop, ApiCommand::Disconnect]
        ));
    }
}
