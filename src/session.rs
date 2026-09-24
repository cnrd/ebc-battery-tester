mod history;

use std::collections::BTreeSet;

use crate::backend::{
    BackendCommand, BackendConnectionStatus, BackendEvent, BackendState, DiagnosticDirection,
};
use crate::backend_client::BackendClient;
#[cfg(not(target_arch = "wasm32"))]
use crate::backend_client::BackendTarget;
use crate::core::{
    ApiCommand, AuthoritativeSnapshot, Capabilities, CreateSavedRecipeRequest, CurrentRunMetadata,
    CycleRecipe, CycleSample, CycleState, CycleStatus, DeleteSavedRecipeRequest, RecipeExport,
    RenameRequest, Sample, SavedRecipe, SavedRecipeReference, ServerConnectionState,
    StartCycleRequest, StartSavedRecipeRequest, StartTestRequest, TestConfiguration, TestState,
    UpdateSavedRecipeRequest, cycle_presentation_history, power_microwatts,
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
    let bucket_count = (limit - 2) / 6;
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
                .clone()
                .max_by_key(|index| samples[*index].current_ma)
                .unwrap_or(start),
        );
        selected.insert(
            indices
                .clone()
                .min_by_key(|index| {
                    power_microwatts(samples[*index].voltage_mv, samples[*index].current_ma)
                })
                .unwrap_or(start),
        );
        selected.insert(
            indices
                .max_by_key(|index| {
                    power_microwatts(samples[*index].voltage_mv, samples[*index].current_ma)
                })
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
    pub(crate) history: history::HistoryState,
    pub(crate) samples: Vec<Sample>,
    pub(crate) cycle_samples: Vec<CycleSample>,
    pub(crate) current_device_mode: Option<device::DeviceMode>,
    /// Snapshot metadata for plotting only; physical state stays backend-owned.
    pub(crate) current_test_config: Option<TestConfiguration>,
    pub(crate) activity_known: bool,
    pub(crate) mode_on: bool,
    pub(crate) test_state: TestState,
    pub(crate) cycle: CycleStatus,
    pub(crate) current_run: CurrentRunMetadata,
    pub(crate) saved_recipes: Vec<SavedRecipe>,
    pub(crate) log_entries: Vec<LogEntry>,
    pub(crate) command_error: Option<String>,
    transport_mode: TransportMode,
    capabilities: Capabilities,
    elapsed_seconds: u64,
    remote_run_id: Option<String>,
    last_remote_sequence: Option<u64>,
    cycle_execution_id: Option<String>,
    last_cycle_sequence: Option<u64>,
    pending_recipe_export: Option<RecipeExport>,
    pending_created_recipe: Option<SavedRecipe>,
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
            history: history::HistoryState::default(),
            samples: Vec::new(),
            cycle_samples: Vec::new(),
            current_device_mode: None,
            current_test_config: None,
            activity_known: false,
            mode_on: false,
            test_state: TestState::Idle,
            cycle: CycleStatus::default(),
            current_run: CurrentRunMetadata::default(),
            saved_recipes: Vec::new(),
            log_entries: Vec::new(),
            command_error: None,
            transport_mode: TransportMode::Direct,
            capabilities: Capabilities::default(),
            elapsed_seconds: 0,
            remote_run_id: None,
            last_remote_sequence: None,
            cycle_execution_id: None,
            last_cycle_sequence: None,
            pending_recipe_export: None,
            pending_created_recipe: None,
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
        self.capabilities.start && !self.cycle_owns_orchestration()
    }

    pub(crate) fn can_resume(&self) -> bool {
        self.capabilities.resume && !self.cycle_owns_orchestration()
    }

    pub(crate) fn can_calibrate(&self) -> bool {
        self.capabilities.calibrate_voltage && !self.cycle_owns_orchestration()
    }

    pub(crate) fn can_calibrate_voltage(&self) -> bool {
        self.capabilities.calibrate_voltage && !self.cycle_owns_orchestration()
    }

    pub(crate) fn can_calibrate_current(&self) -> bool {
        self.capabilities.calibrate_current && !self.cycle_owns_orchestration()
    }

    pub(crate) fn can_confirm_calibration(&self) -> bool {
        self.capabilities.confirm_calibration && !self.cycle_owns_orchestration()
    }

    pub(crate) fn can_adjust(&self) -> bool {
        self.capabilities.adjust && !self.cycle_owns_orchestration()
    }

    pub(crate) fn show_stop_control(&self) -> bool {
        self.capabilities.show_stop || self.cycle_owns_orchestration()
    }

    pub(crate) fn can_stop(&self) -> bool {
        self.capabilities.stop || self.cycle_owns_orchestration()
    }

    pub(crate) fn cycle_owns_orchestration(&self) -> bool {
        matches!(
            self.cycle.state,
            CycleState::Preparing
                | CycleState::StartingStep
                | CycleState::RunningStep
                | CycleState::Settling
                | CycleState::Resting
                | CycleState::Stopping
        )
    }

    pub(crate) fn start_test(&mut self, request: StartTestRequest) {
        if !self.remote_command_available("test start") {
            return;
        }
        self.backend.command(BackendCommand::StartTest(request));
    }

    pub(crate) fn start_cycle(&mut self, request: StartCycleRequest) {
        if self.is_remote() && self.remote_status != BackendConnectionStatus::Connected {
            self.command_error =
                Some("browser is disconnected; cycle command was not sent".to_owned());
            return;
        }
        self.backend.command(BackendCommand::StartCycle(request));
    }

    pub(crate) fn start_saved_recipe(&mut self, recipe_id: String, execution_name: Option<String>) {
        if !self.remote_command_available("saved recipe start") {
            return;
        }
        self.backend.command(BackendCommand::StartSavedRecipe {
            recipe_id,
            request: StartSavedRecipeRequest { execution_name },
        });
    }

    pub(crate) fn start_local_saved_recipe(
        &self,
        recipe: CycleRecipe,
        reference: SavedRecipeReference,
        execution_name: Option<String>,
    ) {
        self.backend
            .command(BackendCommand::StartSavedRecipeSnapshot {
                recipe,
                reference,
                execution_name,
            });
    }

    pub(crate) fn create_saved_recipe(&mut self, request: CreateSavedRecipeRequest) {
        if self.remote_command_available("saved recipe create") {
            self.backend
                .command(BackendCommand::CreateSavedRecipe(request));
        }
    }

    pub(crate) fn update_saved_recipe(
        &mut self,
        recipe_id: String,
        request: UpdateSavedRecipeRequest,
    ) {
        if self.remote_command_available("saved recipe update") {
            self.backend
                .command(BackendCommand::UpdateSavedRecipe { recipe_id, request });
        }
    }

    pub(crate) fn delete_saved_recipe(
        &mut self,
        recipe_id: String,
        request: DeleteSavedRecipeRequest,
    ) {
        if self.remote_command_available("saved recipe delete") {
            self.backend
                .command(BackendCommand::DeleteSavedRecipe { recipe_id, request });
        }
    }

    pub(crate) fn import_recipe(&mut self, recipe: RecipeExport) {
        if self.remote_command_available("recipe import") {
            self.backend.command(BackendCommand::ImportRecipe(recipe));
        }
    }

    pub(crate) fn export_recipe(&mut self, recipe_id: String) {
        if self.remote_command_available("recipe export") {
            self.backend
                .command(BackendCommand::ExportRecipe { recipe_id });
        }
    }

    pub(crate) fn refresh_recipes(&mut self) {
        if self.remote_command_available("recipe refresh") {
            self.backend.command(BackendCommand::RefreshRecipes);
        }
    }

    pub(crate) fn take_recipe_export(&mut self) -> Option<RecipeExport> {
        self.pending_recipe_export.take()
    }

    pub(crate) fn take_created_recipe(&mut self) -> Option<SavedRecipe> {
        self.pending_created_recipe.take()
    }

    fn apply_recipe_upsert(&mut self, recipe: SavedRecipe) {
        if let Some(existing) = self
            .saved_recipes
            .iter_mut()
            .find(|existing| existing.id == recipe.id)
        {
            if recipe.revision >= existing.revision {
                *existing = recipe;
            }
        } else {
            self.saved_recipes.push(recipe);
        }
        self.saved_recipes.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
    }

    pub(crate) fn rename_current_run(&mut self, request: RenameRequest) {
        if !self.remote_command_available("run rename") {
            return;
        }
        let Some(run_id) = self.current_run.id.clone() else {
            self.command_error = Some("there is no current run to rename".to_owned());
            return;
        };
        self.rename_run(run_id, request);
    }

    pub(crate) fn rename_current_cycle(&mut self, request: RenameRequest) {
        if !self.remote_command_available("cycle rename") {
            return;
        }
        let Some(execution_id) = self.cycle.execution_id.clone() else {
            self.command_error = Some("there is no current cycle to rename".to_owned());
            return;
        };
        self.rename_cycle(execution_id, request);
    }

    fn remote_command_available(&mut self, description: &str) -> bool {
        if self.is_remote() && self.remote_status != BackendConnectionStatus::Connected {
            self.command_error = Some(format!(
                "browser is disconnected; {description} command was not sent"
            ));
            return false;
        }
        true
    }

    pub(crate) fn stop_cycle(&mut self) {
        if self.is_remote() && self.remote_status != BackendConnectionStatus::Connected {
            self.command_error =
                Some("browser is disconnected; cycle command was not sent".to_owned());
            return;
        }
        self.backend.command(BackendCommand::StopCycle);
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
        let mut cycle_history = snapshot.cycle_history;
        let cycle_execution_id = snapshot.cycle.execution_id.clone().or_else(|| {
            cycle_history
                .last()
                .map(|sample| sample.execution_id.clone())
        });
        let mut last_sequence = None;
        cycle_history.retain(|sample| {
            if cycle_execution_id.as_deref() != Some(sample.execution_id.as_str())
                || last_sequence.is_some_and(|sequence| sample.sequence <= sequence)
            {
                return false;
            }
            last_sequence = Some(sample.sequence);
            true
        });
        cycle_history = cycle_presentation_history(&cycle_history, MAX_PRESENTATION_SAMPLES);
        self.cycle_execution_id = cycle_execution_id;
        self.last_cycle_sequence = cycle_history.last().map(|sample| sample.sequence);
        self.cycle_samples = cycle_history;
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
        self.current_test_config = update.test.config;
        self.activity_known = update.device.activity_known;
        self.mode_on = update.device.active;
        self.test_state = update.test.state;
        self.current_run = update.current_run;
        if let Some(execution_id) = &update.cycle.execution_id
            && self.cycle_execution_id.as_deref() != Some(execution_id)
        {
            self.cycle_samples.clear();
            self.last_cycle_sequence = None;
            self.cycle_execution_id = Some(execution_id.clone());
        }
        self.cycle = update.cycle;
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

    fn apply_cycle_sample(&mut self, sample: CycleSample) {
        if self
            .cycle
            .execution_id
            .as_deref()
            .is_some_and(|execution_id| execution_id != sample.execution_id)
        {
            return;
        }
        if self.cycle_execution_id.as_deref() != Some(sample.execution_id.as_str()) {
            self.cycle_samples.clear();
            self.last_cycle_sequence = None;
            self.cycle_execution_id = Some(sample.execution_id.clone());
        }
        if self
            .last_cycle_sequence
            .is_some_and(|sequence| sample.sequence <= sequence)
        {
            return;
        }
        self.last_cycle_sequence = Some(sample.sequence);
        self.cycle_samples.push(sample);
        if self.cycle_samples.len() > MAX_PRESENTATION_SAMPLES {
            self.cycle_samples =
                cycle_presentation_history(&self.cycle_samples, COMPACTED_PRESENTATION_SAMPLES);
        }
    }

    pub(crate) fn consume_events(&mut self, ctx: &egui::Context) {
        while let Some(event) = self.backend.try_event() {
            match event {
                BackendEvent::History(event) => self.history.apply(event),
                BackendEvent::HistoryError(error) => {
                    self.history.pending_requests = self.history.pending_requests.saturating_sub(1);
                    self.history.error = Some(error);
                }
                BackendEvent::HistoryRenamed { id, cycle, name } => {
                    self.history.renamed(&id, cycle, name);
                }
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
                BackendEvent::BackendConnectionChanged(status) => {
                    if status != BackendConnectionStatus::Connected
                        && self.history.pending_requests > 0
                    {
                        self.history.pending_requests = 0;
                        self.history.error =
                            Some("Server disconnected. Reconnect and refresh history.".to_owned());
                    }
                    self.remote_status = status;
                }
                BackendEvent::Snapshot(snapshot) => self.apply_snapshot(snapshot),
                BackendEvent::Update(state) => self.apply_state(state),
                BackendEvent::Sample(sample) => self.apply_sample(sample),
                BackendEvent::CycleSample(sample) => self.apply_cycle_sample(sample),
                BackendEvent::RecipeLibrary(recipes) => self.saved_recipes = recipes,
                BackendEvent::RecipeUpsert(recipe) => self.apply_recipe_upsert(recipe),
                BackendEvent::RecipeCreated(recipe) => {
                    self.apply_recipe_upsert(recipe.clone());
                    self.pending_created_recipe = Some(recipe);
                }
                BackendEvent::RecipeDeleted(id) => {
                    self.saved_recipes.retain(|recipe| recipe.id != id);
                }
                BackendEvent::RecipeExported(export) => {
                    self.pending_recipe_export = Some(export);
                }
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
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use tungstenite::Message;

    use super::*;
    use crate::core::{
        CurrentRunMetadata, DeviceState, SnapshotUpdate, TestStatus, WebSocketEvent,
    };

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
    fn snapshot_and_rename_update_metadata_without_resetting_histories() {
        let mut session = DeviceSession::default();
        let run_sample = sample("run-1", 0, 1);
        let cycle_sample = cycle_sample("cycle-1", 0, 0, 0);
        session.apply_snapshot(AuthoritativeSnapshot {
            current_run: CurrentRunMetadata {
                id: Some("run-1".to_owned()),
                name: Some("initial run".to_owned()),
                cycle: None,
            },
            cycle: CycleStatus {
                execution_id: Some("cycle-1".to_owned()),
                name: Some("initial cycle".to_owned()),
                ..CycleStatus::default()
            },
            history: vec![run_sample.clone()],
            cycle_history: vec![cycle_sample.clone()],
            ..AuthoritativeSnapshot::default()
        });
        assert_eq!(session.current_run.name.as_deref(), Some("initial run"));

        session.apply_state(BackendState {
            update: SnapshotUpdate {
                current_run: CurrentRunMetadata {
                    id: Some("run-1".to_owned()),
                    name: Some("renamed run".to_owned()),
                    cycle: None,
                },
                cycle: CycleStatus {
                    execution_id: Some("cycle-1".to_owned()),
                    name: Some("renamed cycle".to_owned()),
                    ..CycleStatus::default()
                },
                ..SnapshotUpdate::default()
            },
        });

        assert_eq!(session.current_run.name.as_deref(), Some("renamed run"));
        assert_eq!(session.cycle.name.as_deref(), Some("renamed cycle"));
        assert_eq!(session.samples, vec![run_sample]);
        assert_eq!(session.cycle_samples, vec![cycle_sample]);
    }

    #[test]
    fn cycle_history_reconnects_deduplicates_and_resets_only_for_a_new_execution() {
        let mut session = DeviceSession::default();
        let first = cycle_sample("cycle-1", 0, 0, 0);
        let second = cycle_sample("cycle-1", 1, 0, 2);
        session.apply_snapshot(AuthoritativeSnapshot {
            cycle: CycleStatus {
                execution_id: Some("cycle-1".to_owned()),
                state: CycleState::RunningStep,
                ..CycleStatus::default()
            },
            cycle_history: vec![first.clone(), second.clone()],
            ..AuthoritativeSnapshot::default()
        });
        assert_eq!(session.cycle_samples, vec![first.clone(), second.clone()]);

        session.apply_cycle_sample(second);
        session.apply_cycle_sample(cycle_sample("cycle-1", 2, 1, 0));
        session.apply_sample(sample("new-physical-run", 0, 0));
        assert_eq!(session.cycle_samples.len(), 3);

        session.apply_state(BackendState {
            update: SnapshotUpdate {
                cycle: CycleStatus {
                    execution_id: Some("cycle-2".to_owned()),
                    state: CycleState::StartingStep,
                    ..CycleStatus::default()
                },
                ..SnapshotUpdate::default()
            },
        });
        assert!(session.cycle_samples.is_empty());
        session.apply_cycle_sample(cycle_sample("cycle-1", 3, 1, 1));
        assert!(session.cycle_samples.is_empty());
        session.apply_cycle_sample(cycle_sample("cycle-2", 0, 0, 0));
        assert_eq!(session.cycle_samples.len(), 1);
    }

    #[test]
    fn cycle_snapshot_rejects_other_executions_and_non_increasing_sequences() {
        let mut session = DeviceSession::default();
        session.apply_snapshot(AuthoritativeSnapshot {
            cycle: CycleStatus {
                execution_id: Some("cycle-2".to_owned()),
                state: CycleState::RunningStep,
                ..CycleStatus::default()
            },
            cycle_history: vec![
                cycle_sample("cycle-1", 8, 0, 0),
                cycle_sample("cycle-2", 0, 0, 0),
                cycle_sample("cycle-2", 0, 0, 0),
                cycle_sample("cycle-2", 2, 0, 0),
                cycle_sample("cycle-2", 1, 0, 0),
            ],
            ..AuthoritativeSnapshot::default()
        });

        assert_eq!(
            session
                .cycle_samples
                .iter()
                .map(|sample| sample.sequence)
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
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
    fn physical_compaction_preserves_distinct_power_peak() {
        let mut samples: Vec<_> = (0..12)
            .map(|sequence| sample("run", sequence, sequence))
            .collect();
        samples[2].voltage_mv = 9_000;
        samples[2].current_ma = 100;
        samples[3].voltage_mv = 5_000;
        samples[3].current_ma = 5_000;
        samples[4].voltage_mv = 100;
        samples[4].current_ma = 9_000;

        compact_samples(&mut samples, 8);
        let sequences: Vec<_> = samples.iter().map(|sample| sample.sequence).collect();
        assert!(samples.len() <= 8);
        assert_eq!(sequences.first(), Some(&0));
        assert_eq!(sequences.last(), Some(&11));
        assert!(sequences.contains(&3));
        assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
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

    #[test]
    fn recipe_events_replace_authoritatively_ignore_stale_upserts_and_delete_idempotently() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .unwrap_or_else(|error| panic!("failed to bind test server: {error}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("failed to read test server address: {error}"));
        let authoritative = saved_recipe("recipe-1", "Authoritative", 3);
        let server_recipe = authoritative.clone();
        let stale = saved_recipe("recipe-1", "Stale", 2);
        let marker = saved_recipe("marker", "Marker", 1);
        let final_marker = saved_recipe("final", "Final", 1);
        let (send_events, receive_events) = mpsc::channel::<Vec<WebSocketEvent>>();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener
                .accept()
                .unwrap_or_else(|error| panic!("failed to accept WebSocket: {error}"));
            let mut websocket = tungstenite::accept(stream)
                .unwrap_or_else(|error| panic!("failed WebSocket handshake: {error}"));
            send_websocket_event(
                &mut websocket,
                &WebSocketEvent::Snapshot(AuthoritativeSnapshot::default()),
            );
            send_websocket_event(&mut websocket, &WebSocketEvent::RecipeLibrary(Vec::new()));

            let (mut http, _) = listener
                .accept()
                .unwrap_or_else(|error| panic!("failed to accept recipe GET: {error}"));
            http.set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap_or_else(|error| panic!("failed to set HTTP timeout: {error}"));
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = http
                    .read(&mut chunk)
                    .unwrap_or_else(|error| panic!("failed to read recipe GET: {error}"));
                assert_ne!(read, 0, "recipe GET ended before headers");
                request.extend_from_slice(&chunk[..read]);
            }
            assert!(String::from_utf8_lossy(&request).starts_with("GET /api/recipes HTTP/1.1\r\n"));
            let body = serde_json::to_string(&vec![server_recipe])
                .unwrap_or_else(|error| panic!("failed to serialize recipe library: {error}"));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            http.write_all(response.as_bytes())
                .unwrap_or_else(|error| panic!("failed to send recipe library: {error}"));

            for events in receive_events {
                for event in events {
                    send_websocket_event(&mut websocket, &event);
                }
            }
        });

        let context = egui::Context::default();
        let mut session = DeviceSession::new(
            &context,
            BackendTarget::Remote,
            &format!("http://{address}"),
        )
        .unwrap_or_else(|error| panic!("failed to create remote session: {error}"));
        session.saved_recipes = vec![saved_recipe("old", "Old", 9)];
        wait_for_recipes(&mut session, &context, |recipes| {
            recipes == [authoritative.clone()]
        });

        send_events
            .send(vec![
                WebSocketEvent::RecipeUpsert(stale),
                WebSocketEvent::RecipeUpsert(marker.clone()),
            ])
            .unwrap_or_else(|error| panic!("failed to send upsert events: {error}"));
        wait_for_recipes(&mut session, &context, |recipes| recipes.contains(&marker));
        assert!(session.saved_recipes.contains(&authoritative));

        send_events
            .send(vec![
                WebSocketEvent::RecipeDelete("recipe-1".to_owned()),
                WebSocketEvent::RecipeDelete("recipe-1".to_owned()),
                WebSocketEvent::RecipeUpsert(final_marker.clone()),
            ])
            .unwrap_or_else(|error| panic!("failed to send delete events: {error}"));
        wait_for_recipes(&mut session, &context, |recipes| {
            recipes.contains(&final_marker)
        });
        assert!(
            !session
                .saved_recipes
                .iter()
                .any(|recipe| recipe.id == "recipe-1")
        );

        drop(send_events);
        drop(session);
        assert!(server.join().is_ok(), "test server panicked");
    }

    fn saved_recipe(id: &str, name: &str, revision: u64) -> SavedRecipe {
        SavedRecipe {
            id: id.to_owned(),
            name: name.to_owned(),
            recipe: CycleRecipe {
                steps: Vec::new(),
                repeat_count: 1,
            },
            revision,
            created_at_utc: "created".to_owned(),
            updated_at_utc: "updated".to_owned(),
        }
    }

    fn send_websocket_event(
        websocket: &mut tungstenite::WebSocket<std::net::TcpStream>,
        event: &WebSocketEvent,
    ) {
        let text = serde_json::to_string(&event)
            .unwrap_or_else(|error| panic!("failed to serialize WebSocket event: {error}"));
        websocket
            .send(Message::Text(text.into()))
            .unwrap_or_else(|error| panic!("failed to send WebSocket event: {error}"));
    }

    fn wait_for_recipes(
        session: &mut DeviceSession,
        context: &egui::Context,
        condition: impl Fn(&[SavedRecipe]) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            session.consume_events(context);
            if condition(&session.saved_recipes) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for recipe events");
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

    fn cycle_sample(
        execution_id: &str,
        sequence: u64,
        repeat_index: u32,
        step_index: usize,
    ) -> CycleSample {
        CycleSample {
            execution_id: execution_id.to_owned(),
            sequence,
            timestamp_utc: String::new(),
            elapsed_milliseconds: sequence * 250,
            repeat_index,
            step_index,
            cycle_state: CycleState::RunningStep,
            test_state: TestState::Running,
            mode: device::DeviceMode::DischargeConstantCurrent,
            activity_known: true,
            active: true,
            voltage_mv: 3900,
            current_ma: 1000,
            device_capacity_mah: 5,
            test_capacity_mah: Some(5),
            test_energy_wh: 0.005,
        }
    }
}
