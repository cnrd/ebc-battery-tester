//! Semantic command and event contract between GUI clients and backends.

use std::sync::Arc;

use futures::channel::mpsc::UnboundedSender;

use crate::core::{
    ApiCommand, AuthoritativeSnapshot, CycleSample, RenameRequest, Sample, SnapshotUpdate,
    StartCycleRequest, StartTestRequest, TestConfiguration,
};
use crate::device::UsbDeviceInfo;

#[derive(Clone, Debug)]
pub(crate) enum BackendCommand {
    RefreshDevices,
    Connect(usize),
    Disconnect,
    Api(ApiCommand),
    StartTest(StartTestRequest),
    Resume(TestConfiguration),
    StartCycle(StartCycleRequest),
    RenameRun {
        run_id: String,
        request: RenameRequest,
    },
    RenameCycle {
        execution_id: String,
        request: RenameRequest,
    },
    StopCycle,
    Shutdown,
}

#[derive(Clone, Debug)]
pub(crate) struct BackendState {
    pub update: SnapshotUpdate,
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
pub(crate) enum BackendConnectionStatus {
    #[default]
    NotUsed,
    Connecting,
    Connected,
    Reconnecting,
    #[cfg_attr(
        all(not(target_arch = "wasm32"), not(test)),
        expect(
            dead_code,
            reason = "native reconnect failures retain reconnecting status"
        )
    )]
    Error(String),
}

#[derive(Clone, Debug)]
pub(crate) enum BackendEvent {
    DevicesUpdated(Vec<UsbDeviceInfo>),
    BackendConnectionChanged(BackendConnectionStatus),
    Snapshot(AuthoritativeSnapshot),
    Update(BackendState),
    Sample(Sample),
    CycleSample(CycleSample),
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

#[expect(
    clippy::needless_pass_by_value,
    reason = "backend command dispatchers transfer command ownership"
)]
pub(crate) fn remote_api_commands(command: BackendCommand) -> Vec<ApiCommand> {
    match command {
        BackendCommand::Connect(_) => vec![ApiCommand::Connect],
        BackendCommand::Disconnect => vec![ApiCommand::Disconnect],
        BackendCommand::Api(command) => vec![command],
        BackendCommand::Resume(_) => vec![ApiCommand::Resume],
        BackendCommand::RefreshDevices
        | BackendCommand::StartTest(_)
        | BackendCommand::StartCycle(_)
        | BackendCommand::RenameRun { .. }
        | BackendCommand::RenameCycle { .. }
        | BackendCommand::StopCycle
        | BackendCommand::Shutdown => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{CycleRecipe, RenameRequest, StartCycleRequest, StartTestRequest};

    fn config() -> TestConfiguration {
        TestConfiguration::DischargeConstantCurrent {
            current_ma: 1000,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 0,
        }
    }

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
            [ApiCommand::Disconnect]
        ));
    }

    #[test]
    fn envelope_commands_are_not_mapped_back_to_api_commands() {
        let commands = [
            BackendCommand::StartTest(StartTestRequest {
                config: config(),
                name: Some("run".to_owned()),
            }),
            BackendCommand::StartCycle(StartCycleRequest {
                recipe: CycleRecipe {
                    steps: Vec::new(),
                    repeat_count: 1,
                },
                name: Some("cycle".to_owned()),
            }),
            BackendCommand::RenameRun {
                run_id: "run-1".to_owned(),
                request: RenameRequest::default(),
            },
            BackendCommand::RenameCycle {
                execution_id: "cycle-1".to_owned(),
                request: RenameRequest::default(),
            },
        ];
        for command in commands {
            assert!(remote_api_commands(command).is_empty());
        }
    }
}
