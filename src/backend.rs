//! Semantic command and event contract between GUI clients and backends.

use std::sync::Arc;

use futures::channel::mpsc::UnboundedSender;

use crate::core::{
    ApiCommand, AuthoritativeSnapshot, CreateSavedRecipeRequest, CycleRecipe, CycleSample,
    DeleteSavedRecipeRequest, RecipeExport, RenameRequest, Sample, SavedRecipe,
    SavedRecipeReference, SnapshotUpdate, StartCycleRequest, StartSavedRecipeRequest,
    StartTestRequest, TestConfiguration, UpdateSavedRecipeRequest,
};
use crate::device::UsbDeviceInfo;

#[derive(Clone, Debug)]
pub(crate) enum BackendCommand {
    History(HistoryRequest),
    RefreshDevices,
    Connect(usize),
    Disconnect,
    Api(ApiCommand),
    StartTest(StartTestRequest),
    Resume(TestConfiguration),
    StartCycle(StartCycleRequest),
    StartSavedRecipe {
        recipe_id: String,
        request: StartSavedRecipeRequest,
    },
    StartSavedRecipeSnapshot {
        recipe: CycleRecipe,
        reference: SavedRecipeReference,
        execution_name: Option<String>,
    },
    RefreshRecipes,
    CreateSavedRecipe(CreateSavedRecipeRequest),
    UpdateSavedRecipe {
        recipe_id: String,
        request: UpdateSavedRecipeRequest,
    },
    DeleteSavedRecipe {
        recipe_id: String,
        request: DeleteSavedRecipeRequest,
    },
    ImportRecipe(RecipeExport),
    ExportRecipe {
        recipe_id: String,
    },
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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum HistoryRequest {
    RefreshRuns,
    RefreshCycles,
    LoadRun(String),
    LoadCycle(String),
    ExportRunCsv(String),
    ExportCycleCsv(String),
}

#[derive(Clone, Debug)]
pub(crate) struct DownloadedFile {
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(crate) enum HistoryEvent {
    Runs(Vec<crate::core::RunSummary>),
    Cycles(Vec<crate::core::CycleSummary>),
    RunLoaded(crate::core::RunHistory),
    CycleLoaded(crate::core::CycleHistory),
    FileExported(DownloadedFile),
}

impl HistoryRequest {
    pub(crate) fn path(&self) -> String {
        match self {
            Self::RefreshRuns => "/api/runs".to_owned(),
            Self::RefreshCycles => "/api/cycles".to_owned(),
            Self::LoadRun(id) => format!("/api/runs/{id}"),
            Self::LoadCycle(id) => format!("/api/cycles/{id}"),
            Self::ExportRunCsv(id) => format!("/api/runs/{id}/history.csv"),
            Self::ExportCycleCsv(id) => format!("/api/cycles/{id}/history.csv"),
        }
    }

    pub(crate) fn decode(&self, bytes: Vec<u8>) -> Result<HistoryEvent, String> {
        let result = match self {
            Self::RefreshRuns => serde_json::from_slice(&bytes).map(HistoryEvent::Runs),
            Self::RefreshCycles => serde_json::from_slice(&bytes).map(HistoryEvent::Cycles),
            Self::LoadRun(_) => serde_json::from_slice(&bytes).map(HistoryEvent::RunLoaded),
            Self::LoadCycle(_) => serde_json::from_slice(&bytes).map(HistoryEvent::CycleLoaded),
            Self::ExportRunCsv(id) | Self::ExportCycleCsv(id) => {
                return Ok(HistoryEvent::FileExported(DownloadedFile {
                    filename: format!("{id}.csv"),
                    content_type: "text/csv".to_owned(),
                    bytes,
                }));
            }
        };
        result.map_err(|error| format!("invalid history response: {error}"))
    }
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
    HistoryResult {
        request: HistoryRequest,
        result: Result<HistoryEvent, String>,
    },
    HistoryRenamed {
        id: String,
        cycle: bool,
        name: Option<String>,
    },
    DevicesUpdated(Vec<UsbDeviceInfo>),
    BackendConnectionChanged(BackendConnectionStatus),
    Snapshot(AuthoritativeSnapshot),
    Update(BackendState),
    Sample(Sample),
    CycleSample(CycleSample),
    RecipeLibrary(Vec<SavedRecipe>),
    RecipeUpsert(SavedRecipe),
    RecipeCreated(SavedRecipe),
    RecipeDeleted(String),
    RecipeExported(RecipeExport),
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
        BackendCommand::History(_)
        | BackendCommand::RefreshDevices
        | BackendCommand::StartTest(_)
        | BackendCommand::StartCycle(_)
        | BackendCommand::StartSavedRecipe { .. }
        | BackendCommand::StartSavedRecipeSnapshot { .. }
        | BackendCommand::RefreshRecipes
        | BackendCommand::CreateSavedRecipe(_)
        | BackendCommand::UpdateSavedRecipe { .. }
        | BackendCommand::DeleteSavedRecipe { .. }
        | BackendCommand::ImportRecipe(_)
        | BackendCommand::ExportRecipe { .. }
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
