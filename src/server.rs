//! Native headless server and the single-owner device actor.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read as _, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path as AxumPath, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot};
use tokio::{fs as tokio_fs, io::AsyncReadExt as _};
use tokio_util::io::ReaderStream;
use tower_http::services::{ServeDir, ServeFile};

use crate::controller::{
    CommandKind, ControllerMode, DeviceReport, PreparedCommand, ReportState, TestController,
};
mod history;
#[cfg(test)]
mod history_tests;

use crate::core::{
    ApiCommand, AuthoritativeSnapshot, CalibrationCommand, CreateSavedRecipeRequest,
    CurrentRunMetadata, CycleHistory, CycleRecipe, CycleRunContext, CycleSample, CycleState,
    CycleStatus, CycleSummary, DeleteSavedRecipeRequest, RECIPE_EXPORT_FORMAT,
    RECIPE_EXPORT_VERSION, RecipeExport, RenameRequest, RunHistory, RunSummary, Sample,
    SavedRecipe, SavedRecipeReference, ServerConnectionState, SnapshotUpdate, StartCycleRequest,
    StartSavedRecipeRequest, StartTestRequest, TestConfiguration, TestState,
    UpdateSavedRecipeRequest, WebSocketEvent, cycle_presentation_history, normalize_optional_name,
    normalize_required_name,
};
use crate::cycle::{CycleAction, CycleEngine};
use crate::device::{self, InboundFrame, OUTBOUND_FRAME_SIZE, OutboundFrame};

const SNAPSHOT_CHANNEL_CAPACITY: usize = 16;
const SNAPSHOT_SAMPLE_LIMIT: usize = 5_000;
const SERIAL_TIMEOUT: Duration = Duration::from_millis(20);
const ACTOR_TICK: Duration = Duration::from_millis(100);

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub http_addr: SocketAddr,
    pub serial_port: String,
    pub data_dir: PathBuf,
    pub mock: bool,
    pub static_dir: PathBuf,
}

impl ServerConfig {
    /// Reads server configuration from the documented `EBC_*` variables.
    ///
    /// # Errors
    /// Returns an error when the listen address or mock boolean is invalid.
    pub fn from_env() -> Result<Self, String> {
        let http_addr = std::env::var("EBC_HTTP_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_owned())
            .parse()
            .map_err(|error| format!("invalid EBC_HTTP_ADDR: {error}"))?;
        let mock = std::env::var("EBC_MOCK")
            .unwrap_or_else(|_| "false".to_owned())
            .parse()
            .map_err(|error| format!("invalid EBC_MOCK boolean: {error}"))?;
        Ok(Self {
            http_addr,
            serial_port: std::env::var("EBC_SERIAL_PORT")
                .unwrap_or_else(|_| "/dev/ttyUSB0".to_owned()),
            data_dir: std::env::var_os("EBC_DATA_DIR")
                .map_or_else(|| PathBuf::from("/data"), PathBuf::from),
            mock,
            static_dir: std::env::var_os("EBC_STATIC_DIR")
                .map_or_else(|| PathBuf::from("dist"), PathBuf::from),
        })
    }
}

#[derive(Clone)]
struct AppState {
    actor_tx: std_mpsc::Sender<ActorMessage>,
    allowed_origin: Option<String>,
}

enum ActorRequest {
    Snapshot,
    Subscribe,
    History,
    HistoryCsv,
    CycleHistoryCsv,
    CycleCsv(String),
    Runs,
    RunHistory(String),
    Cycles,
    CycleHistory(String),
    RunCsv(String),
    Recipes,
    CreateRecipe(CreateSavedRecipeRequest),
    UpdateRecipe {
        id: String,
        request: UpdateSavedRecipeRequest,
    },
    DeleteRecipe {
        id: String,
        request: DeleteSavedRecipeRequest,
    },
    ImportRecipe(RecipeExport),
    ExportRecipe(String),
    StartSavedRecipe {
        id: String,
        request: StartSavedRecipeRequest,
    },
    Command(ApiCommand),
    StartTest(StartTestRequest),
    StartCycle(StartCycleRequest),
    RenameRun {
        id: String,
        request: RenameRequest,
    },
    RenameCycle {
        execution_id: String,
        request: RenameRequest,
    },
    StopCycle,
    Shutdown,
}

enum ActorResponse {
    Snapshot(AuthoritativeSnapshot),
    Subscription(
        AuthoritativeSnapshot,
        Vec<SavedRecipe>,
        broadcast::Receiver<WebSocketEvent>,
    ),
    History(Vec<Sample>),
    Runs(Vec<RunSummary>),
    RunHistory(Result<Option<RunHistory>, String>),
    Cycles(Result<Vec<CycleSummary>, String>),
    CycleHistory(Result<Option<CycleHistory>, String>),
    ArchivedExport(Result<Option<ExportDescriptor>, String>),
    Export(ExportDescriptor),
    Start(Result<AuthoritativeSnapshot, StartError>),
    Rename(Result<AuthoritativeSnapshot, RenameError>),
    Recipes(Vec<SavedRecipe>),
    Recipe(Result<SavedRecipe, RecipeError>),
    RecipeExport(Result<RecipeExport, RecipeError>),
}

#[derive(Debug)]
enum StartError {
    BadRequest(String),
    NotFound(String),
    Internal(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(message) | Self::NotFound(message) | Self::Internal(message) => {
                formatter.write_str(message)
            }
        }
    }
}

#[derive(Debug)]
enum RenameError {
    BadRequest(String),
    NotFound(String),
    Internal(String),
}

#[derive(Debug)]
enum RecipeError {
    BadRequest(String),
    NotFound(String),
    Conflict(String),
    Internal(String),
}

struct ExportDescriptor {
    file: File,
    length: u64,
    filename: String,
}

struct ActorMessage {
    request: ActorRequest,
    response: oneshot::Sender<Result<ActorResponse, String>>,
}

struct Persistence {
    metadata_path: PathBuf,
    samples_path: PathBuf,
    runs_dir: PathBuf,
    runs: Vec<RunSummary>,
    archived_run_id: Option<String>,
    current_run_id: String,
    current_run_name: Option<String>,
    current_run_cycle: Option<CycleRunContext>,
    next_sequence: u64,
    raw_sample_count: usize,
    sample_writer: Option<BufWriter<File>>,
    last_sample_sync: Instant,
    cycles_dir: PathBuf,
    current_cycle_id: Option<String>,
    current_cycle_name: Option<String>,
    next_cycle_sequence: u64,
    raw_cycle_sample_count: usize,
    cycle_writer: Option<BufWriter<File>>,
    last_cycle_sync: Instant,
    recipes_dir: PathBuf,
    recipes: Vec<SavedRecipe>,
    reserved_recipe_ids: BTreeSet<String>,
}

#[derive(Serialize, Deserialize)]
struct Metadata {
    connection: ServerConnectionState,
    connection_error: Option<String>,
    device: crate::core::DeviceState,
    test: crate::core::TestStatus,
    #[serde(default)]
    cycle: CycleStatus,
    #[serde(default)]
    archived_run_id: Option<String>,
    #[serde(default)]
    current_run_id: String,
    #[serde(default)]
    current_run_name: Option<String>,
    #[serde(default)]
    current_run_cycle: Option<CycleRunContext>,
    #[serde(default)]
    next_sequence: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct CycleExecutionMetadata {
    execution_id: String,
    name: Option<String>,
    recipe: Option<CycleRecipe>,
    saved_recipe: Option<SavedRecipeReference>,
    #[serde(alias = "started_at")]
    started_at_utc: Option<String>,
    state: Option<CycleState>,
    result: Option<String>,
    elapsed_milliseconds: Option<u64>,
    sample_count: Option<usize>,
}

impl Persistence {
    fn new(data_dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(data_dir).map_err(|error| {
            format!(
                "failed to create data directory {}: {error}",
                data_dir.display()
            )
        })?;
        let runs_dir = data_dir.join("runs");
        fs::create_dir_all(&runs_dir).map_err(|error| {
            format!(
                "failed to create archive directory {}: {error}",
                runs_dir.display()
            )
        })?;
        let cycles_dir = data_dir.join("cycles");
        fs::create_dir_all(&cycles_dir).map_err(|error| {
            format!(
                "failed to create cycle telemetry directory {}: {error}",
                cycles_dir.display()
            )
        })?;
        let recipes_dir = data_dir.join("recipes");
        fs::create_dir_all(&recipes_dir).map_err(|error| {
            format!(
                "failed to create recipe directory {}: {error}",
                recipes_dir.display()
            )
        })?;
        let mut persistence = Self {
            metadata_path: data_dir.join("session.json"),
            samples_path: data_dir.join("samples.csv"),
            runs_dir,
            runs: Vec::new(),
            archived_run_id: None,
            current_run_id: String::new(),
            current_run_name: None,
            current_run_cycle: None,
            next_sequence: 0,
            raw_sample_count: 0,
            sample_writer: None,
            last_sample_sync: Instant::now(),
            cycles_dir,
            current_cycle_id: None,
            current_cycle_name: None,
            next_cycle_sequence: 0,
            raw_cycle_sample_count: 0,
            cycle_writer: None,
            last_cycle_sync: Instant::now(),
            recipes_dir,
            recipes: Vec::new(),
            reserved_recipe_ids: BTreeSet::new(),
        };
        persistence.runs = persistence.load_run_summaries()?;
        (persistence.recipes, persistence.reserved_recipe_ids) =
            persistence.load_saved_recipes()?;
        Ok(persistence)
    }

    fn load(&mut self) -> Result<AuthoritativeSnapshot, String> {
        self.flush_samples()?;
        let mut snapshot = if self.metadata_path.exists() {
            let bytes = fs::read(&self.metadata_path).map_err(|error| error.to_string())?;
            let metadata: Metadata =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            self.archived_run_id = metadata.archived_run_id;
            self.current_run_id = metadata.current_run_id;
            self.current_run_name = metadata.current_run_name;
            self.current_run_cycle = metadata.current_run_cycle;
            self.next_sequence = metadata.next_sequence;
            AuthoritativeSnapshot {
                connection: ServerConnectionState::Disconnected,
                connection_error: None,
                device: metadata.device,
                test: metadata.test,
                cycle: metadata.cycle,
                capabilities: Default::default(),
                current_run: CurrentRunMetadata::default(),
                history: Vec::new(),
                cycle_history: Vec::new(),
            }
        } else {
            AuthoritativeSnapshot::default()
        };
        snapshot.device.active = false;
        snapshot.device.activity_known = false;
        if matches!(
            snapshot.test.state,
            TestState::Starting
                | TestState::Running
                | TestState::Stopping
                | TestState::RecoveredUncertain
        ) {
            snapshot.test.state = TestState::RecoveredUncertain;
            snapshot.test.result =
                Some("server restarted; device state is not yet confirmed".to_owned());
        }
        snapshot.history = self.load_samples()?;
        self.raw_sample_count = snapshot.history.len();
        if self.current_run_id.is_empty() && !snapshot.history.is_empty() {
            self.current_run_id = snapshot
                .history
                .iter()
                .find(|sample| !sample.run_id.is_empty())
                .map_or_else(
                    || "legacy-current".to_owned(),
                    |sample| sample.run_id.clone(),
                );
        }
        let mut normalized_legacy = false;
        let mut previous_sequence = None;
        for (index, sample) in snapshot.history.iter_mut().enumerate() {
            if sample.run_id.is_empty() {
                sample.run_id.clone_from(&self.current_run_id);
                normalized_legacy = true;
            }
            let minimum = previous_sequence.map_or(0, |sequence: u64| sequence.saturating_add(1));
            if sample.sequence < minimum || (index > 0 && sample.sequence == 0) {
                sample.sequence = minimum;
                normalized_legacy = true;
            }
            previous_sequence = Some(sample.sequence);
        }
        if normalized_legacy {
            self.rewrite_samples(&snapshot.history)?;
        }
        if let Some(last) = snapshot.history.last() {
            snapshot.test.elapsed_seconds = snapshot.test.elapsed_seconds.max(last.elapsed_seconds);
            snapshot.test.energy_wh = snapshot.test.energy_wh.max(last.energy_wh);
            snapshot.test.capacity_mah = Some(
                snapshot
                    .test
                    .capacity_mah
                    .unwrap_or(0)
                    .max(last.capacity_mah),
            );
            self.next_sequence = self.next_sequence.max(last.sequence.saturating_add(1));
            if self.current_run_id.is_empty() {
                self.current_run_id.clone_from(&last.run_id);
            }
        }
        self.load_current_cycle(&mut snapshot)?;
        self.sync_snapshot_metadata(&mut snapshot);
        Ok(snapshot)
    }

    fn load_current_cycle(&mut self, snapshot: &mut AuthoritativeSnapshot) -> Result<(), String> {
        let Some(execution_id) = snapshot.cycle.execution_id.clone() else {
            return Ok(());
        };
        let cycle_history = self.load_cycle_samples(&execution_id)?;
        if let Some(metadata) = self.load_cycle_metadata(&execution_id)? {
            self.current_cycle_name = metadata.name;
            if metadata.recipe.is_some() {
                snapshot.cycle.recipe = metadata.recipe;
            }
            if metadata.saved_recipe.is_some() {
                snapshot.cycle.saved_recipe = metadata.saved_recipe;
            }
            if metadata.started_at_utc.is_some() {
                snapshot.cycle.started_at_utc = metadata.started_at_utc;
            }
        }
        snapshot.cycle.name.clone_from(&self.current_cycle_name);
        self.current_cycle_id = Some(execution_id);
        self.next_cycle_sequence = cycle_history
            .last()
            .map_or(0, |sample| sample.sequence.saturating_add(1));
        self.raw_cycle_sample_count = cycle_history.len();
        snapshot.cycle_history = cycle_history;
        Ok(())
    }

    fn save_metadata(&self, snapshot: &AuthoritativeSnapshot) -> Result<(), String> {
        let metadata = Metadata {
            connection: snapshot.connection.clone(),
            connection_error: snapshot.connection_error.clone(),
            device: snapshot.device.clone(),
            test: snapshot.test.clone(),
            cycle: snapshot.cycle.clone(),
            archived_run_id: self.archived_run_id.clone(),
            current_run_id: self.current_run_id.clone(),
            current_run_name: self.current_run_name.clone(),
            current_run_cycle: self.current_run_cycle.clone(),
            next_sequence: self.next_sequence,
        };
        atomic_write_json(&self.metadata_path, &metadata)
    }

    fn reset_samples(&mut self) -> Result<(), String> {
        self.sample_writer = None;
        let temporary = self.samples_path.with_extension("csv.tmp");
        let mut file = File::create(&temporary).map_err(|error| error.to_string())?;
        file.write_all(csv_header().as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::rename(temporary, &self.samples_path).map_err(|error| error.to_string())?;
        self.raw_sample_count = 0;
        sync_parent(&self.samples_path)
    }

    fn rewrite_samples(&mut self, samples: &[Sample]) -> Result<(), String> {
        self.sample_writer = None;
        let temporary = self.samples_path.with_extension("csv.tmp");
        let mut file = File::create(&temporary).map_err(|error| error.to_string())?;
        file.write_all(Self::history_csv(samples).as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::rename(temporary, &self.samples_path).map_err(|error| error.to_string())?;
        sync_parent(&self.samples_path)
    }

    fn begin_current_run(
        &mut self,
        run_id: String,
        name: Option<String>,
        cycle: Option<CycleRunContext>,
    ) {
        self.archived_run_id = None;
        self.current_run_id = run_id;
        self.current_run_name = name;
        self.current_run_cycle = cycle;
        self.next_sequence = 0;
    }

    fn restore_current_samples(
        &mut self,
        archived_run_id: &str,
        sample_count: usize,
    ) -> Result<(), String> {
        self.sample_writer = None;
        let source = self.runs_dir.join(format!("{archived_run_id}.csv"));
        let temporary = self.samples_path.with_extension("csv.tmp");
        fs::copy(source, &temporary).map_err(|error| error.to_string())?;
        File::open(&temporary)
            .and_then(|file| file.sync_all())
            .map_err(|error| error.to_string())?;
        fs::rename(temporary, &self.samples_path).map_err(|error| error.to_string())?;
        self.raw_sample_count = sample_count;
        sync_parent(&self.samples_path)
    }

    fn new_run_id(&self) -> String {
        self.next_run_id(Some(&Utc::now().to_rfc3339()))
    }

    fn archive_current(
        &mut self,
        snapshot: &AuthoritativeSnapshot,
    ) -> Result<Option<RunSummary>, String> {
        if snapshot.test.config.is_none()
            && snapshot.history.is_empty()
            && self.raw_sample_count == 0
        {
            return Ok(None);
        }
        if let Some(id) = &self.archived_run_id
            && let Some(summary) = self.runs.iter().find(|run| &run.id == id)
            && (!matches!(
                snapshot.test.state,
                TestState::Completed | TestState::Stopped
            ) || (summary.sample_count == self.raw_sample_count
                && summary.state == snapshot.test.state
                && summary.result == snapshot.test.result
                && summary.elapsed_seconds == snapshot.test.elapsed_seconds))
        {
            return Ok(Some(summary.clone()));
        }

        self.flush_samples()?;
        let id = if let Some(id) = &self.archived_run_id {
            id.clone()
        } else if valid_run_id(&self.current_run_id) {
            self.current_run_id.clone()
        } else {
            self.next_run_id(snapshot.test.started_at_utc.as_deref())
        };
        let csv_path = self.runs_dir.join(format!("{id}.csv"));
        let csv_temporary = self.runs_dir.join(format!("{id}.csv.tmp"));
        if self.samples_path.exists() {
            fs::copy(&self.samples_path, &csv_temporary).map_err(|error| error.to_string())?;
        } else {
            let mut file = File::create(&csv_temporary).map_err(|error| error.to_string())?;
            file.write_all(Self::history_csv(&snapshot.history).as_bytes())
                .map_err(|error| error.to_string())?;
        }
        File::open(&csv_temporary)
            .and_then(|file| file.sync_all())
            .map_err(|error| error.to_string())?;
        let summary = RunSummary {
            id: id.clone(),
            name: if self.current_run_cycle.is_none() {
                self.current_run_name.clone()
            } else {
                None
            },
            started_at_utc: snapshot.test.started_at_utc.clone(),
            archived_at_utc: Utc::now().to_rfc3339(),
            state: snapshot.test.state.clone(),
            config: snapshot.test.config,
            elapsed_seconds: snapshot.test.elapsed_seconds,
            result: snapshot.test.result.clone(),
            capacity_mah: snapshot
                .test
                .capacity_mah
                .or_else(|| snapshot.device.capacity_mah.map(u64::from)),
            energy_wh: snapshot.test.energy_wh,
            model: snapshot.device.model.clone(),
            firmware_version: snapshot.device.firmware_version.clone(),
            sample_count: if self.samples_path.exists() {
                self.raw_sample_count
            } else {
                snapshot.history.len()
            },
            cycle: self.current_run_cycle.clone(),
        };
        let json_path = self.runs_dir.join(format!("{id}.json"));
        let json_temporary = self.runs_dir.join(format!("{id}.json.tmp"));
        let mut file = File::create(&json_temporary).map_err(|error| error.to_string())?;
        serde_json::to_writer_pretty(&mut file, &summary).map_err(|error| error.to_string())?;
        file.write_all(b"\n").map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::rename(&csv_temporary, &csv_path).map_err(|error| error.to_string())?;
        fs::rename(&json_temporary, &json_path).map_err(|error| error.to_string())?;
        sync_directory(&self.runs_dir)?;
        if let Some(existing) = self.runs.iter_mut().find(|run| run.id == id) {
            *existing = summary.clone();
        } else {
            self.runs.push(summary.clone());
        }
        self.runs.sort_by(|left, right| right.id.cmp(&left.id));
        self.archived_run_id = Some(id);
        self.save_metadata(snapshot)?;
        Ok(Some(summary))
    }

    fn next_run_id(&self, started_at_utc: Option<&str>) -> String {
        let source = started_at_utc
            .map(str::to_owned)
            .unwrap_or_else(|| Utc::now().to_rfc3339());
        let base: String = source
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                    character
                } else {
                    '-'
                }
            })
            .collect();
        let mut id = base.clone();
        let mut collision = 2_u32;
        while self.runs_dir.join(format!("{id}.json")).exists()
            || self.runs_dir.join(format!("{id}.csv")).exists()
        {
            id = format!("{base}-{collision}");
            collision += 1;
        }
        id
    }

    fn load_run_summaries(&self) -> Result<Vec<RunSummary>, String> {
        let mut runs = Vec::new();
        for entry in fs::read_dir(&self.runs_dir).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if !valid_run_id(id) {
                continue;
            }
            if !self.runs_dir.join(format!("{id}.csv")).is_file() {
                continue;
            }
            let bytes = fs::read(&path).map_err(|error| error.to_string())?;
            let summary: RunSummary = serde_json::from_slice(&bytes)
                .map_err(|error| format!("invalid run summary {}: {error}", path.display()))?;
            if summary.id != id {
                return Err(format!("run summary id mismatch in {}", path.display()));
            }
            runs.push(summary);
        }
        runs.sort_by(|left, right| right.id.cmp(&left.id));
        Ok(runs)
    }

    fn run_summaries(&self) -> Vec<RunSummary> {
        self.runs.clone()
    }

    fn rewrite_run_summary(&self, summary: &RunSummary) -> Result<(), String> {
        let path = self.runs_dir.join(format!("{}.json", summary.id));
        atomic_write_json(&path, summary)
    }

    fn load_saved_recipes(&self) -> Result<(Vec<SavedRecipe>, BTreeSet<String>), String> {
        let mut recipes = Vec::new();
        let mut reserved = BTreeSet::new();
        for entry in fs::read_dir(&self.recipes_dir).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            if !path.is_file() {
                return Err(format!(
                    "unexpected entry in recipe directory: {}",
                    path.display()
                ));
            }
            let extension = path.extension().and_then(|extension| extension.to_str());
            if extension == Some("tmp") || extension == Some("rollback") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| format!("invalid recipe filename: {}", path.display()))?;
            if !valid_run_id(id) {
                return Err(format!("invalid recipe id in filename: {}", path.display()));
            }
            if !reserved.insert(id.to_owned()) {
                return Err(format!("duplicate reserved recipe id: {id}"));
            }
            if extension == Some("deleted") {
                continue;
            }
            if extension != Some("json") {
                return Err(format!(
                    "unexpected file in recipe directory: {}",
                    path.display()
                ));
            }
            let bytes = fs::read(&path).map_err(|error| error.to_string())?;
            let recipe: SavedRecipe = serde_json::from_slice(&bytes)
                .map_err(|error| format!("invalid saved recipe {}: {error}", path.display()))?;
            validate_saved_recipe(&recipe, id)
                .map_err(|error| format!("invalid saved recipe {}: {error}", path.display()))?;
            recipes.push(recipe);
        }
        sort_saved_recipes(&mut recipes);
        Ok((recipes, reserved))
    }

    fn saved_recipes(&self) -> Vec<SavedRecipe> {
        self.recipes.clone()
    }

    fn saved_recipe(&self, id: &str) -> Option<&SavedRecipe> {
        self.recipes.iter().find(|recipe| recipe.id == id)
    }

    fn create_saved_recipe(
        &mut self,
        name: String,
        recipe: CycleRecipe,
    ) -> Result<SavedRecipe, String> {
        let now = Utc::now().to_rfc3339();
        let id = self.next_recipe_id(&now);
        let saved = SavedRecipe {
            id: id.clone(),
            name,
            recipe,
            revision: 1,
            created_at_utc: now.clone(),
            updated_at_utc: now,
        };
        atomic_write_json(&self.recipe_path(&id), &saved)?;
        self.reserved_recipe_ids.insert(id);
        self.recipes.push(saved.clone());
        sort_saved_recipes(&mut self.recipes);
        Ok(saved)
    }

    fn update_saved_recipe(
        &mut self,
        id: &str,
        request: UpdateSavedRecipeRequest,
    ) -> Result<SavedRecipe, RecipeError> {
        let Some(index) = self.recipes.iter().position(|recipe| recipe.id == id) else {
            return Err(RecipeError::NotFound("saved recipe not found".to_owned()));
        };
        if self.recipes[index].revision != request.expected_revision {
            return Err(RecipeError::Conflict(format!(
                "saved recipe revision is {}; expected {}",
                self.recipes[index].revision, request.expected_revision
            )));
        }
        let mut updated = self.recipes[index].clone();
        updated.name = request.name;
        updated.recipe = request.recipe;
        updated.revision = updated
            .revision
            .checked_add(1)
            .ok_or_else(|| RecipeError::Internal("saved recipe revision overflow".to_owned()))?;
        updated.updated_at_utc = Utc::now().to_rfc3339();
        atomic_write_json(&self.recipe_path(id), &updated).map_err(RecipeError::Internal)?;
        self.recipes[index] = updated.clone();
        sort_saved_recipes(&mut self.recipes);
        Ok(updated)
    }

    fn delete_saved_recipe(
        &mut self,
        id: &str,
        expected_revision: u64,
    ) -> Result<SavedRecipe, RecipeError> {
        let Some(index) = self.recipes.iter().position(|recipe| recipe.id == id) else {
            return Err(RecipeError::NotFound("saved recipe not found".to_owned()));
        };
        if self.recipes[index].revision != expected_revision {
            return Err(RecipeError::Conflict(format!(
                "saved recipe revision is {}; expected {expected_revision}",
                self.recipes[index].revision
            )));
        }
        let path = self.recipe_path(id);
        let tombstone = self.recipes_dir.join(format!("{id}.deleted"));
        fs::rename(&path, &tombstone).map_err(|error| RecipeError::Internal(error.to_string()))?;
        if let Err(error) = sync_directory(&self.recipes_dir) {
            let rollback = fs::rename(&tombstone, &path)
                .map_err(|rollback_error| rollback_error.to_string())
                .and_then(|()| sync_directory(&self.recipes_dir));
            return Err(RecipeError::Internal(match rollback {
                Ok(()) => error,
                Err(rollback_error) => {
                    format!("{error}; failed to restore deleted recipe: {rollback_error}")
                }
            }));
        }
        Ok(self.recipes.remove(index))
    }

    fn next_recipe_id(&self, timestamp: &str) -> String {
        let base = format!("recipe-{}", timestamp_id(timestamp));
        let mut id = base.clone();
        let mut collision = 2_u32;
        while self.reserved_recipe_ids.contains(&id)
            || self.recipe_path(&id).exists()
            || self.recipes_dir.join(format!("{id}.deleted")).exists()
        {
            id = format!("{base}-{collision}");
            collision = collision.saturating_add(1);
        }
        id
    }

    fn recipe_path(&self, id: &str) -> PathBuf {
        self.recipes_dir.join(format!("{id}.json"))
    }

    fn live_export(&mut self) -> Result<ExportDescriptor, String> {
        self.flush_samples()?;
        if !self.samples_path.exists() {
            self.reset_samples()?;
        }
        open_export(&self.samples_path, "history.csv".to_owned())
    }

    fn run_export(&self, id: &str) -> Result<ExportDescriptor, String> {
        if !valid_run_id(id) {
            return Err("invalid run id".to_owned());
        }
        if !self.runs.iter().any(|run| run.id == id) {
            return Err("archived run not found".to_owned());
        }
        open_export(
            &self.runs_dir.join(format!("{id}.csv")),
            format!("{id}.csv"),
        )
    }

    fn begin_cycle(
        &mut self,
        execution_id: String,
        name: Option<String>,
        recipe: CycleRecipe,
        saved_recipe: Option<SavedRecipeReference>,
        started_at_utc: String,
    ) -> Result<(), String> {
        if !valid_run_id(&execution_id) {
            return Err("invalid cycle execution id".to_owned());
        }
        self.flush_cycle_samples()?;
        self.cycle_writer = None;
        let path = self.cycle_path(&execution_id);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|error| error.to_string())?;
        file.write_all(cycle_csv_header().as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        let metadata = CycleExecutionMetadata {
            execution_id: execution_id.clone(),
            name: name.clone(),
            recipe: Some(recipe),
            saved_recipe,
            started_at_utc: Some(started_at_utc),
            state: Some(CycleState::Preparing),
            result: None,
            elapsed_milliseconds: Some(0),
            sample_count: Some(0),
        };
        if let Err(error) = self.write_cycle_metadata(&metadata) {
            let cleanup = fs::remove_file(&path);
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup_error) => {
                    format!("{error}; failed to remove incomplete cycle CSV: {cleanup_error}")
                }
            });
        }
        sync_directory(&self.cycles_dir)?;
        self.current_cycle_id = Some(execution_id);
        self.current_cycle_name = name;
        self.next_cycle_sequence = 0;
        self.raw_cycle_sample_count = 0;
        self.last_cycle_sync = Instant::now();
        Ok(())
    }

    fn append_cycle_sample(&mut self, sample: &CycleSample) -> Result<(), String> {
        if self.current_cycle_id.as_deref() != Some(sample.execution_id.as_str()) {
            return Err(
                "cycle sample execution does not match the active telemetry file".to_owned(),
            );
        }
        if self.cycle_writer.is_none() {
            let path = self.cycle_path(&sample.execution_id);
            self.cycle_writer = Some(BufWriter::new(
                OpenOptions::new()
                    .append(true)
                    .open(path)
                    .map_err(|error| error.to_string())?,
            ));
        }
        self.cycle_writer
            .as_mut()
            .ok_or_else(|| "cycle sample writer was not initialized".to_owned())?
            .write_all(cycle_sample_csv_row(sample).as_bytes())
            .map_err(|error| error.to_string())?;
        if self.last_cycle_sync.elapsed() >= Duration::from_secs(1) {
            self.flush_cycle_samples()?;
        }
        self.next_cycle_sequence = sample.sequence.saturating_add(1);
        self.raw_cycle_sample_count = self.raw_cycle_sample_count.saturating_add(1);
        Ok(())
    }

    fn flush_cycle_samples(&mut self) -> Result<(), String> {
        if let Some(writer) = &mut self.cycle_writer {
            writer.flush().map_err(|error| error.to_string())?;
            writer
                .get_ref()
                .sync_data()
                .map_err(|error| error.to_string())?;
        }
        self.last_cycle_sync = Instant::now();
        Ok(())
    }

    fn cycle_export(&mut self, execution_id: Option<&str>) -> Result<ExportDescriptor, String> {
        let id = execution_id
            .map(str::to_owned)
            .or_else(|| self.current_cycle_id.clone())
            .ok_or_else(|| "cycle telemetry is unavailable".to_owned())?;
        if !valid_run_id(&id) {
            return Err("invalid cycle execution id".to_owned());
        }
        if self.current_cycle_id.as_deref() == Some(id.as_str()) {
            self.flush_cycle_samples()?;
        }
        let path = self.cycle_path(&id);
        if !path.is_file() {
            return Err("cycle telemetry was not found".to_owned());
        }
        open_export(&path, format!("{id}.csv"))
    }

    fn cycle_path(&self, execution_id: &str) -> PathBuf {
        self.cycles_dir.join(format!("{execution_id}.csv"))
    }

    fn cycle_metadata_path(&self, execution_id: &str) -> PathBuf {
        self.cycles_dir.join(format!("{execution_id}.json"))
    }

    fn load_cycle_metadata(
        &self,
        execution_id: &str,
    ) -> Result<Option<CycleExecutionMetadata>, String> {
        let path = self.cycle_metadata_path(execution_id);
        if !path.is_file() {
            return Ok(None);
        }
        let bytes = fs::read(&path).map_err(|error| {
            format!("could not read cycle metadata {}: {error}", path.display())
        })?;
        let metadata: CycleExecutionMetadata = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid cycle metadata {}: {error}", path.display()))?;
        if metadata.execution_id != execution_id {
            return Err(format!(
                "cycle metadata execution id mismatch in {}",
                path.display()
            ));
        }
        Ok(Some(metadata))
    }

    fn write_cycle_metadata(&self, metadata: &CycleExecutionMetadata) -> Result<(), String> {
        atomic_write_json(&self.cycle_metadata_path(&metadata.execution_id), metadata)
    }

    fn sync_snapshot_metadata(&self, snapshot: &mut AuthoritativeSnapshot) {
        snapshot.current_run = CurrentRunMetadata {
            id: (!self.current_run_id.is_empty()).then(|| self.current_run_id.clone()),
            name: self.current_run_name.clone(),
            cycle: self.current_run_cycle.clone(),
        };
        if snapshot.cycle.execution_id.as_deref() == self.current_cycle_id.as_deref() {
            snapshot.cycle.name.clone_from(&self.current_cycle_name);
        }
    }

    fn append_sample(&mut self, sample: &Sample) -> Result<(), String> {
        if self.sample_writer.is_none() {
            let new_file = !self.samples_path.exists();
            let mut writer = BufWriter::new(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.samples_path)
                    .map_err(|error| error.to_string())?,
            );
            if new_file {
                writer
                    .write_all(csv_header().as_bytes())
                    .map_err(|error| error.to_string())?;
            }
            self.sample_writer = Some(writer);
        }
        self.sample_writer
            .as_mut()
            .ok_or_else(|| "sample writer was not initialized".to_owned())?
            .write_all(sample_csv_row(sample).as_bytes())
            .map_err(|error| error.to_string())?;
        if self.last_sample_sync.elapsed() >= Duration::from_secs(1) {
            self.flush_samples()?;
        }
        self.next_sequence = sample.sequence.saturating_add(1);
        self.raw_sample_count = self.raw_sample_count.saturating_add(1);
        Ok(())
    }

    fn flush_samples(&mut self) -> Result<(), String> {
        if let Some(writer) = &mut self.sample_writer {
            writer.flush().map_err(|error| error.to_string())?;
            writer
                .get_ref()
                .sync_data()
                .map_err(|error| error.to_string())?;
        }
        self.last_sample_sync = Instant::now();
        Ok(())
    }

    fn load_samples(&self) -> Result<Vec<Sample>, String> {
        Self::load_run_samples(&self.samples_path)
    }

    fn load_run_samples(path: &Path) -> Result<Vec<Sample>, String> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(path).map_err(|error| error.to_string())?;
        let terminated = bytes.last() == Some(&b'\n');
        let text = String::from_utf8(bytes.clone()).map_err(|error| error.to_string())?;
        let mut lines = text.split_terminator('\n');
        let header = lines.next().unwrap_or_default();
        if !valid_csv_header(header) {
            return Err("invalid samples.csv header".to_owned());
        }
        let rows: Vec<&str> = lines.collect();
        let mut samples = Vec::with_capacity(rows.len());
        for (index, line) in rows.iter().enumerate() {
            if line.is_empty() {
                return Err("invalid empty complete samples.csv row".to_owned());
            }
            match parse_sample(line) {
                Ok(sample) => samples.push(sample),
                Err(_) if !terminated && index + 1 == rows.len() => {
                    let length = bytes
                        .iter()
                        .rposition(|byte| *byte == b'\n')
                        .map_or(0, |position| position + 1);
                    let file = OpenOptions::new()
                        .write(true)
                        .open(path)
                        .map_err(|error| error.to_string())?;
                    file.set_len(u64::try_from(length).map_err(|error| error.to_string())?)
                        .map_err(|error| error.to_string())?;
                    file.sync_all().map_err(|error| error.to_string())?;
                    sync_parent(path)?;
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        if !terminated && rows.last().is_some_and(|line| parse_sample(line).is_ok()) {
            let mut file = OpenOptions::new()
                .append(true)
                .open(path)
                .map_err(|error| error.to_string())?;
            file.write_all(b"\n").map_err(|error| error.to_string())?;
            file.sync_all().map_err(|error| error.to_string())?;
        }
        Ok(samples)
    }

    fn load_cycle_samples(&self, execution_id: &str) -> Result<Vec<CycleSample>, String> {
        let path = self.cycle_path(execution_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(&path).map_err(|error| error.to_string())?;
        let terminated = bytes.last() == Some(&b'\n');
        let text = String::from_utf8(bytes.clone()).map_err(|error| error.to_string())?;
        let mut lines = text.split_terminator('\n');
        if lines.next().unwrap_or_default() != cycle_csv_header().trim_end() {
            return Err(format!(
                "invalid cycle telemetry header in {}",
                path.display()
            ));
        }
        let rows: Vec<&str> = lines.collect();
        let mut samples = Vec::with_capacity(rows.len());
        for (index, line) in rows.iter().enumerate() {
            if line.is_empty() {
                return Err("invalid empty complete cycle telemetry row".to_owned());
            }
            match parse_cycle_sample(line) {
                Ok(sample) if sample.execution_id == execution_id => {
                    if samples
                        .last()
                        .is_some_and(|previous: &CycleSample| previous.sequence >= sample.sequence)
                    {
                        return Err(
                            "cycle telemetry sequence is not strictly increasing".to_owned()
                        );
                    }
                    samples.push(sample);
                }
                Ok(_) => return Err("cycle telemetry execution id mismatch".to_owned()),
                Err(_) if !terminated && index + 1 == rows.len() => {
                    let length = bytes
                        .iter()
                        .rposition(|byte| *byte == b'\n')
                        .map_or(0, |position| position + 1);
                    let file = OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .map_err(|error| error.to_string())?;
                    file.set_len(u64::try_from(length).map_err(|error| error.to_string())?)
                        .map_err(|error| error.to_string())?;
                    file.sync_all().map_err(|error| error.to_string())?;
                    sync_parent(&path)?;
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        if !terminated
            && rows
                .last()
                .is_some_and(|line| parse_cycle_sample(line).is_ok())
        {
            let mut file = OpenOptions::new()
                .append(true)
                .open(&path)
                .map_err(|error| error.to_string())?;
            file.write_all(b"\n").map_err(|error| error.to_string())?;
            file.sync_all().map_err(|error| error.to_string())?;
        }
        Ok(samples)
    }

    fn history_csv(samples: &[Sample]) -> String {
        let mut csv = String::from(csv_header());
        for sample in samples {
            csv.push_str(&sample_csv_row(sample));
        }
        csv
    }
}

fn open_export(path: &Path, filename: String) -> Result<ExportDescriptor, String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    Ok(ExportDescriptor {
        file,
        length,
        filename,
    })
}

fn csv_header() -> &'static str {
    "run_id,sequence,timestamp_utc,elapsed_seconds,voltage_mv,current_ma,capacity_mah,energy_wh,mode\n"
}

fn valid_csv_header(header: &str) -> bool {
    matches!(
        header,
        "timestamp_utc,elapsed_seconds,voltage_mv,current_ma,capacity_mah,mode"
            | "timestamp_utc,elapsed_seconds,voltage_mv,current_ma,capacity_mah,energy_wh,mode"
            | "run_id,sequence,timestamp_utc,elapsed_seconds,voltage_mv,current_ma,capacity_mah,energy_wh,mode"
    )
}

fn sample_csv_row(sample: &Sample) -> String {
    format!(
        "{},{},{},{},{},{},{},{:.9},{:?}\n",
        sample.run_id,
        sample.sequence,
        sample.timestamp_utc,
        sample.elapsed_seconds,
        sample.voltage_mv,
        sample.current_ma,
        sample.capacity_mah,
        sample.energy_wh,
        sample.mode
    )
}

fn parse_sample(line: &str) -> Result<Sample, String> {
    let fields: Vec<&str> = line.split(',').collect();
    if !matches!(fields.len(), 6 | 7 | 9) {
        return Err(format!("invalid samples.csv row: {line}"));
    }
    let (offset, run_id, sequence) = if fields.len() == 9 {
        (
            2,
            fields[0].to_owned(),
            fields[1]
                .parse()
                .map_err(|error| format!("invalid sequence: {error}"))?,
        )
    } else {
        (0, String::new(), 0)
    };
    let (energy_wh, mode_field) = if fields.len() == 7 || fields.len() == 9 {
        (
            fields[5 + offset]
                .parse()
                .map_err(|error| format!("invalid energy: {error}"))?,
            fields[6 + offset],
        )
    } else {
        (0.0, fields[5])
    };
    let mode = match mode_field {
        "DischargeConstantCurrent" => device::DeviceMode::DischargeConstantCurrent,
        "DischargeConstantPower" => device::DeviceMode::DischargeConstantPower,
        "ChargeConstantVoltage" => device::DeviceMode::ChargeConstantVoltage,
        value => return Err(format!("invalid device mode in samples.csv: {value}")),
    };
    Ok(Sample {
        run_id,
        sequence,
        timestamp_utc: fields[offset].to_owned(),
        elapsed_seconds: fields[1 + offset]
            .parse()
            .map_err(|error| format!("invalid elapsed: {error}"))?,
        voltage_mv: fields[2 + offset]
            .parse()
            .map_err(|error| format!("invalid voltage: {error}"))?,
        current_ma: fields[3 + offset]
            .parse()
            .map_err(|error| format!("invalid current: {error}"))?,
        capacity_mah: fields[4 + offset]
            .parse()
            .map_err(|error| format!("invalid capacity: {error}"))?,
        energy_wh,
        mode,
    })
}

fn cycle_csv_header() -> &'static str {
    "execution_id,sequence,timestamp_utc,elapsed_milliseconds,repeat_index,step_index,cycle_state,test_state,mode,activity_known,active,voltage_mv,current_ma,device_capacity_mah,test_capacity_mah,test_energy_wh\n"
}

fn cycle_sample_csv_row(sample: &CycleSample) -> String {
    format!(
        "{},{},{},{},{},{},{:?},{:?},{:?},{},{},{},{},{},{},{:.9}\n",
        sample.execution_id,
        sample.sequence,
        sample.timestamp_utc,
        sample.elapsed_milliseconds,
        sample.repeat_index,
        sample.step_index,
        sample.cycle_state,
        sample.test_state,
        sample.mode,
        u8::from(sample.activity_known),
        u8::from(sample.active),
        sample.voltage_mv,
        sample.current_ma,
        sample.device_capacity_mah,
        sample
            .test_capacity_mah
            .map_or_else(String::new, |value| value.to_string()),
        sample.test_energy_wh,
    )
}

fn parse_cycle_sample(line: &str) -> Result<CycleSample, String> {
    let fields: Vec<&str> = line.split(',').collect();
    if fields.len() != 16 {
        return Err(format!("invalid cycle telemetry row: {line}"));
    }
    let cycle_state = match fields[6] {
        "Idle" => CycleState::Idle,
        "Preparing" => CycleState::Preparing,
        "StartingStep" => CycleState::StartingStep,
        "RunningStep" => CycleState::RunningStep,
        "Settling" => CycleState::Settling,
        "Resting" => CycleState::Resting,
        "Stopping" => CycleState::Stopping,
        "Completed" => CycleState::Completed,
        "Stopped" => CycleState::Stopped,
        "Interrupted" => CycleState::Interrupted,
        value => return Err(format!("invalid cycle state in telemetry: {value}")),
    };
    let test_state = match fields[7] {
        "Idle" => TestState::Idle,
        "Starting" => TestState::Starting,
        "Running" => TestState::Running,
        "Stopping" => TestState::Stopping,
        "Stopped" => TestState::Stopped,
        "Completed" => TestState::Completed,
        "RecoveredUncertain" => TestState::RecoveredUncertain,
        value => return Err(format!("invalid test state in cycle telemetry: {value}")),
    };
    let mode = match fields[8] {
        "DischargeConstantCurrent" => device::DeviceMode::DischargeConstantCurrent,
        "DischargeConstantPower" => device::DeviceMode::DischargeConstantPower,
        "ChargeConstantVoltage" => device::DeviceMode::ChargeConstantVoltage,
        value => return Err(format!("invalid device mode in cycle telemetry: {value}")),
    };
    let parse_bool = |value: &str| match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(format!("invalid boolean in cycle telemetry: {value}")),
    };
    Ok(CycleSample {
        execution_id: fields[0].to_owned(),
        sequence: fields[1]
            .parse()
            .map_err(|error| format!("invalid cycle sequence: {error}"))?,
        timestamp_utc: fields[2].to_owned(),
        elapsed_milliseconds: fields[3]
            .parse()
            .map_err(|error| format!("invalid cycle elapsed time: {error}"))?,
        repeat_index: fields[4]
            .parse()
            .map_err(|error| format!("invalid repeat index: {error}"))?,
        step_index: fields[5]
            .parse()
            .map_err(|error| format!("invalid step index: {error}"))?,
        cycle_state,
        test_state,
        mode,
        activity_known: parse_bool(fields[9])?,
        active: parse_bool(fields[10])?,
        voltage_mv: fields[11]
            .parse()
            .map_err(|error| format!("invalid cycle voltage: {error}"))?,
        current_ma: fields[12]
            .parse()
            .map_err(|error| format!("invalid cycle current: {error}"))?,
        device_capacity_mah: fields[13]
            .parse()
            .map_err(|error| format!("invalid cycle device capacity: {error}"))?,
        test_capacity_mah: if fields[14].is_empty() {
            None
        } else {
            Some(
                fields[14]
                    .parse()
                    .map_err(|error| format!("invalid cycle test capacity: {error}"))?,
            )
        },
        test_energy_wh: fields[15]
            .parse()
            .map_err(|error| format!("invalid cycle test energy: {error}"))?,
    })
}

fn valid_run_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 120
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn timestamp_id(timestamp: &str) -> String {
    timestamp
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn validate_saved_recipe(recipe: &SavedRecipe, filename_id: &str) -> Result<(), String> {
    if recipe.id != filename_id || !valid_run_id(&recipe.id) {
        return Err("id does not match its filename".to_owned());
    }
    let normalized = normalize_required_name(&recipe.name).map_err(|error| error.to_string())?;
    if normalized != recipe.name {
        return Err("name is not normalized".to_owned());
    }
    recipe
        .recipe
        .validate()
        .map_err(|error| error.to_string())?;
    if recipe.revision == 0 {
        return Err("revision must be at least 1".to_owned());
    }
    let created = chrono::DateTime::parse_from_rfc3339(&recipe.created_at_utc)
        .map_err(|error| format!("invalid created_at_utc: {error}"))?;
    let updated = chrono::DateTime::parse_from_rfc3339(&recipe.updated_at_utc)
        .map_err(|error| format!("invalid updated_at_utc: {error}"))?;
    if updated < created {
        return Err("updated_at_utc precedes created_at_utc".to_owned());
    }
    Ok(())
}

fn sort_saved_recipes(recipes: &mut [SavedRecipe]) {
    recipes.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn presentation_history(samples: &[Sample], limit: usize) -> Vec<Sample> {
    if samples.len() <= limit {
        return samples.to_vec();
    }
    if limit < 2 {
        return samples.last().cloned().into_iter().collect();
    }
    let bucket_count = (limit - 2) / 4;
    if bucket_count == 0 {
        return vec![samples[0].clone(), samples[samples.len() - 1].clone()];
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
    selected
        .into_iter()
        .map(|index| samples[index].clone())
        .collect()
}

fn sync_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    sync_directory(parent)
}

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("failed to sync directory {}: {error}", path.display()))
}

fn atomic_write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<(), String> {
    let original = match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("json");
    let temporary = path.with_extension(format!("{extension}.tmp"));
    let mut file = File::create(&temporary).map_err(|error| error.to_string())?;
    serde_json::to_writer_pretty(&mut file, value).map_err(|error| error.to_string())?;
    file.write_all(b"\n").map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    sync_parent(&temporary)?;
    fs::rename(&temporary, path).map_err(|error| error.to_string())?;
    if let Err(error) = sync_parent(path) {
        let rollback = if let Some(bytes) = original {
            let rollback = path.with_extension(format!("{extension}.rollback"));
            File::create(&rollback)
                .and_then(|mut file| {
                    file.write_all(&bytes)?;
                    file.sync_all()
                })
                .map_err(|rollback_error| rollback_error.to_string())
                .and_then(|()| {
                    fs::rename(&rollback, path).map_err(|rollback_error| rollback_error.to_string())
                })
                .and_then(|()| sync_parent(path))
        } else {
            fs::remove_file(path)
                .map_err(|rollback_error| rollback_error.to_string())
                .and_then(|()| sync_parent(path))
        };
        return Err(match rollback {
            Ok(()) => error,
            Err(rollback_error) => {
                format!("{error}; failed to roll back replacement: {rollback_error}")
            }
        });
    }
    Ok(())
}

struct DeviceActor {
    config: ServerConfig,
    snapshot: AuthoritativeSnapshot,
    controller: TestController,
    cycle: CycleEngine,
    persistence: Persistence,
    port: Option<Box<dyn serialport::SerialPort>>,
    serial_buffer: Vec<u8>,
    mock_sample_number: u64,
    last_mock_sample: Instant,
    mock_idle_report_due: Option<Instant>,
    last_metadata_sync: Instant,
    snapshot_tx: broadcast::Sender<WebSocketEvent>,
    #[cfg(test)]
    write_failure: Option<String>,
    #[cfg(test)]
    write_failure_after: Option<(usize, String)>,
    #[cfg(test)]
    sent_frames: Vec<OutboundFrame>,
    #[cfg(test)]
    start_metadata_failure: Option<String>,
}

struct StartPreparationRollback {
    history: Vec<Sample>,
    archived_run_id: Option<String>,
    current_run_id: String,
    current_run_name: Option<String>,
    current_run_cycle: Option<CycleRunContext>,
    next_sequence: u64,
    raw_sample_count: usize,
}

impl DeviceActor {
    fn new(
        config: ServerConfig,
        snapshot_tx: broadcast::Sender<WebSocketEvent>,
    ) -> Result<Self, String> {
        let mut persistence = Persistence::new(&config.data_dir)?;
        let mut snapshot = persistence.load()?;
        persistence.recover_cycle_history(&mut snapshot)?;
        let controller = TestController::from_state(
            ControllerMode::Server,
            snapshot.device.clone(),
            snapshot.test.clone(),
            snapshot.history.last(),
        );
        let cycle = CycleEngine::from_persisted_status(snapshot.cycle.clone());
        snapshot.cycle = cycle.status().clone();
        if snapshot.history.len() > SNAPSHOT_SAMPLE_LIMIT {
            snapshot.history = presentation_history(&snapshot.history, SNAPSHOT_SAMPLE_LIMIT);
        }
        if snapshot.cycle_history.len() > SNAPSHOT_SAMPLE_LIMIT {
            snapshot.cycle_history =
                cycle_presentation_history(&snapshot.cycle_history, SNAPSHOT_SAMPLE_LIMIT);
        }
        Ok(Self {
            config,
            snapshot,
            controller,
            cycle,
            persistence,
            port: None,
            serial_buffer: Vec::new(),
            mock_sample_number: 0,
            last_mock_sample: Instant::now(),
            mock_idle_report_due: None,
            last_metadata_sync: Instant::now(),
            snapshot_tx,
            #[cfg(test)]
            write_failure: None,
            #[cfg(test)]
            write_failure_after: None,
            #[cfg(test)]
            sent_frames: Vec::new(),
            #[cfg(test)]
            start_metadata_failure: None,
        })
    }

    fn run(mut self, rx: &std_mpsc::Receiver<ActorMessage>) {
        if let Err(error) = self.connect() {
            self.set_connection_error(&error);
        }
        self.publish();
        let mut next_tick = Instant::now();
        loop {
            let now = Instant::now();
            if now >= next_tick {
                self.tick();
                next_tick = now + ACTOR_TICK;
            }
            let timeout = next_tick.saturating_duration_since(Instant::now());
            match rx.recv_timeout(timeout) {
                Ok(message) => {
                    if matches!(message.request, ActorRequest::Shutdown) {
                        let result = self
                            .shutdown()
                            .map(|()| ActorResponse::Snapshot(self.current_snapshot()));
                        let _response_sent = message.response.send(result);
                        break;
                    }
                    self.handle_message(message);
                }
                Err(std_mpsc::RecvTimeoutError::Timeout) => {}
                Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let _shutdown = self.shutdown();
    }

    fn shutdown(&mut self) -> Result<(), String> {
        self.sync_controller_state();
        self.persistence.flush_samples()?;
        self.persistence.flush_cycle_samples()?;
        self.persistence.save_metadata(&self.snapshot)
    }

    fn handle_message(&mut self, message: ActorMessage) {
        let result = match message.request {
            ActorRequest::Snapshot => Ok(ActorResponse::Snapshot(self.current_snapshot())),
            ActorRequest::Subscribe => {
                self.sync_controller_state();
                Ok(ActorResponse::Subscription(
                    self.snapshot_for_clients(),
                    self.persistence.saved_recipes(),
                    self.snapshot_tx.subscribe(),
                ))
            }
            ActorRequest::History => Ok(ActorResponse::History(presentation_history(
                &self.snapshot.history,
                SNAPSHOT_SAMPLE_LIMIT,
            ))),
            ActorRequest::HistoryCsv => self.persistence.live_export().map(ActorResponse::Export),
            ActorRequest::CycleHistoryCsv => self
                .persistence
                .cycle_export(None)
                .map(ActorResponse::Export),
            ActorRequest::CycleCsv(id) => Ok(ActorResponse::ArchivedExport(
                if self.persistence.cycle_path(&id).is_file() {
                    self.persistence.cycle_export(Some(&id)).map(Some)
                } else {
                    Ok(None)
                },
            )),
            ActorRequest::RunHistory(id) => {
                Ok(ActorResponse::RunHistory(self.persistence.run_history(&id)))
            }
            ActorRequest::Cycles => Ok(ActorResponse::Cycles(self.cycle_summaries())),
            ActorRequest::CycleHistory(id) => {
                Ok(ActorResponse::CycleHistory(self.cycle_history(&id)))
            }
            ActorRequest::Runs => Ok(ActorResponse::Runs(self.persistence.run_summaries())),
            ActorRequest::RunCsv(id) => Ok(ActorResponse::ArchivedExport(
                if self.persistence.runs.iter().any(|run| run.id == id) {
                    self.persistence.run_export(&id).map(Some)
                } else {
                    Ok(None)
                },
            )),
            ActorRequest::Recipes => Ok(ActorResponse::Recipes(self.persistence.saved_recipes())),
            ActorRequest::CreateRecipe(request) => {
                Ok(ActorResponse::Recipe(self.create_saved_recipe(request)))
            }
            ActorRequest::UpdateRecipe { id, request } => Ok(ActorResponse::Recipe(
                self.update_saved_recipe(&id, request),
            )),
            ActorRequest::DeleteRecipe { id, request } => Ok(ActorResponse::Recipe(
                self.delete_saved_recipe(&id, request),
            )),
            ActorRequest::ImportRecipe(export) => {
                Ok(ActorResponse::Recipe(self.import_saved_recipe(export)))
            }
            ActorRequest::ExportRecipe(id) => {
                Ok(ActorResponse::RecipeExport(self.export_saved_recipe(&id)))
            }
            ActorRequest::StartSavedRecipe { id, request } => {
                let result = self.start_saved_recipe(&id, request);
                Ok(ActorResponse::Start(self.finish_start_response(result)))
            }
            ActorRequest::Command(command) => self
                .handle_command(command)
                .map(|()| ActorResponse::Snapshot(self.current_snapshot())),
            ActorRequest::StartTest(request) => {
                let result = self.start_test(request.config, request.name, None);
                Ok(ActorResponse::Start(self.finish_start_response(result)))
            }
            ActorRequest::StartCycle(request) => {
                let result = self.start_cycle(request);
                Ok(ActorResponse::Start(self.finish_start_response(result)))
            }
            ActorRequest::RenameRun { id, request } => Ok(ActorResponse::Rename(
                self.rename_run(&id, request)
                    .map(|()| self.current_snapshot()),
            )),
            ActorRequest::RenameCycle {
                execution_id,
                request,
            } => Ok(ActorResponse::Rename(
                self.rename_cycle(&execution_id, request)
                    .map(|()| self.current_snapshot()),
            )),
            ActorRequest::StopCycle => self
                .stop_cycle()
                .and_then(|()| self.persist_and_publish())
                .map(|()| ActorResponse::Snapshot(self.current_snapshot())),
            ActorRequest::Shutdown => Err("shutdown must be handled by the actor loop".to_owned()),
        };
        let _response_sent = message.response.send(result);
    }

    fn handle_command(&mut self, command: ApiCommand) -> Result<(), String> {
        if command == ApiCommand::Stop
            && (self.cycle.owns_orchestration()
                || self.cycle.status().state == CycleState::Interrupted)
        {
            self.stop_cycle()?;
            self.persist_and_publish()?;
            return Ok(());
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
            return Err("the active cycle owns test orchestration".to_owned());
        }
        match command {
            ApiCommand::Connect => {
                if let Err(error) = self.connect() {
                    self.set_connection_error(&error);
                    return Err(error);
                }
            }
            ApiCommand::Disconnect => {
                self.cycle
                    .interrupt("cycle interrupted by explicit device disconnect");
                self.disconnect()?;
            }
            ApiCommand::Start(config) => self
                .start_test(config, None, None)
                .map_err(|error| error.to_string())?,
            ApiCommand::Adjust(config) => self.adjust_test(config)?,
            ApiCommand::Stop => self.stop_test()?,
            ApiCommand::Resume => self.resume_test()?,
            ApiCommand::Calibration(command) => self.calibrate(command)?,
        }
        self.persist_and_publish()?;
        Ok(())
    }

    fn current_snapshot(&mut self) -> AuthoritativeSnapshot {
        self.sync_controller_state();
        self.snapshot_for_clients()
    }

    fn connect(&mut self) -> Result<(), String> {
        self.cycle
            .interrupt_for_gap("cycle interrupted by device connection change");
        self.controller
            .begin_connection("device connection changed; physical state is unknown");
        self.snapshot.connection = ServerConnectionState::Connecting;
        self.snapshot.connection_error = None;
        if self.config.mock {
            self.snapshot.connection = ServerConnectionState::Connected;
            self.controller.connection_established();
            self.controller.set_device_identity(
                Some("EBC-MOCK".to_owned()),
                Some("0.0.1".to_owned()),
                Some(4200),
            );
            self.sync_controller_state();
            self.mock_idle_report_due = Some(Instant::now() + ACTOR_TICK);
            return Ok(());
        }
        let mut port = serialport::new(&self.config.serial_port, 9600)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::Odd)
            .stop_bits(serialport::StopBits::One)
            .timeout(SERIAL_TIMEOUT)
            .open()
            .map_err(|error| format!("failed to open {}: {error}", self.config.serial_port))?;
        write_frame(&mut port, OutboundFrame::Connect(0))?;
        self.port = Some(port);
        self.serial_buffer.clear();
        self.snapshot.connection = ServerConnectionState::Connected;
        self.controller.connection_established();
        self.sync_controller_state();
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), String> {
        if self.controller.requires_stop_before_disconnect() {
            self.stop_test()?;
        }
        self.send_frame_with_recovery(OutboundFrame::Disconnect, None)?;
        self.port = None;
        self.serial_buffer.clear();
        self.mock_idle_report_due = None;
        self.snapshot.connection = ServerConnectionState::Disconnected;
        self.controller
            .disconnect("device disconnected; physical test state is unknown");
        self.sync_controller_state();
        Ok(())
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "start preparation takes ownership of application metadata"
    )]
    fn start_test(
        &mut self,
        config: TestConfiguration,
        name: Option<String>,
        cycle: Option<CycleRunContext>,
    ) -> Result<(), StartError> {
        let name = normalize_optional_name(name.as_deref())
            .map_err(|error| StartError::BadRequest(error.to_string()))?;
        let name = if cycle.is_some() { None } else { name };
        let prepared = self
            .controller
            .prepare_command(ApiCommand::Start(config))
            .map_err(StartError::BadRequest)?;
        self.sync_controller_state();
        self.persistence
            .archive_current(&self.snapshot)
            .map_err(StartError::Internal)?;
        let rollback = StartPreparationRollback {
            history: self.snapshot.history.clone(),
            archived_run_id: self.persistence.archived_run_id.clone(),
            current_run_id: self.persistence.current_run_id.clone(),
            current_run_name: self.persistence.current_run_name.clone(),
            current_run_cycle: self.persistence.current_run_cycle.clone(),
            next_sequence: self.persistence.next_sequence,
            raw_sample_count: self.persistence.raw_sample_count,
        };
        if let Err(error) = self.persistence.reset_samples() {
            return Err(StartError::Internal(
                self.restore_start_preparation(rollback, &error),
            ));
        }
        self.snapshot.history.clear();
        let run_id = self.persistence.new_run_id();
        self.persistence.begin_current_run(run_id, name, cycle);
        self.sync_controller_state();
        if let Err(error) = self.save_start_metadata() {
            return Err(StartError::Internal(
                self.restore_start_preparation(rollback, &error),
            ));
        }
        self.send_command_frame(prepared)
            .map_err(StartError::Internal)?;
        self.controller
            .commit_command(prepared, Some(Utc::now().to_rfc3339()));
        self.sync_controller_state();
        if self.config.mock {
            self.controller.set_current_ma(mock_current(&config));
            self.sync_controller_state();
        }
        Ok(())
    }

    fn get_saved_recipe(&self, id: &str) -> Result<SavedRecipe, RecipeError> {
        if !valid_run_id(id) {
            return Err(RecipeError::BadRequest(
                "invalid saved recipe id".to_owned(),
            ));
        }
        self.persistence
            .saved_recipe(id)
            .cloned()
            .ok_or_else(|| RecipeError::NotFound("saved recipe not found".to_owned()))
    }

    fn create_saved_recipe(
        &mut self,
        request: CreateSavedRecipeRequest,
    ) -> Result<SavedRecipe, RecipeError> {
        let name = normalize_required_name(&request.name)
            .map_err(|error| RecipeError::BadRequest(error.to_string()))?;
        request
            .recipe
            .validate()
            .map_err(|error| RecipeError::BadRequest(error.to_string()))?;
        let recipe = self
            .persistence
            .create_saved_recipe(name, request.recipe)
            .map_err(RecipeError::Internal)?;
        let _receivers = self
            .snapshot_tx
            .send(WebSocketEvent::RecipeUpsert(recipe.clone()));
        Ok(recipe)
    }

    fn update_saved_recipe(
        &mut self,
        id: &str,
        mut request: UpdateSavedRecipeRequest,
    ) -> Result<SavedRecipe, RecipeError> {
        if !valid_run_id(id) {
            return Err(RecipeError::BadRequest(
                "invalid saved recipe id".to_owned(),
            ));
        }
        request.name = normalize_required_name(&request.name)
            .map_err(|error| RecipeError::BadRequest(error.to_string()))?;
        request
            .recipe
            .validate()
            .map_err(|error| RecipeError::BadRequest(error.to_string()))?;
        let recipe = self.persistence.update_saved_recipe(id, request)?;
        let _receivers = self
            .snapshot_tx
            .send(WebSocketEvent::RecipeUpsert(recipe.clone()));
        Ok(recipe)
    }

    fn delete_saved_recipe(
        &mut self,
        id: &str,
        request: DeleteSavedRecipeRequest,
    ) -> Result<SavedRecipe, RecipeError> {
        if !valid_run_id(id) {
            return Err(RecipeError::BadRequest(
                "invalid saved recipe id".to_owned(),
            ));
        }
        let recipe = self
            .persistence
            .delete_saved_recipe(id, request.expected_revision)?;
        let _receivers = self
            .snapshot_tx
            .send(WebSocketEvent::RecipeDelete(id.to_owned()));
        Ok(recipe)
    }

    fn import_saved_recipe(&mut self, export: RecipeExport) -> Result<SavedRecipe, RecipeError> {
        export
            .validate()
            .map_err(|error| RecipeError::BadRequest(error.to_string()))?;
        self.create_saved_recipe(CreateSavedRecipeRequest {
            name: export.name,
            recipe: export.recipe,
        })
    }

    fn export_saved_recipe(&self, id: &str) -> Result<RecipeExport, RecipeError> {
        let saved = self.get_saved_recipe(id)?;
        Ok(RecipeExport {
            format: RECIPE_EXPORT_FORMAT.to_owned(),
            version: RECIPE_EXPORT_VERSION,
            name: saved.name,
            recipe: saved.recipe,
        })
    }

    fn start_saved_recipe(
        &mut self,
        id: &str,
        request: StartSavedRecipeRequest,
    ) -> Result<(), StartError> {
        let saved = self.get_saved_recipe(id).map_err(|error| match error {
            RecipeError::BadRequest(message) => StartError::BadRequest(message),
            RecipeError::NotFound(message) => StartError::NotFound(message),
            RecipeError::Conflict(message) | RecipeError::Internal(message) => {
                StartError::Internal(message)
            }
        })?;
        let saved_reference = SavedRecipeReference {
            id: saved.id,
            name: saved.name,
            revision: saved.revision,
        };
        self.start_cycle_with_saved(
            StartCycleRequest {
                recipe: saved.recipe,
                name: request.execution_name,
            },
            Some(saved_reference),
        )
    }

    fn start_cycle(&mut self, request: StartCycleRequest) -> Result<(), StartError> {
        self.start_cycle_with_saved(request, None)
    }

    fn start_cycle_with_saved(
        &mut self,
        request: StartCycleRequest,
        saved_recipe: Option<SavedRecipeReference>,
    ) -> Result<(), StartError> {
        request
            .recipe
            .validate()
            .map_err(|error| StartError::BadRequest(error.to_string()))?;
        let name = normalize_optional_name(request.name.as_deref())
            .map_err(|error| StartError::BadRequest(error.to_string()))?;
        if self.cycle.is_executing() {
            return Err(StartError::BadRequest(
                "a cycle is already active".to_owned(),
            ));
        }
        if !self.controller.capabilities().start {
            return Err(StartError::BadRequest(
                "cycle start requires a fresh current-connection inactive report".to_owned(),
            ));
        }
        if self.controller.device().current_ma != Some(0) {
            return Err(StartError::BadRequest(
                "cycle start requires confirmed zero device current".to_owned(),
            ));
        }
        let previous_cycle = self.cycle.clone();
        let previous_status = self.snapshot.cycle.clone();
        let previous_history = self.snapshot.cycle_history.clone();
        let previous_cycle_id = self.persistence.current_cycle_id.clone();
        let previous_cycle_name = self.persistence.current_cycle_name.clone();
        let previous_sequence = self.persistence.next_cycle_sequence;
        let previous_sample_count = self.persistence.raw_cycle_sample_count;
        let started_at = Utc::now().to_rfc3339();
        let execution_id = format!("cycle-{}", started_at.replace([':', '.', '+'], "-"));
        self.persistence
            .begin_cycle(
                execution_id.clone(),
                name.clone(),
                request.recipe.clone(),
                saved_recipe.clone(),
                started_at.clone(),
            )
            .map_err(StartError::Internal)?;
        let action = self
            .cycle
            .start(
                request.recipe,
                execution_id.clone(),
                name,
                saved_recipe,
                Some(started_at),
                Instant::now(),
            )
            .map_err(|error| StartError::BadRequest(error.to_string()))?;
        self.snapshot.cycle_history.clear();
        self.sync_controller_state();
        if let Err(error) = self.save_start_metadata() {
            self.cycle = previous_cycle;
            self.snapshot.cycle = previous_status;
            self.snapshot.cycle_history = previous_history;
            self.persistence.current_cycle_id = previous_cycle_id;
            self.persistence.current_cycle_name = previous_cycle_name;
            self.persistence.next_cycle_sequence = previous_sequence;
            self.persistence.raw_cycle_sample_count = previous_sample_count;
            self.sync_controller_state();
            let csv_cleanup = fs::remove_file(self.persistence.cycle_path(&execution_id));
            let sidecar_cleanup =
                fs::remove_file(self.persistence.cycle_metadata_path(&execution_id));
            let sync_cleanup = sync_directory(&self.persistence.cycles_dir);
            let cleanup_errors = [csv_cleanup, sidecar_cleanup]
                .into_iter()
                .filter_map(Result::err)
                .filter(|cleanup_error| cleanup_error.kind() != std::io::ErrorKind::NotFound)
                .map(|cleanup_error| cleanup_error.to_string())
                .chain(sync_cleanup.err())
                .collect::<Vec<_>>();
            return Err(StartError::Internal(if cleanup_errors.is_empty() {
                error
            } else {
                format!(
                    "{error}; failed to roll back cycle start: {}",
                    cleanup_errors.join("; ")
                )
            }));
        }
        if let Some(action) = action {
            self.execute_cycle_action(action)
                .map_err(StartError::Internal)?;
        }
        Ok(())
    }

    fn finish_start_response(
        &mut self,
        result: Result<(), StartError>,
    ) -> Result<AuthoritativeSnapshot, StartError> {
        result?;
        if let Err(error) = self.persist_and_publish() {
            log::error!(
                "failed to persist committed start state; initial recovery metadata remains durable: {error}"
            );
            self.publish();
        }
        Ok(self.current_snapshot())
    }

    fn stop_cycle(&mut self) -> Result<(), String> {
        if let Some(action) = self.cycle.stop(self.controller.test()) {
            self.execute_cycle_action(action)?;
        }
        self.sync_controller_state();
        Ok(())
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "actor requests transfer rename ownership"
    )]
    fn rename_run(&mut self, id: &str, request: RenameRequest) -> Result<(), RenameError> {
        if !valid_run_id(id) {
            return Err(RenameError::BadRequest("invalid run id".to_owned()));
        }
        let name = normalize_optional_name(request.name.as_deref())
            .map_err(|error| RenameError::BadRequest(error.to_string()))?;

        if self.persistence.current_run_id == id {
            if self.persistence.current_run_cycle.is_some() {
                return Err(RenameError::BadRequest(
                    "cycle child runs cannot be named independently; rename the cycle instead"
                        .to_owned(),
                ));
            }
            let archived = self
                .persistence
                .runs
                .iter()
                .position(|summary| summary.id == id);
            let previous_summary = archived.map(|index| self.persistence.runs[index].clone());
            if let Some(previous) = &previous_summary {
                if previous.cycle.is_some() {
                    return Err(RenameError::BadRequest(
                        "cycle child runs cannot be named independently; rename the cycle instead"
                            .to_owned(),
                    ));
                }
                let mut renamed = previous.clone();
                renamed.name.clone_from(&name);
                self.persistence
                    .rewrite_run_summary(&renamed)
                    .map_err(RenameError::Internal)?;
            }

            let previous_name = self.persistence.current_run_name.clone();
            self.persistence.current_run_name.clone_from(&name);
            self.persistence.sync_snapshot_metadata(&mut self.snapshot);
            if let Err(error) = self.persistence.save_metadata(&self.snapshot) {
                self.persistence.current_run_name = previous_name;
                self.persistence.sync_snapshot_metadata(&mut self.snapshot);
                if let Some(previous) = previous_summary
                    && let Err(rollback_error) = self.persistence.rewrite_run_summary(&previous)
                {
                    return Err(RenameError::Internal(format!(
                        "{error}; failed to restore archived run name: {rollback_error}"
                    )));
                }
                return Err(RenameError::Internal(error));
            }
            if let Some(index) = archived {
                self.persistence.runs[index].name = name;
            }
            self.publish();
            return Ok(());
        }

        let Some(index) = self
            .persistence
            .runs
            .iter()
            .position(|summary| summary.id == id)
        else {
            return Err(RenameError::NotFound("run not found".to_owned()));
        };
        if self.persistence.runs[index].cycle.is_some() {
            return Err(RenameError::BadRequest(
                "cycle child runs cannot be named independently; rename the cycle instead"
                    .to_owned(),
            ));
        }
        let mut renamed = self.persistence.runs[index].clone();
        renamed.name = name;
        self.persistence
            .rewrite_run_summary(&renamed)
            .map_err(RenameError::Internal)?;
        self.persistence.runs[index] = renamed;
        Ok(())
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "actor requests transfer rename ownership"
    )]
    fn rename_cycle(
        &mut self,
        execution_id: &str,
        request: RenameRequest,
    ) -> Result<(), RenameError> {
        if !valid_run_id(execution_id) {
            return Err(RenameError::BadRequest(
                "invalid cycle execution id".to_owned(),
            ));
        }
        let name = normalize_optional_name(request.name.as_deref())
            .map_err(|error| RenameError::BadRequest(error.to_string()))?;
        if !self.persistence.cycle_path(execution_id).is_file() {
            return Err(RenameError::NotFound(
                "cycle execution not found".to_owned(),
            ));
        }

        let previous = self
            .persistence
            .load_cycle_metadata(execution_id)
            .map_err(RenameError::Internal)?;
        let mut renamed = previous.clone().unwrap_or_else(|| CycleExecutionMetadata {
            execution_id: execution_id.to_owned(),
            ..CycleExecutionMetadata::default()
        });
        renamed.name.clone_from(&name);
        self.persistence
            .write_cycle_metadata(&renamed)
            .map_err(RenameError::Internal)?;

        if self.snapshot.cycle.execution_id.as_deref() == Some(execution_id) {
            let previous_name = self.persistence.current_cycle_name.clone();
            self.persistence.current_cycle_id = Some(execution_id.to_owned());
            self.persistence.current_cycle_name = name;
            self.persistence.sync_snapshot_metadata(&mut self.snapshot);
            if let Err(error) = self.persistence.save_metadata(&self.snapshot) {
                self.persistence.current_cycle_name = previous_name;
                self.persistence.sync_snapshot_metadata(&mut self.snapshot);
                let rollback = match previous {
                    Some(metadata) => self.persistence.write_cycle_metadata(&metadata),
                    None => fs::remove_file(self.persistence.cycle_metadata_path(execution_id))
                        .map_err(|rollback_error| rollback_error.to_string())
                        .and_then(|()| sync_directory(&self.persistence.cycles_dir)),
                };
                return Err(RenameError::Internal(match rollback {
                    Ok(()) => error,
                    Err(rollback_error) => {
                        format!("{error}; failed to restore cycle name: {rollback_error}")
                    }
                }));
            }
            self.publish();
        }
        Ok(())
    }

    fn execute_cycle_action(&mut self, action: CycleAction) -> Result<(), String> {
        let result = match action {
            CycleAction::Start(config) => {
                let context = self
                    .current_cycle_context()
                    .ok_or_else(|| "cycle device step is missing execution context".to_owned())?;
                self.start_test(config, None, Some(context))
                    .map_err(|error| error.to_string())
            }
            CycleAction::Stop => self.stop_test(),
        };
        if let Err(error) = result {
            self.cycle
                .on_action_failed(format!("cycle physical action failed: {error}"));
            self.sync_controller_state();
            if let Err(persistence_error) = self.persistence.flush_cycle_samples() {
                log::error!("failed to flush interrupted cycle telemetry: {persistence_error}");
            }
            if let Err(persistence_error) = self.persistence.save_metadata(&self.snapshot) {
                log::error!("failed to persist interrupted cycle action: {persistence_error}");
            }
            self.publish();
            return Err(error);
        }
        self.cycle
            .on_action_committed(&action, self.controller.test());
        self.sync_controller_state();
        Ok(())
    }

    fn current_cycle_context(&self) -> Option<CycleRunContext> {
        Some(CycleRunContext {
            execution_id: self.cycle.status().execution_id.clone()?,
            repeat_index: self.cycle.status().repeat_index,
            step_index: self.cycle.status().step_index,
        })
    }

    fn save_start_metadata(&self) -> Result<(), String> {
        #[cfg(test)]
        if let Some(error) = &self.start_metadata_failure {
            return Err(error.clone());
        }
        self.persistence.save_metadata(&self.snapshot)
    }

    fn restore_start_preparation(
        &mut self,
        rollback: StartPreparationRollback,
        cause: &str,
    ) -> String {
        self.snapshot.history = rollback.history;
        self.persistence.archived_run_id = rollback.archived_run_id.clone();
        self.persistence.current_run_id = rollback.current_run_id;
        self.persistence.current_run_name = rollback.current_run_name;
        self.persistence.current_run_cycle = rollback.current_run_cycle;
        self.persistence.next_sequence = rollback.next_sequence;
        self.persistence.raw_sample_count = rollback.raw_sample_count;

        let mut rollback_errors = Vec::new();
        if let Some(archived_run_id) = rollback.archived_run_id
            && let Err(error) = self
                .persistence
                .restore_current_samples(&archived_run_id, rollback.raw_sample_count)
        {
            rollback_errors.push(format!("failed to restore current samples: {error}"));
        }
        if let Err(error) = self.persistence.save_metadata(&self.snapshot) {
            rollback_errors.push(format!("failed to restore current metadata: {error}"));
        }
        if rollback_errors.is_empty() {
            cause.to_owned()
        } else {
            format!("{cause}; {}", rollback_errors.join("; "))
        }
    }

    fn stop_test(&mut self) -> Result<(), String> {
        let prepared = self.controller.prepare_command(ApiCommand::Stop)?;
        self.send_command_frame(prepared)?;
        self.controller.commit_command(prepared, None);
        self.sync_controller_state();
        self.persistence.flush_samples()?;
        Ok(())
    }

    fn adjust_test(&mut self, config: TestConfiguration) -> Result<(), String> {
        let prepared = self
            .controller
            .prepare_command(ApiCommand::Adjust(config))?;
        let frame = prepared
            .frame()
            .ok_or_else(|| "adjustment did not produce a protocol frame".to_owned())?;
        self.send_frame_with_recovery(frame, Some(prepared.kind()))?;
        self.controller.commit_command(prepared, None);
        if self.config.mock {
            let TestConfiguration::DischargeConstantCurrent { current_ma, .. } = config else {
                unreachable!();
            };
            self.controller.set_current_ma(current_ma);
        }
        self.sync_controller_state();
        Ok(())
    }

    fn resume_test(&mut self) -> Result<(), String> {
        let prepared = self.controller.prepare_command(ApiCommand::Resume)?;
        let frame = prepared
            .frame()
            .ok_or_else(|| "resume did not produce a protocol frame".to_owned())?;
        self.send_frame_with_recovery(frame, Some(prepared.kind()))?;
        self.controller.commit_command(prepared, None);
        self.sync_controller_state();
        Ok(())
    }

    fn calibrate(&mut self, command: CalibrationCommand) -> Result<(), String> {
        let prepared = self
            .controller
            .prepare_command(ApiCommand::Calibration(command))?;
        let frame = prepared
            .frame()
            .ok_or_else(|| "calibration did not produce a protocol frame".to_owned())?;
        self.send_frame_with_recovery(frame, Some(prepared.kind()))?;
        self.controller.commit_command(prepared, None);
        self.sync_controller_state();
        Ok(())
    }

    fn send(&mut self, frame: OutboundFrame) -> Result<(), String> {
        #[cfg(test)]
        self.sent_frames.push(frame);
        #[cfg(test)]
        if let Some((successful_writes, error)) = self.write_failure_after.take() {
            if successful_writes == 0 {
                return Err(error);
            }
            self.write_failure_after = Some((successful_writes - 1, error));
        }
        #[cfg(test)]
        if let Some(error) = self.write_failure.take() {
            return Err(error);
        }
        if self.config.mock {
            return Ok(());
        }
        let port = self
            .port
            .as_mut()
            .ok_or_else(|| "serial port is not open".to_owned())?;
        write_frame(port, frame)
    }

    fn send_command_frame(&mut self, prepared: PreparedCommand) -> Result<(), String> {
        let Some(frame) = prepared.frame() else {
            return Ok(());
        };
        self.send_frame_with_recovery(frame, Some(prepared.kind()))
    }

    fn send_frame_with_recovery(
        &mut self,
        frame: OutboundFrame,
        command: Option<CommandKind>,
    ) -> Result<(), String> {
        if let Err(error) = self.send(frame) {
            self.handle_protocol_write_failure(command, &error);
            return Err(error);
        }
        Ok(())
    }

    fn handle_protocol_write_failure(&mut self, command: Option<CommandKind>, error: &str) {
        self.cycle.interrupt(format!(
            "cycle interrupted by protocol write failure: {error}"
        ));
        self.port = None;
        self.serial_buffer.clear();
        self.mock_idle_report_due = None;
        self.snapshot.connection = ServerConnectionState::Error;
        self.snapshot.connection_error = Some(error.to_owned());
        let reason = format!("protocol write failed; physical state is unknown: {error}");
        if let Some(kind) = command {
            self.controller.command_write_failed(kind, &reason);
        } else {
            self.controller.disconnect(&reason);
        }
        self.sync_controller_state();
        if let Err(persistence_error) = self.persistence.flush_cycle_samples() {
            log::error!("failed to flush interrupted cycle telemetry: {persistence_error}");
        }
        if let Err(persistence_error) = self.persistence.save_metadata(&self.snapshot) {
            log::error!("failed to persist protocol write failure: {persistence_error}");
        }
        self.publish();
    }

    fn tick(&mut self) {
        let previous_cycle = self.cycle.status().clone();
        if self.config.mock {
            self.tick_mock();
        } else {
            self.read_serial();
        }
        if let Some(minutes) = self.controller.next_timer_sync()
            && let Err(error) =
                self.send_frame_with_recovery(OutboundFrame::TimerSync(minutes), None)
        {
            log::error!("timer sync write failed: {error}");
        }
        self.sync_controller_state();
        if let Some(action) = self.cycle.tick(Instant::now())
            && let Err(error) = self.execute_cycle_action(action)
        {
            log::error!("cycle action failed during tick: {error}");
        }
        if self.cycle.status() != &previous_cycle {
            if let Err(error) = self.archive_final_cycle_run(&previous_cycle) {
                log::error!("failed to archive final cycle run: {error}");
            }
            if let Err(error) = self.persist_and_publish() {
                log::error!("failed to persist cycle tick: {error}");
            }
        }
    }

    fn tick_mock(&mut self) {
        if let Some(due) = self.mock_idle_report_due {
            if Instant::now() < due {
                return;
            }
            self.mock_idle_report_due = None;
            let mode = self
                .snapshot
                .device
                .mode
                .unwrap_or(device::DeviceMode::DischargeConstantCurrent);
            self.record_report_with_source(
                mode,
                4200,
                0,
                0,
                ReportState::Idle,
                true,
                "EBC-MOCK",
                None,
            );
            return;
        }
        if self.controller.is_stopping() {
            let mode = self
                .snapshot
                .device
                .mode
                .unwrap_or(device::DeviceMode::DischargeConstantCurrent);
            self.record_report_with_source(
                mode,
                self.controller.device().voltage_mv.unwrap_or(4200),
                0,
                self.controller.device().capacity_mah.unwrap_or(0),
                ReportState::Idle,
                true,
                "EBC-MOCK",
                None,
            );
            return;
        }
        if !(self.controller.is_starting() || self.controller.is_running_owned())
            || self.last_mock_sample.elapsed() < Duration::from_secs(1)
        {
            return;
        }
        self.last_mock_sample = Instant::now();
        self.mock_sample_number += 1;
        let Some(config) = self.controller.test().config else {
            return;
        };
        let voltage =
            4200_u16.saturating_sub(u16::try_from(self.mock_sample_number / 5).unwrap_or(u16::MAX));
        let capacity = u16::try_from(self.mock_sample_number / 3).unwrap_or(u16::MAX);
        self.record_report_with_source(
            config.mode(),
            voltage,
            mock_current(&config),
            capacity,
            ReportState::Active,
            true,
            "EBC-MOCK",
            None,
        );
    }

    fn read_serial(&mut self) {
        let Some(port) = &mut self.port else { return };
        let mut bytes = [0_u8; 64];
        match port.read(&mut bytes) {
            Ok(count) if count > 0 => {
                self.serial_buffer.extend_from_slice(&bytes[..count]);
                for (frame, _) in device::process_buffer(&mut self.serial_buffer) {
                    self.handle_frame(frame);
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => self.set_connection_error(&format!("serial read failed: {error}")),
        }
    }

    fn handle_frame(&mut self, frame: InboundFrame) {
        match frame {
            InboundFrame::Firmware(report) => self.record_report_with_source(
                report.device_mode,
                report.voltage_mv,
                report.current_ma,
                report.milli_ampere_hours,
                if report.in_progress {
                    ReportState::Active
                } else {
                    ReportState::InactiveUnknown
                },
                false,
                &report.device_type,
                Some(report.firmware_version),
            ),
            InboundFrame::Charge(report) => self.record_report_with_source(
                device::DeviceMode::ChargeConstantVoltage,
                report.voltage_mv,
                report.current_ma,
                report.milli_ampere_hours,
                report.state.into(),
                true,
                &report.device_type,
                None,
            ),
            InboundFrame::DischargeConstantCurrent(report) => self.record_report_with_source(
                device::DeviceMode::DischargeConstantCurrent,
                report.voltage_mv,
                report.current_ma,
                report.milli_ampere_hours,
                report.state.into(),
                true,
                &report.device_type,
                None,
            ),
            InboundFrame::DischargeConstantPower(report) => self.record_report_with_source(
                device::DeviceMode::DischargeConstantPower,
                report.voltage_mv,
                report.current_ma,
                report.milli_ampere_hours,
                report.state.into(),
                true,
                &report.device_type,
                None,
            ),
        }
    }

    #[cfg(test)]
    #[expect(clippy::too_many_arguments)]
    fn record_report(
        &mut self,
        mode: device::DeviceMode,
        voltage_mv: u16,
        current_ma: u16,
        capacity_mah: u16,
        report_state: ReportState,
        model: &str,
        firmware: Option<String>,
    ) {
        self.record_report_with_source(
            mode,
            voltage_mv,
            current_ma,
            capacity_mah,
            report_state,
            true,
            model,
            firmware,
        );
    }

    #[expect(clippy::too_many_arguments)]
    fn record_report_with_source(
        &mut self,
        mode: device::DeviceMode,
        voltage_mv: u16,
        current_ma: u16,
        capacity_mah: u16,
        report_state: ReportState,
        normal_report: bool,
        model: &str,
        firmware: Option<String>,
    ) {
        let active = report_state == ReportState::Active;
        let now = Instant::now();
        let timestamp_utc = Utc::now().to_rfc3339();
        let report = DeviceReport {
            mode,
            state: report_state,
            voltage_mv,
            current_ma,
            capacity_mah,
            model: model.to_owned(),
            firmware_version: firmware,
        };
        let (outcome, measurement) = self.controller.report(report.clone());
        self.sync_controller_state();
        if normal_report && self.cycle.is_executing() {
            self.record_cycle_sample(&report, &timestamp_utc, now);
        }
        if let Some(measurement) = measurement {
            let sample = Sample {
                run_id: self.persistence.current_run_id.clone(),
                sequence: self.persistence.next_sequence,
                timestamp_utc,
                elapsed_seconds: measurement.elapsed_seconds,
                voltage_mv: measurement.voltage_mv,
                current_ma: measurement.current_ma,
                capacity_mah: measurement.capacity_mah,
                energy_wh: measurement.energy_wh,
                mode: measurement.mode,
            };
            if let Err(error) = self.persistence.append_sample(&sample) {
                log::error!("failed to append sample: {error}");
            } else {
                self.snapshot.history.push(sample.clone());
                if self.snapshot.history.len() > SNAPSHOT_SAMPLE_LIMIT {
                    self.snapshot.history =
                        presentation_history(&self.snapshot.history, SNAPSHOT_SAMPLE_LIMIT);
                }
                let _receivers = self.snapshot_tx.send(WebSocketEvent::Sample(sample));
            }
        }
        if outcome.transitioned_to_inactive
            && let Err(error) = self.persistence.flush_samples()
        {
            log::error!("failed to flush inactive samples: {error}");
        }
        if self.persistence.current_run_cycle.is_none()
            && !self.persistence.current_run_id.is_empty()
            && matches!(
                self.snapshot.test.state,
                TestState::Completed | TestState::Stopped
            )
            && let Err(error) = self.persistence.archive_current(&self.snapshot)
        {
            log::error!("failed to archive terminal manual run: {error}");
        }
        let persistence_result = if !active {
            self.persist_and_publish()
        } else {
            self.persist_report_and_publish()
        };
        if let Err(error) = persistence_result {
            log::error!("failed to persist device report: {error}");
        }
        let previous_cycle = self.cycle.status().clone();
        let action = self.cycle.on_physical_state(
            now,
            self.controller.device(),
            self.controller.test(),
            true,
        );
        if let Some(action) = action
            && let Err(error) = self.execute_cycle_action(action)
        {
            log::error!("cycle action failed after device report: {error}");
        }
        if self.cycle.status() != &previous_cycle {
            if let Err(error) = self.archive_final_cycle_run(&previous_cycle) {
                log::error!("failed to archive final cycle run: {error}");
            }
            if let Err(error) = self.persist_and_publish() {
                log::error!("failed to persist cycle report transition: {error}");
            }
        }
    }

    fn record_cycle_sample(&mut self, report: &DeviceReport, timestamp_utc: &str, now: Instant) {
        let cycle_status = self.cycle.status();
        let Some(execution_id) = cycle_status.execution_id.clone() else {
            log::error!("executing cycle is missing its execution id");
            return;
        };
        let sample = CycleSample {
            execution_id,
            sequence: self.persistence.next_cycle_sequence,
            timestamp_utc: timestamp_utc.to_owned(),
            elapsed_milliseconds: u64::try_from(self.cycle.elapsed(now).as_millis())
                .unwrap_or(u64::MAX),
            repeat_index: cycle_status.repeat_index,
            step_index: cycle_status.step_index,
            cycle_state: cycle_status.state,
            test_state: self.controller.test().state.clone(),
            mode: report.mode,
            activity_known: self.controller.device().activity_known,
            active: self.controller.device().active,
            voltage_mv: report.voltage_mv,
            current_ma: report.current_ma,
            device_capacity_mah: report.capacity_mah,
            test_capacity_mah: self.controller.test().capacity_mah,
            test_energy_wh: self.controller.test().energy_wh,
        };
        if let Err(error) = self.persistence.append_cycle_sample(&sample) {
            log::error!("failed to append cycle sample: {error}");
            return;
        }
        self.snapshot.cycle_history.push(sample.clone());
        if self.snapshot.cycle_history.len() > SNAPSHOT_SAMPLE_LIMIT {
            self.snapshot.cycle_history =
                cycle_presentation_history(&self.snapshot.cycle_history, SNAPSHOT_SAMPLE_LIMIT);
        }
        let _receivers = self.snapshot_tx.send(WebSocketEvent::CycleSample(sample));
    }

    fn archive_final_cycle_run(&mut self, previous: &CycleStatus) -> Result<(), String> {
        if previous.state == CycleState::Completed
            || self.cycle.status().state != CycleState::Completed
        {
            return Ok(());
        }
        let current_execution = self
            .persistence
            .current_run_cycle
            .as_ref()
            .map(|context| context.execution_id.as_str());
        if current_execution == self.cycle.status().execution_id.as_deref() {
            self.sync_controller_state();
            self.persistence.archive_current(&self.snapshot)?;
        }
        Ok(())
    }

    fn set_connection_error(&mut self, error: &str) {
        self.cycle.interrupt_for_gap(format!(
            "cycle interrupted by device connection failure: {error}"
        ));
        self.port = None;
        self.serial_buffer.clear();
        self.mock_idle_report_due = None;
        self.snapshot.connection = ServerConnectionState::Error;
        self.snapshot.connection_error = Some(error.to_owned());
        self.controller.disconnect(&format!(
            "device connection failed; physical test state is unknown: {error}"
        ));
        self.sync_controller_state();
        if let Err(persistence_error) = self.persistence.flush_cycle_samples() {
            log::error!("failed to flush interrupted cycle telemetry: {persistence_error}");
        }
        if let Err(persistence_error) = self.persistence.save_metadata(&self.snapshot) {
            log::error!("failed to persist connection failure: {persistence_error}");
        }
        self.publish();
    }

    fn persist_and_publish(&mut self) -> Result<(), String> {
        self.sync_controller_state();
        if !self.cycle.is_executing() {
            self.persistence.flush_cycle_samples()?;
        }
        self.persistence.save_metadata(&self.snapshot)?;
        self.publish();
        Ok(())
    }

    fn persist_report_and_publish(&mut self) -> Result<(), String> {
        self.sync_controller_state();
        if self.last_metadata_sync.elapsed() >= Duration::from_secs(1) {
            self.persistence.save_metadata(&self.snapshot)?;
            self.last_metadata_sync = Instant::now();
        }
        self.publish();
        Ok(())
    }

    fn publish(&self) {
        let mut snapshot = self.snapshot.clone();
        self.persistence.sync_snapshot_metadata(&mut snapshot);
        let update = SnapshotUpdate::from(&snapshot);
        let _receivers = self.snapshot_tx.send(WebSocketEvent::Update(update));
    }

    fn sync_controller_state(&mut self) {
        if let Err(error) = self.persist_cycle_transition() {
            log::error!("failed to persist cycle history transition: {error}");
        }
        self.controller.update_elapsed();
        self.snapshot.device = self.controller.device().clone();
        self.snapshot.test = self.controller.test().clone();
        self.snapshot.cycle = self.cycle.status().clone();
        self.persistence.sync_snapshot_metadata(&mut self.snapshot);
        self.snapshot.capabilities = self.controller.capabilities();
        if self.cycle.owns_orchestration() {
            self.snapshot.capabilities.start = false;
            self.snapshot.capabilities.resume = false;
            self.snapshot.capabilities.stop = true;
            self.snapshot.capabilities.show_stop = true;
            self.snapshot.capabilities.adjust = false;
            self.snapshot.capabilities.calibrate_voltage = false;
            self.snapshot.capabilities.calibrate_current = false;
            self.snapshot.capabilities.confirm_calibration = false;
        }
    }

    fn snapshot_for_clients(&self) -> AuthoritativeSnapshot {
        let mut snapshot = self.snapshot.clone();
        self.persistence.sync_snapshot_metadata(&mut snapshot);
        snapshot.history = presentation_history(&snapshot.history, SNAPSHOT_SAMPLE_LIMIT);
        snapshot.cycle_history =
            cycle_presentation_history(&snapshot.cycle_history, SNAPSHOT_SAMPLE_LIMIT);
        snapshot
    }
}

fn mock_current(config: &TestConfiguration) -> u16 {
    match *config {
        TestConfiguration::DischargeConstantCurrent { current_ma, .. }
        | TestConfiguration::ChargeConstantVoltage { current_ma, .. } => current_ma,
        TestConfiguration::DischargeConstantPower { power_w, .. } => power_w.saturating_mul(238),
    }
}

fn write_frame(
    port: &mut Box<dyn serialport::SerialPort>,
    frame: OutboundFrame,
) -> Result<(), String> {
    let bytes: [u8; OUTBOUND_FRAME_SIZE] = frame.into();
    port.write_all(&bytes)
        .map_err(|error| format!("serial write failed: {error}"))
}

/// Runs the device actor and HTTP server until shutdown.
///
/// # Errors
/// Returns an error if persistence initialization, listener binding, or HTTP
/// serving fails.
pub async fn run(config: ServerConfig) -> Result<(), String> {
    let (actor_tx, actor_rx) = std_mpsc::channel();
    let (snapshot_tx, _) = broadcast::channel(SNAPSHOT_CHANNEL_CAPACITY);
    let actor_config = config.clone();
    let actor_snapshot_tx = snapshot_tx.clone();
    let (initialization_tx, initialization_rx) = oneshot::channel();
    let actor_thread = thread::Builder::new()
        .name("ebc-device-actor".to_owned())
        .spawn(
            move || match DeviceActor::new(actor_config, actor_snapshot_tx) {
                Ok(actor) => {
                    let _sent = initialization_tx.send(Ok(()));
                    actor.run(&actor_rx);
                }
                Err(error) => {
                    let _sent = initialization_tx.send(Err(error));
                }
            },
        )
        .map_err(|error| format!("failed to start device actor: {error}"))?;
    match initialization_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tokio::task::spawn_blocking(move || actor_thread.join())
                .await
                .map_err(|join_error| format!("failed to join device actor: {join_error}"))?
                .map_err(|_panic_payload| "device actor panicked".to_owned())?;
            return Err(error);
        }
        Err(_closed) => {
            tokio::task::spawn_blocking(move || actor_thread.join())
                .await
                .map_err(|join_error| format!("failed to join device actor: {join_error}"))?
                .map_err(|_panic_payload| "device actor panicked".to_owned())?;
            return Err("device actor stopped during initialization".to_owned());
        }
    }

    let state = AppState {
        actor_tx: actor_tx.clone(),
        allowed_origin: std::env::var("EBC_ALLOWED_ORIGIN").ok(),
    };
    let static_files = ServeDir::new(&config.static_dir)
        .not_found_service(ServeFile::new(config.static_dir.join("index.html")));
    let app = Router::new()
        .nest("/api", api_router())
        .fallback_service(static_files)
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(config.http_addr)
        .await
        .map_err(|error| format!("failed to bind {}: {error}", config.http_addr))?;
    log::info!(
        "ebc-server listening on {}; serial={}, data={}, static={}, mock={}",
        config.http_addr,
        config.serial_port,
        config.data_dir.display(),
        config.static_dir.display(),
        config.mock
    );
    let server_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| format!("HTTP server failed: {error}"));
    let (response_tx, response_rx) = oneshot::channel();
    let _sent = actor_tx.send(ActorMessage {
        request: ActorRequest::Shutdown,
        response: response_tx,
    });
    let _flushed = response_rx.await;
    tokio::task::spawn_blocking(move || actor_thread.join())
        .await
        .map_err(|error| format!("failed to join device actor: {error}"))?
        .map_err(|_panic_payload| "device actor panicked".to_owned())?;
    server_result
}

fn api_router() -> Router<AppState> {
    Router::new()
        .route("/status", get(get_status))
        .route("/history", get(get_history))
        .route("/history.csv", get(get_history_csv))
        .route("/cycle/history.csv", get(get_cycle_history_csv))
        .route("/cycles/{id}/history.csv", get(get_cycle_csv))
        .route("/runs", get(get_runs))
        .route("/runs/{id}", get(history::get_run_history))
        .route("/runs/{id}/history.csv", get(get_run_csv))
        .route("/cycles", get(history::get_cycles))
        .route("/cycles/{id}", get(history::get_cycle_history))
        .route("/recipes", get(get_recipes).post(create_saved_recipe))
        .route("/recipes/import", post(import_saved_recipe))
        .route(
            "/recipes/{id}",
            put(update_saved_recipe).delete(delete_saved_recipe),
        )
        .route("/recipes/{id}/export", get(export_saved_recipe))
        .route("/recipes/{id}/start", post(start_saved_recipe))
        .route("/runs/{id}/name", post(rename_run))
        .route("/cycles/{id}/name", post(rename_cycle))
        .route("/connect", post(connect))
        .route("/disconnect", post(disconnect))
        .route("/test/start", post(start_test))
        .route("/test/adjust", post(adjust_test))
        .route("/test/stop", post(stop_test))
        .route("/test/resume", post(resume_test))
        .route("/cycle/start", post(start_cycle))
        .route("/cycle/stop", post(stop_cycle))
        .route("/calibration", post(calibration))
        .route("/ws", get(websocket))
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(error) => {
                    log::error!("failed to install SIGTERM handler: {error}");
                    let _ctrl_c = tokio::signal::ctrl_c().await;
                    return;
                }
            };
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    log::error!("failed to install shutdown signal handler: {error}");
                }
            }
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        log::error!("failed to install shutdown signal handler: {error}");
    }
}

async fn request(state: &AppState, request: ActorRequest) -> Result<ActorResponse, ApiError> {
    let (response_tx, response_rx) = oneshot::channel();
    state
        .actor_tx
        .send(ActorMessage {
            request,
            response: response_tx,
        })
        .map_err(|_send_error| ApiError::internal("device actor stopped"))?;
    response_rx
        .await
        .map_err(|_receive_error| ApiError::internal("device actor dropped its response"))?
        .map_err(ApiError::bad_request)
}

async fn command(
    state: &AppState,
    command: ApiCommand,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    match request(state, ActorRequest::Command(command)).await? {
        ActorResponse::Snapshot(snapshot) => Ok(Json(snapshot)),
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}

async fn cycle_command(
    state: &AppState,
    request_kind: ActorRequest,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    match request(state, request_kind).await? {
        ActorResponse::Snapshot(snapshot) => Ok(Json(snapshot)),
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}

async fn start_command(
    state: &AppState,
    request_kind: ActorRequest,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    match request(state, request_kind).await? {
        ActorResponse::Start(Ok(snapshot)) => Ok(Json(snapshot)),
        ActorResponse::Start(Err(StartError::BadRequest(message))) => {
            Err(ApiError::bad_request(message))
        }
        ActorResponse::Start(Err(StartError::NotFound(message))) => {
            Err(ApiError::not_found(message))
        }
        ActorResponse::Start(Err(StartError::Internal(message))) => {
            Err(ApiError::internal(message))
        }
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}

async fn get_status(
    State(state): State<AppState>,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    match request(&state, ActorRequest::Snapshot).await? {
        ActorResponse::Snapshot(snapshot) => Ok(Json(snapshot)),
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}

async fn get_history(State(state): State<AppState>) -> Result<Json<Vec<Sample>>, ApiError> {
    match request(&state, ActorRequest::History).await? {
        ActorResponse::History(history) => Ok(Json(history)),
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}

async fn get_history_csv(State(state): State<AppState>) -> Result<Response, ApiError> {
    match request(&state, ActorRequest::HistoryCsv).await? {
        ActorResponse::Export(export) => export_response(export),
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}

async fn get_cycle_history_csv(State(state): State<AppState>) -> Result<Response, ApiError> {
    match request(&state, ActorRequest::CycleHistoryCsv).await? {
        ActorResponse::Export(export) => export_response(export),
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}

async fn get_cycle_csv(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, ApiError> {
    if !valid_run_id(&id) {
        return Err(ApiError::bad_request("invalid cycle execution id"));
    }
    match request(&state, ActorRequest::CycleCsv(id)).await? {
        ActorResponse::ArchivedExport(Ok(Some(export))) => export_response(export),
        ActorResponse::ArchivedExport(Ok(None)) => {
            Err(ApiError::not_found("history CSV not found"))
        }
        ActorResponse::ArchivedExport(Err(error)) => Err(ApiError::internal(error)),
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}

async fn get_runs(State(state): State<AppState>) -> Result<Json<Vec<RunSummary>>, ApiError> {
    match request(&state, ActorRequest::Runs).await? {
        ActorResponse::Runs(runs) => Ok(Json(runs)),
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}

async fn get_recipes(State(state): State<AppState>) -> Result<Json<Vec<SavedRecipe>>, ApiError> {
    match request(&state, ActorRequest::Recipes).await? {
        ActorResponse::Recipes(recipes) => Ok(Json(recipes)),
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}

async fn create_saved_recipe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request_body): Json<CreateSavedRecipeRequest>,
) -> Result<Json<SavedRecipe>, ApiError> {
    validate_mutation(&headers, &state)?;
    recipe_response(request(&state, ActorRequest::CreateRecipe(request_body)).await?)
}

async fn update_saved_recipe(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(request_body): Json<UpdateSavedRecipeRequest>,
) -> Result<Json<SavedRecipe>, ApiError> {
    validate_mutation(&headers, &state)?;
    recipe_response(
        request(
            &state,
            ActorRequest::UpdateRecipe {
                id,
                request: request_body,
            },
        )
        .await?,
    )
}

async fn delete_saved_recipe(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(request_body): Json<DeleteSavedRecipeRequest>,
) -> Result<Json<SavedRecipe>, ApiError> {
    validate_mutation(&headers, &state)?;
    recipe_response(
        request(
            &state,
            ActorRequest::DeleteRecipe {
                id,
                request: request_body,
            },
        )
        .await?,
    )
}

async fn import_saved_recipe(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<SavedRecipe>, ApiError> {
    validate_mutation(&headers, &state)?;
    let export: RecipeExport = serde_json::from_slice(&body)
        .map_err(|error| ApiError::bad_request(format!("invalid recipe JSON: {error}")))?;
    recipe_response(request(&state, ActorRequest::ImportRecipe(export)).await?)
}

async fn export_saved_recipe(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, ApiError> {
    match request(&state, ActorRequest::ExportRecipe(id)).await? {
        ActorResponse::RecipeExport(Ok(export)) => {
            let filename = recipe_export_filename(&export.name);
            let disposition = HeaderValue::from_str(&format!(
                "attachment; filename=\"{filename}\""
            ))
            .map_err(|error| ApiError::internal(format!("invalid export filename: {error}")))?;
            let body = serde_json::to_vec_pretty(&export)
                .map_err(|error| ApiError::internal(format!("failed to encode recipe: {error}")))?;
            Ok((
                [
                    (
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json; charset=utf-8"),
                    ),
                    (header::CONTENT_DISPOSITION, disposition),
                ],
                body,
            )
                .into_response())
        }
        ActorResponse::RecipeExport(Err(error)) => Err(recipe_api_error(error)),
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}

fn recipe_export_filename(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, ' ' | '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches([' ', '.']);
    let stem = if sanitized.is_empty() {
        "recipe"
    } else {
        sanitized
    };
    format!("{stem}.ebc-recipe.json")
}

async fn start_saved_recipe(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(request_body): Json<StartSavedRecipeRequest>,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    validate_mutation(&headers, &state)?;
    start_command(
        &state,
        ActorRequest::StartSavedRecipe {
            id,
            request: request_body,
        },
    )
    .await
}

fn recipe_response(response: ActorResponse) -> Result<Json<SavedRecipe>, ApiError> {
    match response {
        ActorResponse::Recipe(Ok(recipe)) => Ok(Json(recipe)),
        ActorResponse::Recipe(Err(error)) => Err(recipe_api_error(error)),
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}

fn recipe_api_error(error: RecipeError) -> ApiError {
    match error {
        RecipeError::BadRequest(message) => ApiError::bad_request(message),
        RecipeError::NotFound(message) => ApiError::not_found(message),
        RecipeError::Conflict(message) => ApiError::conflict(message),
        RecipeError::Internal(message) => ApiError::internal(message),
    }
}

async fn get_run_csv(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, ApiError> {
    if !valid_run_id(&id) {
        return Err(ApiError::bad_request("invalid run id"));
    }
    match request(&state, ActorRequest::RunCsv(id)).await? {
        ActorResponse::ArchivedExport(Ok(Some(export))) => export_response(export),
        ActorResponse::ArchivedExport(Ok(None)) => {
            Err(ApiError::not_found("history CSV not found"))
        }
        ActorResponse::ArchivedExport(Err(error)) => Err(ApiError::internal(error)),
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}

fn export_response(export: ExportDescriptor) -> Result<Response, ApiError> {
    let disposition =
        HeaderValue::from_str(&format!("attachment; filename=\"{}\"", export.filename))
            .map_err(|error| ApiError::internal(format!("invalid export filename: {error}")))?;
    let length = HeaderValue::from_str(&export.length.to_string())
        .map_err(|error| ApiError::internal(format!("invalid export length: {error}")))?;
    let file = tokio_fs::File::from_std(export.file).take(export.length);
    let body = Body::from_stream(ReaderStream::new(file));
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/csv; charset=utf-8"),
            ),
            (header::CONTENT_DISPOSITION, disposition),
            (header::CONTENT_LENGTH, length),
        ],
        body,
    )
        .into_response())
}

async fn connect(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    validate_mutation(&headers, &state)?;
    command(&state, ApiCommand::Connect).await
}

async fn disconnect(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    validate_mutation(&headers, &state)?;
    command(&state, ApiCommand::Disconnect).await
}

async fn start_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut request_body): Json<StartTestRequest>,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    validate_mutation(&headers, &state)?;
    request_body
        .config
        .validate()
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    request_body.name = normalize_optional_name(request_body.name.as_deref())
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    start_command(&state, ActorRequest::StartTest(request_body)).await
}

async fn stop_test(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    validate_mutation(&headers, &state)?;
    command(&state, ApiCommand::Stop).await
}

async fn adjust_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(config): Json<TestConfiguration>,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    validate_mutation(&headers, &state)?;
    config
        .validate()
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    command(&state, ApiCommand::Adjust(config)).await
}

async fn resume_test(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    validate_mutation(&headers, &state)?;
    command(&state, ApiCommand::Resume).await
}

async fn start_cycle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut request_body): Json<StartCycleRequest>,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    validate_mutation(&headers, &state)?;
    request_body
        .recipe
        .validate()
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    request_body.name = normalize_optional_name(request_body.name.as_deref())
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    start_command(&state, ActorRequest::StartCycle(request_body)).await
}

async fn rename_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(run_id): AxumPath<String>,
    Json(request_body): Json<RenameRequest>,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    validate_mutation(&headers, &state)?;
    if !valid_run_id(&run_id) {
        return Err(ApiError::bad_request("invalid run id"));
    }
    normalize_optional_name(request_body.name.as_deref())
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    rename_command(
        &state,
        ActorRequest::RenameRun {
            id: run_id,
            request: request_body,
        },
    )
    .await
}

async fn rename_cycle(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(execution_id): AxumPath<String>,
    Json(request_body): Json<RenameRequest>,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    validate_mutation(&headers, &state)?;
    if !valid_run_id(&execution_id) {
        return Err(ApiError::bad_request("invalid cycle execution id"));
    }
    normalize_optional_name(request_body.name.as_deref())
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    rename_command(
        &state,
        ActorRequest::RenameCycle {
            execution_id,
            request: request_body,
        },
    )
    .await
}

async fn rename_command(
    state: &AppState,
    request_kind: ActorRequest,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    match request(state, request_kind).await? {
        ActorResponse::Rename(Ok(snapshot)) => Ok(Json(snapshot)),
        ActorResponse::Rename(Err(RenameError::BadRequest(message))) => {
            Err(ApiError::bad_request(message))
        }
        ActorResponse::Rename(Err(RenameError::NotFound(message))) => {
            Err(ApiError::not_found(message))
        }
        ActorResponse::Rename(Err(RenameError::Internal(message))) => {
            Err(ApiError::internal(message))
        }
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}

async fn stop_cycle(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    validate_mutation(&headers, &state)?;
    cycle_command(&state, ActorRequest::StopCycle).await
}

async fn calibration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(calibration): Json<CalibrationCommand>,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    validate_mutation(&headers, &state)?;
    calibration
        .validate()
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    command(&state, ApiCommand::Calibration(calibration)).await
}

async fn websocket(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Err(error) = validate_origin(&headers, &state, true) {
        return error.into_response();
    }
    upgrade
        .on_upgrade(move |socket| websocket_client(socket, state))
        .into_response()
}

fn validate_mutation(headers: &HeaderMap, state: &AppState) -> Result<(), ApiError> {
    if headers
        .get("x-ebc-command")
        .and_then(|value| value.to_str().ok())
        != Some("1")
    {
        return Err(ApiError::forbidden("X-EBC-Command: 1 is required"));
    }
    validate_origin(headers, state, false)
}

fn validate_origin(
    headers: &HeaderMap,
    state: &AppState,
    origin_required: bool,
) -> Result<(), ApiError> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return if origin_required {
            Err(ApiError::forbidden("Origin is required"))
        } else {
            Ok(())
        };
    };
    let origin = origin
        .to_str()
        .map_err(|_invalid_header| ApiError::forbidden("invalid Origin"))?;
    if origin == "null" {
        return Err(ApiError::forbidden("Origin null is not allowed"));
    }
    if let Some(allowed) = &state.allowed_origin {
        return if origin == allowed {
            Ok(())
        } else {
            Err(ApiError::forbidden("Origin is not allowed"))
        };
    }
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::forbidden("Host is required with Origin"))?;
    let http_origin = format!("http://{host}");
    let https_origin = format!("https://{host}");
    if origin == http_origin || origin == https_origin {
        Ok(())
    } else {
        Err(ApiError::forbidden("Origin does not match Host"))
    }
}

async fn websocket_client(mut socket: WebSocket, state: AppState) {
    let Ok(ActorResponse::Subscription(snapshot, recipes, mut updates)) =
        request(&state, ActorRequest::Subscribe).await
    else {
        return;
    };
    if send_event(&mut socket, &WebSocketEvent::Snapshot(snapshot))
        .await
        .is_err()
    {
        return;
    }
    if send_event(&mut socket, &WebSocketEvent::RecipeLibrary(recipes))
        .await
        .is_err()
    {
        return;
    }
    loop {
        let event = match updates.recv().await {
            Ok(event) => event,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                match request(&state, ActorRequest::Subscribe).await {
                    Ok(ActorResponse::Subscription(snapshot, recipes, replacement)) => {
                        updates = replacement;
                        if send_event(&mut socket, &WebSocketEvent::Snapshot(snapshot))
                            .await
                            .is_err()
                            || send_event(&mut socket, &WebSocketEvent::RecipeLibrary(recipes))
                                .await
                                .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                    _ => break,
                }
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };
        if send_event(&mut socket, &event).await.is_err() {
            break;
        }
    }
}

async fn send_event(socket: &mut WebSocket, event: &WebSocketEvent) -> Result<(), ()> {
    let text = serde_json::to_string(event).map_err(|_serialize_error| ())?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_send_error| ())
}

#[derive(Serialize)]
struct ApiErrorBody {
    error: String,
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ApiErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "test setup and assertions should fail fast"
)]
mod tests {
    use super::*;

    fn temporary_directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ebc-server-{name}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().expect("valid timestamp")
        ));
        fs::create_dir_all(&path).expect("create test directory");
        path
    }

    #[test]
    fn persistence_round_trip_keeps_samples_and_recovers_running_state() {
        let directory = temporary_directory("persistence");
        let mut persistence = Persistence::new(&directory).expect("create persistence");
        let mut snapshot = AuthoritativeSnapshot::default();
        snapshot.test.state = TestState::Running;
        snapshot.test.started_at_utc = Some("2026-01-01T00:00:00Z".to_owned());
        let sample = Sample {
            run_id: "run-1".to_owned(),
            sequence: 0,
            timestamp_utc: "2026-01-01T00:00:01Z".to_owned(),
            elapsed_seconds: 1,
            voltage_mv: 4000,
            current_ma: 1000,
            capacity_mah: 1,
            energy_wh: 0.001,
            mode: device::DeviceMode::DischargeConstantCurrent,
        };
        persistence.save_metadata(&snapshot).expect("save metadata");
        persistence.append_sample(&sample).expect("append sample");

        let loaded = persistence.load().expect("load persistence");
        assert_eq!(loaded.test.state, TestState::RecoveredUncertain);
        assert_eq!(loaded.history, vec![sample]);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn old_csv_rows_load_with_zero_energy() {
        let sample = parse_sample("2026-01-01T00:00:01Z,1,4000,1000,1,DischargeConstantCurrent")
            .expect("parse old CSV row");
        assert!((sample.energy_wh - 0.0).abs() < f64::EPSILON);

        let current =
            parse_sample("2026-01-01T00:00:02Z,2,4000,1000,2,0.0045,DischargeConstantCurrent")
                .expect("parse current CSV row");
        assert!((current.energy_wh - 0.0045).abs() < f64::EPSILON);
    }

    #[test]
    fn archives_current_run_once_and_loads_summaries_after_restart() {
        let directory = temporary_directory("archive");
        let mut persistence = Persistence::new(&directory).expect("create persistence");
        let sample = Sample {
            run_id: "run-1".to_owned(),
            sequence: 0,
            timestamp_utc: "2026-01-01T00:00:01Z".to_owned(),
            elapsed_seconds: 1,
            voltage_mv: 4000,
            current_ma: 1000,
            capacity_mah: 1,
            energy_wh: 0.001,
            mode: device::DeviceMode::DischargeConstantCurrent,
        };
        let mut snapshot = AuthoritativeSnapshot::default();
        snapshot.test.config = Some(TestConfiguration::DischargeConstantCurrent {
            current_ma: 1000,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 0,
        });
        snapshot.test.started_at_utc = Some("2026-01-01T00:00:00Z".to_owned());
        snapshot.test.state = TestState::Completed;
        snapshot.test.energy_wh = 0.001;
        snapshot.history.push(sample.clone());
        persistence.save_metadata(&snapshot).expect("save metadata");
        persistence.append_sample(&sample).expect("append sample");

        let summary = persistence
            .archive_current(&snapshot)
            .expect("archive current run")
            .expect("meaningful run");
        assert!(valid_run_id(&summary.id));
        assert!(
            read_export(persistence.run_export(&summary.id).expect("open archive"))
                .contains("0.001000000")
        );
        persistence
            .archive_current(&snapshot)
            .expect("archive is idempotent");
        assert_eq!(persistence.run_summaries().len(), 1);
        assert!(persistence.run_export("../session").is_err());
        drop(persistence);

        let mut restarted = Persistence::new(&directory).expect("restart persistence");
        let recovered = restarted.load().expect("load current metadata");
        restarted
            .archive_current(&recovered)
            .expect("restart archive is idempotent");
        assert_eq!(restarted.run_summaries().len(), 1);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    pub(super) fn mock_actor(name: &str) -> (DeviceActor, PathBuf) {
        let directory = temporary_directory(name);
        let (snapshot_tx, _) = broadcast::channel(16);
        let config = ServerConfig {
            http_addr: "127.0.0.1:0".parse().expect("test address"),
            serial_port: "/dev/null".to_owned(),
            data_dir: directory.clone(),
            mock: true,
            static_dir: directory.clone(),
        };
        (
            DeviceActor::new(config, snapshot_tx).expect("create actor"),
            directory,
        )
    }

    fn send_actor_request(
        sender: &std_mpsc::Sender<ActorMessage>,
        request: ActorRequest,
    ) -> Result<ActorResponse, String> {
        let (response, receiver) = oneshot::channel();
        sender
            .send(ActorMessage { request, response })
            .expect("actor accepts request");
        receiver.blocking_recv().expect("actor returns response")
    }

    pub(super) fn read_export(mut export: ExportDescriptor) -> String {
        let mut contents = String::new();
        (&mut export.file)
            .take(export.length)
            .read_to_string(&mut contents)
            .expect("read export");
        contents
    }

    pub(super) fn test_config() -> TestConfiguration {
        TestConfiguration::DischargeConstantCurrent {
            current_ma: 1000,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 0,
        }
    }

    pub(super) fn device_step() -> crate::core::CycleStep {
        crate::core::CycleStep::Device {
            config: test_config(),
            completion: crate::core::CycleStepCompletion::Hardware,
        }
    }

    pub(super) fn cycle_recipe(
        steps: Vec<crate::core::CycleStep>,
        repeat_count: u32,
    ) -> crate::core::CycleRecipe {
        crate::core::CycleRecipe {
            steps,
            repeat_count,
        }
    }

    pub(super) fn unnamed_cycle(recipe: crate::core::CycleRecipe) -> StartCycleRequest {
        StartCycleRequest { recipe, name: None }
    }

    fn saved_recipe_request(name: &str) -> CreateSavedRecipeRequest {
        CreateSavedRecipeRequest {
            name: name.to_owned(),
            recipe: cycle_recipe(
                vec![crate::core::CycleStep::Rest {
                    duration_seconds: 60,
                }],
                1,
            ),
        }
    }

    pub(super) fn confirm_inactive(actor: &mut DeviceActor) {
        actor.snapshot.connection = ServerConnectionState::Connected;
        actor.controller.connection_established();
        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            4200,
            0,
            actor.snapshot.device.capacity_mah.unwrap_or(0),
            ReportState::Idle,
            "EBC-MOCK",
            None,
        );
    }

    fn inject_write_failure(actor: &mut DeviceActor) {
        actor.serial_buffer.extend_from_slice(&[0xAA, 0xBB]);
        actor.write_failure = Some("injected serial write failure".to_owned());
    }

    pub(super) fn confirm_running(actor: &mut DeviceActor) {
        actor
            .start_test(test_config(), None, None)
            .expect("start test");
        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            4000,
            1000,
            1,
            ReportState::Active,
            "EBC-MOCK",
            None,
        );
    }

    fn assert_failed_transport(actor: &DeviceActor) {
        assert_eq!(actor.snapshot.connection, ServerConnectionState::Error);
        assert!(actor.port.is_none());
        assert!(actor.serial_buffer.is_empty());
        assert!(!actor.snapshot.device.activity_known);
    }

    pub(super) fn numbered_sample(sequence: u64, voltage_mv: u16, current_ma: u16) -> Sample {
        Sample {
            run_id: "run-1".to_owned(),
            sequence,
            timestamp_utc: format!("2026-01-01T00:00:{sequence:02}Z"),
            elapsed_seconds: sequence,
            voltage_mv,
            current_ma,
            capacity_mah: sequence,
            energy_wh: sequence as f64 / 1000.0,
            mode: device::DeviceMode::DischargeConstantCurrent,
        }
    }

    pub(super) fn numbered_cycle_sample(execution_id: &str, sequence: u64) -> CycleSample {
        CycleSample {
            execution_id: execution_id.to_owned(),
            sequence,
            timestamp_utc: format!("2026-01-01T00:00:{sequence:02}Z"),
            elapsed_milliseconds: sequence * 250,
            repeat_index: 0,
            step_index: 0,
            cycle_state: CycleState::RunningStep,
            test_state: TestState::Running,
            mode: device::DeviceMode::DischargeConstantCurrent,
            activity_known: true,
            active: true,
            voltage_mv: 4000,
            current_ma: 1000,
            device_capacity_mah: u16::try_from(sequence).unwrap_or(u16::MAX),
            test_capacity_mah: Some(sequence),
            test_energy_wh: sequence as f64 / 1000.0,
        }
    }

    #[test]
    fn two_clients_concurrently_start_race_stop_and_disappear_safely() {
        let (actor, directory) = mock_actor("two-client-channel-races");
        let (actor_tx, actor_rx) = std_mpsc::channel();
        let actor_thread = thread::spawn(move || actor.run(&actor_rx));

        for _ in 0..50 {
            let ActorResponse::Snapshot(snapshot) =
                send_actor_request(&actor_tx, ActorRequest::Snapshot).expect("initial status")
            else {
                panic!("unexpected status response");
            };
            if snapshot.device.activity_known {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let clients: Vec<_> = (0..2)
            .map(|_| {
                let sender = actor_tx.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    send_actor_request(
                        &sender,
                        ActorRequest::Command(ApiCommand::Start(test_config())),
                    )
                })
            })
            .collect();
        barrier.wait();
        let start_results: Vec<_> = clients
            .into_iter()
            .map(|client| client.join().expect("client thread"))
            .collect();
        assert_eq!(
            start_results.iter().filter(|result| result.is_ok()).count(),
            1
        );

        let ActorResponse::Snapshot(before) =
            send_actor_request(&actor_tx, ActorRequest::Snapshot).expect("status request")
        else {
            panic!("unexpected status response");
        };
        let disappearing_client = actor_tx.clone();
        drop(disappearing_client);
        send_actor_request(&actor_tx, ActorRequest::Command(ApiCommand::Connect))
            .expect("reconnect request");
        let ActorResponse::Snapshot(after) =
            send_actor_request(&actor_tx, ActorRequest::Snapshot).expect("status after reconnect")
        else {
            panic!("unexpected status response");
        };
        assert_eq!(before.test.state, TestState::Starting);
        assert_eq!(after.test.state, TestState::RecoveredUncertain);
        assert_eq!(after.test.config, before.test.config);
        assert_eq!(after.test.started_at_utc, before.test.started_at_utc);

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let start_sender = actor_tx.clone();
        let start_barrier = std::sync::Arc::clone(&barrier);
        let start_client = thread::spawn(move || {
            start_barrier.wait();
            send_actor_request(
                &start_sender,
                ActorRequest::Command(ApiCommand::Start(test_config())),
            )
        });
        let stop_sender = actor_tx.clone();
        let stop_barrier = std::sync::Arc::clone(&barrier);
        let stop_client = thread::spawn(move || {
            stop_barrier.wait();
            send_actor_request(&stop_sender, ActorRequest::Command(ApiCommand::Stop))
        });
        barrier.wait();
        assert!(start_client.join().expect("start client").is_err());
        assert!(stop_client.join().expect("stop client").is_ok());

        send_actor_request(&actor_tx, ActorRequest::Shutdown).expect("shutdown actor");
        drop(actor_tx);
        actor_thread.join().expect("join actor");
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn duplicate_and_stale_reports_keep_metrics_monotonic() {
        let (mut actor, directory) = mock_actor("duplicate-reports");
        confirm_inactive(&mut actor);
        actor
            .start_test(test_config(), None, None)
            .expect("start command");
        for raw_capacity in [100, 100, 95] {
            actor.record_report(
                device::DeviceMode::DischargeConstantCurrent,
                4000,
                1000,
                raw_capacity,
                ReportState::Active,
                "EBC-MOCK",
                None,
            );
        }
        assert_eq!(actor.snapshot.history.len(), 3);
        assert_eq!(
            actor
                .snapshot
                .history
                .iter()
                .map(|sample| sample.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        for samples in actor.snapshot.history.windows(2) {
            assert!(samples[1].elapsed_seconds >= samples[0].elapsed_seconds);
            assert!(samples[1].energy_wh >= samples[0].energy_wh);
            assert!(samples[1].capacity_mah >= samples[0].capacity_mah);
        }
        assert_eq!(actor.snapshot.test.capacity_mah, Some(100));
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the test verifies one end-to-end provenance sequence"
    )]
    fn uncertain_stop_preserves_owned_metrics_and_does_not_append_samples() {
        let (mut actor, directory) = mock_actor("uncertain-stop-metrics");
        confirm_inactive(&mut actor);
        confirm_running(&mut actor);
        actor
            .persistence
            .flush_samples()
            .expect("flush owned sample");
        let history = actor.snapshot.history.clone();
        assert!(!history.is_empty());
        let sample_count = actor.persistence.raw_sample_count;
        let next_sequence = actor.persistence.next_sequence;
        let csv_before = fs::read(&actor.persistence.samples_path).expect("read owned CSV");
        let owned_elapsed = actor.snapshot.test.elapsed_seconds;
        let owned_capacity = actor.snapshot.test.capacity_mah;
        let owned_energy = actor.snapshot.test.energy_wh;

        actor.controller.begin_connection("serial observation gap");
        actor.controller.connection_established();
        actor.sync_controller_state();
        let mut events = actor.snapshot_tx.subscribe();

        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            3900,
            900,
            20,
            ReportState::Active,
            "EBC-MOCK",
            None,
        );
        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            3800,
            800,
            30,
            ReportState::Active,
            "EBC-MOCK",
            None,
        );

        assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
        assert_eq!(actor.snapshot.device.voltage_mv, Some(3800));
        assert_eq!(actor.snapshot.device.current_ma, Some(800));
        assert_eq!(actor.snapshot.device.capacity_mah, Some(30));
        assert_eq!(actor.snapshot.test.elapsed_seconds, owned_elapsed);
        assert_eq!(actor.snapshot.test.capacity_mah, owned_capacity);
        assert!((actor.snapshot.test.energy_wh - owned_energy).abs() < f64::EPSILON);
        assert_eq!(actor.snapshot.history, history);
        assert_eq!(actor.persistence.raw_sample_count, sample_count);
        assert_eq!(actor.persistence.next_sequence, next_sequence);

        actor.stop_test().expect("stop uncertain test");
        assert_eq!(actor.snapshot.test.state, TestState::Stopping);
        assert_eq!(actor.snapshot.history, history);
        assert_eq!(actor.persistence.raw_sample_count, sample_count);
        assert_eq!(actor.persistence.next_sequence, next_sequence);

        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            3800,
            0,
            40,
            ReportState::Idle,
            "EBC-MOCK",
            None,
        );

        assert_eq!(actor.snapshot.test.state, TestState::Stopped);
        assert_eq!(
            actor.snapshot.test.result.as_deref(),
            Some("stop confirmed by hardware")
        );
        assert_eq!(actor.snapshot.device.capacity_mah, Some(40));
        assert_eq!(actor.snapshot.test.elapsed_seconds, owned_elapsed);
        assert_eq!(actor.snapshot.test.capacity_mah, owned_capacity);
        assert!((actor.snapshot.test.energy_wh - owned_energy).abs() < f64::EPSILON);
        assert_eq!(actor.snapshot.history, history);
        assert_eq!(actor.persistence.raw_sample_count, sample_count);
        assert_eq!(actor.persistence.next_sequence, next_sequence);

        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            3800,
            0,
            50,
            ReportState::Idle,
            "EBC-MOCK",
            None,
        );
        assert_eq!(actor.snapshot.device.capacity_mah, Some(50));
        assert_eq!(actor.snapshot.test.elapsed_seconds, owned_elapsed);
        assert_eq!(actor.snapshot.test.capacity_mah, owned_capacity);
        assert!((actor.snapshot.test.energy_wh - owned_energy).abs() < f64::EPSILON);
        assert_eq!(actor.snapshot.history, history);
        assert_eq!(actor.persistence.raw_sample_count, sample_count);
        assert_eq!(actor.persistence.next_sequence, next_sequence);
        let csv_after = fs::read(&actor.persistence.samples_path).expect("read recovered CSV");
        assert_eq!(csv_after, csv_before);
        let persisted = actor.persistence.load().expect("load recovered metadata");
        assert_eq!(persisted.test.elapsed_seconds, owned_elapsed);
        assert_eq!(persisted.test.capacity_mah, owned_capacity);
        assert!((persisted.test.energy_wh - owned_energy).abs() < f64::EPSILON);

        let mut restarted = TestController::from_state(
            ControllerMode::Server,
            persisted.device.clone(),
            persisted.test.clone(),
            persisted.history.last(),
        );
        restarted.begin_connection("server restart");
        restarted.connection_established();
        restarted.report(DeviceReport {
            mode: device::DeviceMode::DischargeConstantCurrent,
            state: ReportState::Idle,
            voltage_mv: 3800,
            current_ma: 0,
            capacity_mah: 50,
            model: "EBC-MOCK".to_owned(),
            firmware_version: None,
        });
        let resume = restarted
            .prepare_command(ApiCommand::Resume)
            .expect("prepare Continue after restart");
        restarted.commit_command(resume, None);
        let (_, measurement) = restarted.report(DeviceReport {
            mode: device::DeviceMode::DischargeConstantCurrent,
            state: ReportState::Active,
            voltage_mv: 3800,
            current_ma: 1000,
            capacity_mah: 51,
            model: "EBC-MOCK".to_owned(),
            firmware_version: None,
        });
        assert!(measurement.is_some());
        assert_eq!(
            restarted.test().capacity_mah,
            owned_capacity.map(|capacity| capacity + 1)
        );
        while let Ok(event) = events.try_recv() {
            assert!(matches!(event, WebSocketEvent::Update(_)));
        }
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn mock_disconnect_cancels_deferred_idle_report() {
        let (mut actor, directory) = mock_actor("mock-disconnect");
        actor.connect().expect("connect mock");
        assert!(actor.mock_idle_report_due.is_some());

        actor.disconnect().expect("disconnect mock");

        assert_eq!(actor.mock_idle_report_due, None);
        assert!(!actor.snapshot.device.activity_known);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn idle_disconnect_sends_only_disconnect_and_succeeds() {
        let (mut actor, directory) = mock_actor("idle-safe-disconnect");
        confirm_inactive(&mut actor);
        actor.sent_frames.clear();

        actor
            .handle_command(ApiCommand::Disconnect)
            .expect("disconnect idle device");

        assert!(matches!(
            actor.sent_frames.as_slice(),
            [OutboundFrame::Disconnect]
        ));
        assert_eq!(
            actor.snapshot.connection,
            ServerConnectionState::Disconnected
        );
        assert!(actor.port.is_none());
        assert!(actor.serial_buffer.is_empty());
        assert!(!actor.snapshot.device.activity_known);
        assert_eq!(actor.snapshot.test.state, TestState::Idle);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn running_disconnect_stops_then_disconnects_and_notifies_all_clients() {
        let (mut actor, directory) = mock_actor("running-safe-disconnect");
        confirm_inactive(&mut actor);
        confirm_running(&mut actor);
        let history = actor.snapshot.history.clone();
        let current_run_id = actor.persistence.current_run_id.clone();
        let archived_runs = actor.persistence.runs.len();
        actor.sent_frames.clear();
        let mut first_client = actor.snapshot_tx.subscribe();
        let mut second_client = actor.snapshot_tx.subscribe();

        actor
            .handle_command(ApiCommand::Disconnect)
            .expect("disconnect running device");

        assert!(matches!(
            actor.sent_frames.as_slice(),
            [OutboundFrame::Stop, OutboundFrame::Disconnect]
        ));
        assert_eq!(
            actor.snapshot.connection,
            ServerConnectionState::Disconnected
        );
        assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
        assert_eq!(actor.snapshot.history, history);
        assert_eq!(actor.persistence.current_run_id, current_run_id);
        assert_eq!(actor.persistence.runs.len(), archived_runs);
        let durable_samples = fs::read_to_string(&actor.persistence.samples_path)
            .expect("read flushed current samples");
        assert!(durable_samples.lines().count() >= 2);
        assert!(actor.port.is_none());
        for client in [&mut first_client, &mut second_client] {
            let WebSocketEvent::Update(update) = client
                .try_recv()
                .expect("client receives disconnect update")
            else {
                panic!("expected disconnect update");
            };
            assert_eq!(update.connection, ServerConnectionState::Disconnected);
            assert_eq!(update.test.state, TestState::RecoveredUncertain);
        }
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn uncertain_active_disconnect_stops_before_disconnect() {
        let (mut actor, directory) = mock_actor("uncertain-safe-disconnect");
        confirm_inactive(&mut actor);
        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            4000,
            1000,
            1,
            ReportState::Active,
            "EBC-MOCK",
            None,
        );
        assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
        actor.sent_frames.clear();

        actor
            .handle_command(ApiCommand::Disconnect)
            .expect("disconnect uncertain active device");

        assert!(matches!(
            actor.sent_frames.as_slice(),
            [OutboundFrame::Stop, OutboundFrame::Disconnect]
        ));
        assert_eq!(
            actor.snapshot.connection,
            ServerConnectionState::Disconnected
        );
        assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn starting_disconnect_stops_before_disconnect() {
        let (mut actor, directory) = mock_actor("starting-safe-disconnect");
        confirm_inactive(&mut actor);
        actor
            .start_test(test_config(), None, None)
            .expect("start test");
        assert_eq!(actor.snapshot.test.state, TestState::Starting);
        actor.sent_frames.clear();

        actor
            .handle_command(ApiCommand::Disconnect)
            .expect("disconnect starting device");

        assert!(matches!(
            actor.sent_frames.as_slice(),
            [OutboundFrame::Stop, OutboundFrame::Disconnect]
        ));
        assert_eq!(
            actor.snapshot.connection,
            ServerConnectionState::Disconnected
        );
        assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn already_stopping_disconnect_does_not_duplicate_stop() {
        let (mut actor, directory) = mock_actor("stopping-safe-disconnect");
        confirm_inactive(&mut actor);
        confirm_running(&mut actor);
        actor.stop_test().expect("begin stopping");
        assert_eq!(actor.snapshot.test.state, TestState::Stopping);
        actor.sent_frames.clear();

        actor
            .handle_command(ApiCommand::Disconnect)
            .expect("disconnect stopping device");

        assert!(matches!(
            actor.sent_frames.as_slice(),
            [OutboundFrame::Disconnect]
        ));
        assert_eq!(
            actor.snapshot.connection,
            ServerConnectionState::Disconnected
        );
        assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn disconnect_stop_write_failure_is_uncertain_and_does_not_disconnect() {
        let (mut actor, directory) = mock_actor("disconnect-stop-write-failure");
        confirm_inactive(&mut actor);
        confirm_running(&mut actor);
        actor.sent_frames.clear();
        inject_write_failure(&mut actor);

        actor
            .handle_command(ApiCommand::Disconnect)
            .expect_err("required stop write fails");

        assert!(matches!(
            actor.sent_frames.as_slice(),
            [OutboundFrame::Stop]
        ));
        assert_failed_transport(&actor);
        assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
        assert!(
            actor
                .snapshot
                .test
                .result
                .as_deref()
                .is_some_and(|reason| reason.contains("stop outcome is unknown"))
        );
        let persisted = actor.persistence.load().expect("load persisted failure");
        assert_eq!(persisted.test.state, TestState::RecoveredUncertain);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn disconnect_frame_write_failure_revokes_transport_trust() {
        let (mut actor, directory) = mock_actor("disconnect-frame-write-failure");
        confirm_inactive(&mut actor);
        actor.sent_frames.clear();
        inject_write_failure(&mut actor);

        actor
            .handle_command(ApiCommand::Disconnect)
            .expect_err("disconnect frame write fails");

        assert!(matches!(
            actor.sent_frames.as_slice(),
            [OutboundFrame::Disconnect]
        ));
        assert_failed_transport(&actor);
        assert_ne!(
            actor.snapshot.connection,
            ServerConnectionState::Disconnected
        );
        assert_eq!(
            actor.controller.physical_state(),
            crate::controller::PhysicalState::Unknown
        );
        let persisted = actor.persistence.load().expect("load persisted failure");
        assert!(!persisted.device.activity_known);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn disconnect_frame_failure_after_stop_remains_uncertain() {
        let (mut actor, directory) = mock_actor("running-disconnect-frame-failure");
        confirm_inactive(&mut actor);
        confirm_running(&mut actor);
        actor.sent_frames.clear();
        actor.write_failure_after = Some((1, "injected disconnect write failure".to_owned()));

        actor
            .handle_command(ApiCommand::Disconnect)
            .expect_err("disconnect frame fails after stop");

        assert!(matches!(
            actor.sent_frames.as_slice(),
            [OutboundFrame::Stop, OutboundFrame::Disconnect]
        ));
        assert_failed_transport(&actor);
        assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
        assert!(
            actor
                .snapshot
                .test
                .result
                .as_deref()
                .is_some_and(|reason| reason.contains("injected disconnect write failure"))
        );
        let persisted = actor.persistence.load().expect("load persisted failure");
        assert_eq!(persisted.test.state, TestState::RecoveredUncertain);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn disappearing_remote_client_does_not_change_running_test() {
        let (mut actor, directory) = mock_actor("disappearing-remote-client");
        confirm_inactive(&mut actor);
        confirm_running(&mut actor);
        actor.sent_frames.clear();
        let client = actor.snapshot_tx.subscribe();
        drop(client);

        assert!(actor.sent_frames.is_empty());
        assert_eq!(actor.snapshot.connection, ServerConnectionState::Connected);
        assert_eq!(actor.snapshot.test.state, TestState::Running);
        assert!(actor.snapshot.device.active);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn failed_start_write_is_persisted_as_uncertain_and_revokes_transport() {
        let (mut actor, directory) = mock_actor("failed-start-write");
        confirm_inactive(&mut actor);
        inject_write_failure(&mut actor);

        let error = actor
            .start_test(test_config(), Some("uncertain run".to_owned()), None)
            .expect_err("start write fails");

        assert!(error.to_string().contains("injected serial write failure"));
        assert_failed_transport(&actor);
        assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
        assert_eq!(
            actor.current_snapshot().current_run.name.as_deref(),
            Some("uncertain run")
        );
        assert!(
            actor
                .snapshot
                .test
                .result
                .as_deref()
                .is_some_and(|reason| reason.contains("start outcome is unknown"))
        );
        let persisted = actor.persistence.load().expect("load persisted failure");
        assert_eq!(persisted.test.state, TestState::RecoveredUncertain);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn start_persistence_failure_does_not_commit_or_send() {
        let (mut actor, directory) = mock_actor("failed-start-persistence");
        confirm_inactive(&mut actor);
        actor.persistence.metadata_path = directory.join("missing").join("session.json");
        inject_write_failure(&mut actor);

        actor
            .start_test(test_config(), None, None)
            .expect_err("metadata write fails");

        assert_eq!(actor.snapshot.test.state, TestState::Idle);
        assert_eq!(actor.snapshot.connection, ServerConnectionState::Connected);
        assert!(actor.snapshot.device.activity_known);
        assert!(
            actor.write_failure.is_some(),
            "physical send was not attempted"
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn cycle_start_metadata_failure_rolls_back_sidecar_and_status_before_action() {
        let (mut actor, directory) = mock_actor("failed-cycle-start-persistence");
        confirm_inactive(&mut actor);
        actor.start_metadata_failure = Some("injected metadata failure".to_owned());
        let prior_cycle = actor.current_snapshot().cycle;

        actor
            .start_cycle(unnamed_cycle(cycle_recipe(vec![device_step()], 1)))
            .expect_err("cycle metadata write fails");

        assert_eq!(actor.current_snapshot().cycle, prior_cycle);
        assert!(actor.sent_frames.is_empty());
        assert_eq!(
            fs::read_dir(&actor.persistence.cycles_dir)
                .expect("read cycle directory")
                .count(),
            0
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn late_start_metadata_failure_restores_archived_run_for_retry() {
        let (mut actor, directory) = mock_actor("late-start-metadata-failure");
        confirm_inactive(&mut actor);
        actor
            .start_test(test_config(), Some("stable name".to_owned()), None)
            .expect("start named test");
        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            4000,
            1000,
            1,
            ReportState::Active,
            "EBC-MOCK",
            None,
        );
        actor.stop_test().expect("stop previous run");
        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            4000,
            0,
            2,
            ReportState::Idle,
            "EBC-MOCK",
            None,
        );
        let previous_history = actor.snapshot.history.clone();
        let previous_run_id = actor.persistence.current_run_id.clone();
        let previous_run_name = actor.persistence.current_run_name.clone();
        let previous_sample_count = actor.persistence.raw_sample_count;
        actor.start_metadata_failure = Some("injected metadata failure".to_owned());
        actor.write_failure = Some("physical send must not run".to_owned());

        actor
            .start_test(test_config(), Some("attempted name".to_owned()), None)
            .expect_err("metadata write fails");

        assert_eq!(actor.snapshot.test.state, TestState::Stopped);
        assert_eq!(actor.snapshot.history, previous_history);
        assert_eq!(actor.persistence.current_run_id, previous_run_id);
        assert_eq!(actor.persistence.current_run_name, previous_run_name);
        assert_eq!(actor.current_snapshot().current_run.name, previous_run_name);
        assert_eq!(actor.persistence.raw_sample_count, previous_sample_count);
        assert_eq!(actor.persistence.runs.len(), 1);
        assert!(
            actor.write_failure.is_some(),
            "physical send was not attempted"
        );

        actor.write_failure = None;
        actor.start_metadata_failure = None;
        actor
            .start_test(test_config(), None, None)
            .expect("retry start");
        assert_eq!(actor.persistence.runs.len(), 1);
        assert_eq!(actor.snapshot.test.state, TestState::Starting);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn failed_resume_write_is_uncertain_and_revokes_transport() {
        let (mut actor, directory) = mock_actor("failed-resume-write");
        confirm_inactive(&mut actor);
        confirm_running(&mut actor);
        actor.stop_test().expect("stop test");
        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            4000,
            0,
            1,
            ReportState::Idle,
            "EBC-MOCK",
            None,
        );
        inject_write_failure(&mut actor);

        actor.resume_test().expect_err("resume write fails");

        assert_failed_transport(&actor);
        assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
        assert!(
            actor
                .snapshot
                .test
                .result
                .as_deref()
                .is_some_and(|reason| reason.contains("resume outcome is unknown"))
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn failed_stop_write_is_uncertain_and_revokes_transport() {
        let (mut actor, directory) = mock_actor("failed-stop-write");
        confirm_inactive(&mut actor);
        confirm_running(&mut actor);
        inject_write_failure(&mut actor);

        actor.stop_test().expect_err("stop write fails");

        assert_failed_transport(&actor);
        assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
        assert!(
            actor
                .snapshot
                .test
                .result
                .as_deref()
                .is_some_and(|reason| reason.contains("stop outcome is unknown"))
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn write_failure_is_published_even_when_persistence_fails() {
        let (mut actor, directory) = mock_actor("publish-failed-write");
        confirm_inactive(&mut actor);
        confirm_running(&mut actor);
        let mut updates = actor.snapshot_tx.subscribe();
        actor.persistence.metadata_path = directory.join("missing").join("session.json");
        inject_write_failure(&mut actor);

        actor.stop_test().expect_err("stop write fails");

        let WebSocketEvent::Update(update) = updates.try_recv().expect("failure update published")
        else {
            panic!("expected update");
        };
        assert_eq!(update.connection, ServerConnectionState::Error);
        assert_eq!(update.test.state, TestState::RecoveredUncertain);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn failed_adjust_write_does_not_commit_configuration() {
        let (mut actor, directory) = mock_actor("failed-adjust-write");
        confirm_inactive(&mut actor);
        confirm_running(&mut actor);
        let original = actor.snapshot.test.config;
        inject_write_failure(&mut actor);
        let adjusted = TestConfiguration::DischargeConstantCurrent {
            current_ma: 1500,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 0,
        };

        actor.adjust_test(adjusted).expect_err("adjust write fails");

        assert_failed_transport(&actor);
        assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
        assert_eq!(actor.snapshot.test.config, original);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn failed_calibration_write_does_not_stage_reference() {
        let (mut actor, directory) = mock_actor("failed-calibration-write");
        confirm_inactive(&mut actor);
        inject_write_failure(&mut actor);

        actor
            .calibrate(CalibrationCommand::VoltageLow(1000))
            .expect_err("calibration write fails");
        assert_failed_transport(&actor);

        actor.connect().expect("reconnect mock");
        confirm_inactive(&mut actor);
        actor
            .calibrate(CalibrationCommand::VoltageHigh(4000))
            .expect("stage high voltage");
        confirm_running(&mut actor);
        actor
            .calibrate(CalibrationCommand::CurrentLow(500))
            .expect("stage low current");
        actor
            .calibrate(CalibrationCommand::CurrentHigh(2000))
            .expect("stage high current");
        assert!(actor.calibrate(CalibrationCommand::Confirm).is_err());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn failed_timer_sync_write_uses_transport_failure_policy() {
        let (mut actor, directory) = mock_actor("failed-timer-sync-write");
        confirm_inactive(&mut actor);
        confirm_running(&mut actor);
        inject_write_failure(&mut actor);

        actor
            .send_frame_with_recovery(OutboundFrame::TimerSync(1), None)
            .expect_err("timer sync write fails");

        assert_failed_transport(&actor);
        assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
        assert_eq!(actor.controller.next_timer_sync(), None);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn startup_compacts_ten_thousand_legacy_rows_but_archive_count_is_exact() {
        let directory = temporary_directory("startup-compaction");
        let persistence = Persistence::new(&directory).expect("create persistence");
        let mut snapshot = AuthoritativeSnapshot::default();
        snapshot.test.config = Some(test_config());
        snapshot.test.state = TestState::Stopped;
        snapshot.test.started_at_utc = Some("2026-01-01T00:00:00Z".to_owned());
        persistence.save_metadata(&snapshot).expect("save metadata");
        let samples: Vec<Sample> = (0..10_000)
            .map(|index| {
                let mut sample = numbered_sample(index, 4000, 1000);
                sample.run_id.clear();
                sample.sequence = 0;
                sample
            })
            .collect();
        fs::write(
            directory.join("samples.csv"),
            Persistence::history_csv(&samples),
        )
        .expect("write legacy samples");
        drop(persistence);

        let (snapshot_tx, _) = broadcast::channel(1);
        let config = ServerConfig {
            http_addr: "127.0.0.1:0".parse().expect("test address"),
            serial_port: "/dev/null".to_owned(),
            data_dir: directory.clone(),
            mock: true,
            static_dir: directory.clone(),
        };
        let mut actor = DeviceActor::new(config, snapshot_tx).expect("load actor");
        assert!(actor.snapshot.history.len() <= SNAPSHOT_SAMPLE_LIMIT);
        assert_eq!(actor.persistence.raw_sample_count, 10_000);
        assert_eq!(actor.persistence.next_sequence, 10_000);
        assert_eq!(actor.snapshot.history.first().expect("first").sequence, 0);
        assert_eq!(actor.snapshot.history.last().expect("last").sequence, 9_999);
        let archive_snapshot = actor.snapshot.clone();
        let summary = actor
            .persistence
            .archive_current(&archive_snapshot)
            .expect("archive run")
            .expect("run summary");
        assert_eq!(summary.sample_count, 10_000);
        assert_eq!(
            read_export(
                actor
                    .persistence
                    .run_export(&summary.id)
                    .expect("archive CSV")
            )
            .lines()
            .count(),
            10_001
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn persisted_values_never_move_behind_last_durable_sample() {
        let directory = temporary_directory("persisted-skew");
        let mut persistence = Persistence::new(&directory).expect("create persistence");
        let mut snapshot = AuthoritativeSnapshot::default();
        snapshot.test.elapsed_seconds = 10;
        snapshot.test.energy_wh = 0.5;
        persistence.save_metadata(&snapshot).expect("save metadata");
        persistence
            .append_sample(&numbered_sample(7, 4000, 1000))
            .expect("append sample");
        persistence.flush_samples().expect("flush sample");
        let loaded = persistence.load().expect("load persistence");
        assert_eq!(loaded.test.elapsed_seconds, 10);
        assert!((loaded.test.energy_wh - 0.5).abs() < f64::EPSILON);

        let mut newer = numbered_sample(8, 4000, 1000);
        newer.elapsed_seconds = 20;
        newer.energy_wh = 0.75;
        persistence.append_sample(&newer).expect("append newer");
        persistence.flush_samples().expect("flush newer");
        let loaded = persistence.load().expect("reload persistence");
        assert_eq!(loaded.test.elapsed_seconds, 20);
        assert!((loaded.test.energy_wh - 0.75).abs() < f64::EPSILON);
        assert_eq!(persistence.next_sequence, 9);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn torn_final_csv_row_is_repaired_but_complete_corruption_is_rejected() {
        let directory = temporary_directory("torn-csv");
        let path = directory.join("samples.csv");
        let valid = sample_csv_row(&numbered_sample(0, 4000, 1000));
        fs::write(&path, format!("{}{valid}run-1,1,broken", csv_header())).expect("write torn CSV");
        let persistence = Persistence::new(&directory).expect("create persistence");
        let samples = persistence.load_samples().expect("repair torn row");
        assert_eq!(samples.len(), 1);
        assert!(
            fs::read_to_string(&path)
                .expect("read repaired CSV")
                .ends_with('\n')
        );

        fs::write(&path, format!("{}broken,complete,row\n", csv_header()))
            .expect("write corrupt CSV");
        assert!(persistence.load_samples().is_err());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn startup_ignores_incomplete_archive_pairs() {
        let directory = temporary_directory("incomplete-archives");
        let runs = directory.join("runs");
        fs::create_dir_all(&runs).expect("create runs");
        fs::write(runs.join("json-only.json"), b"{}\n").expect("write orphan JSON");
        fs::write(runs.join("csv-only.csv"), csv_header()).expect("write orphan CSV");
        fs::write(runs.join("pending.json.tmp"), b"{}\n").expect("write temp JSON");
        let persistence = Persistence::new(&directory).expect("load persistence");
        assert!(persistence.run_summaries().is_empty());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn presentation_is_bounded_and_preserves_endpoints_and_bucket_extrema() {
        let mut samples: Vec<Sample> = (0..20)
            .map(|sequence| numbered_sample(sequence, 4000, 1000))
            .collect();
        samples[3].voltage_mv = 100;
        samples[7].voltage_mv = 9000;
        samples[12].current_ma = 10;
        samples[17].current_ma = 6000;
        let presented = presentation_history(&samples, 10);
        let sequences: BTreeSet<u64> = presented.iter().map(|sample| sample.sequence).collect();
        assert!(presented.len() <= 10);
        for expected in [0, 3, 7, 12, 17, 19] {
            assert!(sequences.contains(&expected));
        }
    }

    #[test]
    fn raw_csv_remains_complete_when_presentation_is_bounded() {
        let directory = temporary_directory("raw-complete");
        let mut persistence = Persistence::new(&directory).expect("create persistence");
        persistence.reset_samples().expect("reset samples");
        let samples: Vec<Sample> = (0..100)
            .map(|sequence| numbered_sample(sequence, 4000, 1000))
            .collect();
        for sample in &samples {
            persistence.append_sample(sample).expect("append sample");
        }
        persistence.flush_samples().expect("flush samples");
        assert!(presentation_history(&samples, 20).len() <= 20);
        assert_eq!(persistence.load_samples().expect("load raw").len(), 100);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn cycle_csv_is_execution_specific_and_exports_a_durable_prefix() {
        let directory = temporary_directory("cycle-export-prefix");
        let mut persistence = Persistence::new(&directory).expect("create persistence");
        persistence
            .begin_cycle(
                "cycle-one".to_owned(),
                None,
                cycle_recipe(vec![device_step()], 1),
                None,
                "2026-01-01T00:00:00Z".to_owned(),
            )
            .expect("begin cycle");
        persistence
            .append_cycle_sample(&numbered_cycle_sample("cycle-one", 0))
            .expect("append first sample");
        let prefix = persistence.cycle_export(None).expect("capture prefix");
        let prefix_length = prefix.length;
        persistence
            .append_cycle_sample(&numbered_cycle_sample("cycle-one", 1))
            .expect("append second sample");
        let prefix_contents = read_export(prefix);
        assert_eq!(
            u64::try_from(prefix_contents.len()).expect("prefix length"),
            prefix_length
        );
        assert_eq!(prefix_contents.lines().count(), 2);

        persistence
            .begin_cycle(
                "cycle-two".to_owned(),
                None,
                cycle_recipe(vec![device_step()], 1),
                None,
                "2026-01-01T00:00:00Z".to_owned(),
            )
            .expect("begin second cycle");
        persistence
            .append_cycle_sample(&numbered_cycle_sample("cycle-two", 0))
            .expect("append second cycle sample");
        assert!(persistence.cycle_path("cycle-one").is_file());
        assert!(persistence.cycle_path("cycle-two").is_file());
        assert_eq!(
            read_export(
                persistence
                    .cycle_export(Some("cycle-one"))
                    .expect("export old cycle")
            )
            .lines()
            .count(),
            3
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn cycle_presentation_is_bounded_without_truncating_raw_csv() {
        let directory = temporary_directory("cycle-presentation-bound");
        let mut persistence = Persistence::new(&directory).expect("create persistence");
        persistence
            .begin_cycle(
                "cycle-bounded".to_owned(),
                None,
                cycle_recipe(vec![device_step()], 1),
                None,
                "2026-01-01T00:00:00Z".to_owned(),
            )
            .expect("begin cycle");
        let samples: Vec<_> = (0..100)
            .map(|sequence| numbered_cycle_sample("cycle-bounded", sequence))
            .collect();
        for sample in &samples {
            persistence
                .append_cycle_sample(sample)
                .expect("append cycle sample");
        }
        persistence
            .flush_cycle_samples()
            .expect("flush cycle samples");
        assert!(cycle_presentation_history(&samples, 20).len() <= 20);
        assert_eq!(
            persistence
                .load_cycle_samples("cycle-bounded")
                .expect("load raw cycle samples")
                .len(),
            100
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn cycle_csv_rejects_non_increasing_sequences() {
        let directory = temporary_directory("cycle-sequence-order");
        let mut persistence = Persistence::new(&directory).expect("create persistence");
        persistence
            .begin_cycle(
                "cycle-sequences".to_owned(),
                None,
                cycle_recipe(vec![device_step()], 1),
                None,
                "2026-01-01T00:00:00Z".to_owned(),
            )
            .expect("begin cycle");
        persistence
            .append_cycle_sample(&numbered_cycle_sample("cycle-sequences", 1))
            .expect("append first sample");
        persistence
            .append_cycle_sample(&numbered_cycle_sample("cycle-sequences", 1))
            .expect("append duplicate sample");
        persistence
            .flush_cycle_samples()
            .expect("flush cycle samples");

        assert_eq!(
            persistence.load_cycle_samples("cycle-sequences"),
            Err("cycle telemetry sequence is not strictly increasing".to_owned())
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn live_export_is_a_durable_prefix_and_does_not_hold_actor() {
        let (actor, directory) = mock_actor("streaming-export");
        let (actor_tx, actor_rx) = std_mpsc::channel();
        let actor_thread = thread::spawn(move || actor.run(&actor_rx));
        for _ in 0..50 {
            let ActorResponse::Snapshot(snapshot) =
                send_actor_request(&actor_tx, ActorRequest::Snapshot).expect("status")
            else {
                panic!("unexpected status response");
            };
            if snapshot.device.activity_known {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        send_actor_request(
            &actor_tx,
            ActorRequest::Command(ApiCommand::Start(test_config())),
        )
        .expect("start test");
        thread::sleep(Duration::from_millis(1100));

        let ActorResponse::Export(export) =
            send_actor_request(&actor_tx, ActorRequest::HistoryCsv).expect("open live export")
        else {
            panic!("unexpected export response");
        };
        let captured_length = export.length;
        let request_started = Instant::now();
        send_actor_request(&actor_tx, ActorRequest::Snapshot)
            .expect("actor remains responsive while export is unread");
        assert!(request_started.elapsed() < Duration::from_secs(1));
        thread::sleep(Duration::from_millis(1100));
        let exported = read_export(export);
        assert_eq!(
            u64::try_from(exported.len()).expect("length"),
            captured_length
        );
        assert!(exported.ends_with('\n'));
        let ActorResponse::Export(later) =
            send_actor_request(&actor_tx, ActorRequest::HistoryCsv).expect("later export")
        else {
            panic!("unexpected export response");
        };
        assert!(later.length > captured_length);

        send_actor_request(&actor_tx, ActorRequest::Shutdown).expect("shutdown actor");
        drop(actor_tx);
        actor_thread.join().expect("join actor");
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the test verifies one complete multi-run cycle sequence"
    )]
    fn no_client_cycle_settles_rests_repeats_and_archives_each_device_step() {
        let (mut actor, directory) = mock_actor("cycle-no-client");
        confirm_inactive(&mut actor);
        actor
            .start_cycle(StartCycleRequest {
                recipe: cycle_recipe(
                    vec![
                        device_step(),
                        crate::core::CycleStep::Rest {
                            duration_seconds: 1,
                        },
                        device_step(),
                    ],
                    2,
                ),
                name: Some("completed cycle".to_owned()),
            })
            .expect("start cycle");
        assert_eq!(actor.cycle.status().state, CycleState::StartingStep);
        assert!(
            actor
                .handle_command(ApiCommand::Start(test_config()))
                .is_err()
        );
        assert!(
            actor
                .start_cycle(unnamed_cycle(cycle_recipe(vec![device_step()], 1)))
                .is_err()
        );

        for run in 0_u16..4 {
            let base = run.saturating_mul(10);
            actor.record_report(
                device::DeviceMode::DischargeConstantCurrent,
                4000,
                1000,
                base + 1,
                ReportState::Active,
                "EBC-MOCK",
                None,
            );
            assert_eq!(actor.cycle.status().state, CycleState::RunningStep);
            actor.record_report(
                device::DeviceMode::DischargeConstantCurrent,
                3900,
                1000,
                base + 2,
                ReportState::Finished,
                "EBC-MOCK",
                None,
            );
            assert_eq!(actor.cycle.status().state, CycleState::Settling);
            actor.record_report(
                device::DeviceMode::DischargeConstantCurrent,
                3900,
                1000,
                base + 2,
                ReportState::Idle,
                "EBC-MOCK",
                None,
            );
            assert_eq!(actor.cycle.status().state, CycleState::Settling);
            actor.record_report(
                device::DeviceMode::DischargeConstantCurrent,
                3900,
                0,
                base + 2,
                ReportState::Idle,
                "EBC-MOCK",
                None,
            );

            if run.is_multiple_of(2) {
                assert_eq!(actor.cycle.status().state, CycleState::Resting);
                actor.record_report(
                    device::DeviceMode::DischargeConstantCurrent,
                    3950 + run,
                    0,
                    base + 2,
                    ReportState::Idle,
                    "EBC-MOCK",
                    None,
                );
                assert_eq!(actor.cycle.status().state, CycleState::Resting);
                let action = actor
                    .cycle
                    .tick(Instant::now() + Duration::from_secs(1))
                    .expect("rest advances to device step");
                actor.execute_cycle_action(action).expect("start next step");
                assert_eq!(actor.cycle.status().state, CycleState::StartingStep);
            } else if run < 3 {
                assert_eq!(actor.cycle.status().state, CycleState::StartingStep);
            }
        }

        assert_eq!(actor.cycle.status().state, CycleState::Completed);
        assert_eq!(
            actor.current_snapshot().cycle.name.as_deref(),
            Some("completed cycle")
        );
        let starts = actor
            .sent_frames
            .iter()
            .filter(|frame| {
                matches!(
                    frame,
                    OutboundFrame::StartConstantCurrentDischarge(..)
                        | OutboundFrame::StartConstantPowerDischarge(..)
                        | OutboundFrame::StartConstantVoltageCharge(..)
                )
            })
            .count();
        assert_eq!(starts, 4);
        assert_eq!(actor.persistence.run_summaries().len(), 4);
        let contexts: BTreeSet<_> = actor
            .persistence
            .run_summaries()
            .iter()
            .map(|summary| {
                let context = summary.cycle.as_ref().expect("cycle run context");
                (context.repeat_index, context.step_index)
            })
            .collect();
        assert_eq!(contexts, BTreeSet::from([(0, 0), (0, 2), (1, 0), (1, 2)]));
        assert!(
            actor
                .persistence
                .run_summaries()
                .iter()
                .all(|summary| summary.sample_count == 1 && summary.name.is_none())
        );

        let execution_id = actor
            .cycle
            .status()
            .execution_id
            .clone()
            .expect("execution id");
        let history = actor.snapshot.cycle_history.clone();
        assert_eq!(history.len(), 18);
        assert_eq!(
            history
                .iter()
                .map(|sample| sample.sequence)
                .collect::<Vec<_>>(),
            (0..18).collect::<Vec<_>>()
        );
        assert!(
            history.windows(2).all(|samples| {
                samples[1].elapsed_milliseconds >= samples[0].elapsed_milliseconds
            })
        );
        assert!(history.iter().any(|sample| {
            sample.cycle_state == CycleState::Resting
                && sample.step_index == 1
                && sample.voltage_mv >= 3950
                && sample.current_ma == 0
        }));
        assert!(history.iter().any(|sample| {
            sample.cycle_state == CycleState::Settling
                && !sample.active
                && sample.current_ma == 1000
        }));
        assert!(history.iter().any(|sample| {
            sample.cycle_state == CycleState::Settling && !sample.active && sample.current_ma == 0
        }));
        let boundary = history
            .iter()
            .find(|sample| {
                sample.repeat_index == 0
                    && sample.step_index == 2
                    && sample.cycle_state == CycleState::Settling
                    && sample.current_ma == 0
            })
            .expect("repeat boundary sample");
        let next = history
            .iter()
            .find(|sample| sample.sequence == boundary.sequence + 1)
            .expect("next repeat sample");
        assert_eq!((next.repeat_index, next.step_index), (1, 0));
        assert_eq!(next.cycle_state, CycleState::StartingStep);
        let exported = read_export(
            actor
                .persistence
                .cycle_export(None)
                .expect("export complete cycle"),
        );
        assert_eq!(exported.lines().count(), 19);
        assert!(actor.persistence.cycle_path(&execution_id).is_file());

        actor.shutdown().expect("flush cycle telemetry");
        drop(actor);
        let (snapshot_tx, _) = broadcast::channel(4);
        let config = ServerConfig {
            http_addr: "127.0.0.1:0".parse().expect("address"),
            serial_port: "/dev/null".to_owned(),
            data_dir: directory.clone(),
            mock: true,
            static_dir: directory.clone(),
        };
        let mut restarted = DeviceActor::new(config, snapshot_tx).expect("restart actor");
        assert_eq!(restarted.cycle.status().state, CycleState::Completed);
        assert_eq!(
            restarted.current_snapshot().cycle.name.as_deref(),
            Some("completed cycle")
        );
        assert_eq!(restarted.snapshot.cycle_history, history);
        confirm_inactive(&mut restarted);
        restarted
            .start_cycle(unnamed_cycle(cycle_recipe(vec![device_step()], 1)))
            .expect("start later cycle");
        assert!(restarted.persistence.cycle_path(&execution_id).is_file());
        assert_eq!(
            fs::read_dir(directory.join("cycles"))
                .expect("read cycles directory")
                .filter_map(Result::ok)
                .filter(
                    |entry| entry.path().extension().and_then(|value| value.to_str())
                        == Some("csv")
                )
                .count(),
            2
        );
        assert_eq!(
            fs::read_dir(directory.join("cycles"))
                .expect("read cycles directory")
                .filter_map(Result::ok)
                .filter(
                    |entry| entry.path().extension().and_then(|value| value.to_str())
                        == Some("json")
                )
                .count(),
            2
        );
        drop(restarted);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn firmware_reports_do_not_create_cycle_samples() {
        let (mut actor, directory) = mock_actor("cycle-firmware-filter");
        confirm_inactive(&mut actor);
        actor
            .start_cycle(unnamed_cycle(cycle_recipe(vec![device_step()], 1)))
            .expect("start cycle");

        actor.record_report_with_source(
            device::DeviceMode::DischargeConstantCurrent,
            4000,
            1000,
            1,
            ReportState::Active,
            false,
            "EBC-MOCK",
            Some("3.0.2".to_owned()),
        );
        assert!(actor.snapshot.cycle_history.is_empty());
        actor.record_report_with_source(
            device::DeviceMode::DischargeConstantCurrent,
            3990,
            1000,
            2,
            ReportState::Active,
            true,
            "EBC-MOCK",
            None,
        );
        assert_eq!(actor.snapshot.cycle_history.len(), 1);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn cycle_stop_failure_gap_and_disconnect_never_advance() {
        let (mut actor, directory) = mock_actor("cycle-stop-policy");
        confirm_inactive(&mut actor);
        actor
            .start_cycle(unnamed_cycle(cycle_recipe(
                vec![device_step(), device_step()],
                1,
            )))
            .expect("start cycle");
        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            4000,
            1000,
            1,
            ReportState::Active,
            "EBC-MOCK",
            None,
        );
        actor.stop_cycle().expect("request cycle stop");
        assert_eq!(actor.cycle.status().state, CycleState::Stopping);
        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            4000,
            1000,
            2,
            ReportState::Idle,
            "EBC-MOCK",
            None,
        );
        assert_eq!(actor.cycle.status().state, CycleState::Stopped);
        assert_eq!(actor.cycle.status().step_index, 0);

        confirm_inactive(&mut actor);
        actor
            .start_cycle(unnamed_cycle(cycle_recipe(vec![device_step()], 1)))
            .expect("second cycle");
        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            4000,
            1000,
            3,
            ReportState::Active,
            "EBC-MOCK",
            None,
        );
        actor.set_connection_error("injected read failure");
        assert_eq!(actor.cycle.status().state, CycleState::Interrupted);

        confirm_inactive(&mut actor);
        actor
            .start_cycle(unnamed_cycle(cycle_recipe(vec![device_step()], 1)))
            .expect("third cycle");
        actor
            .handle_command(ApiCommand::Disconnect)
            .expect("safe disconnect");
        assert_eq!(actor.cycle.status().state, CycleState::Interrupted);
        assert!(matches!(
            actor.sent_frames.as_slice(),
            [.., OutboundFrame::Stop, OutboundFrame::Disconnect]
        ));
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn interrupted_safety_stop_retries_after_server_reconnect() {
        let (mut actor, directory) = mock_actor("cycle-stop-retry");
        confirm_inactive(&mut actor);
        actor
            .start_cycle(unnamed_cycle(cycle_recipe(vec![device_step()], 1)))
            .expect("start cycle");
        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            4000,
            1000,
            1,
            ReportState::Active,
            "EBC-MOCK",
            None,
        );
        actor.set_connection_error("serial observation gap");
        actor.connect().expect("reconnect mock device before stop");
        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            4000,
            1000,
            2,
            ReportState::Active,
            "EBC-MOCK",
            None,
        );
        inject_write_failure(&mut actor);

        actor.stop_cycle().expect_err("first safety stop fails");
        assert_eq!(actor.cycle.status().state, CycleState::Interrupted);

        actor.connect().expect("reconnect mock device");
        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            4000,
            1000,
            3,
            ReportState::Active,
            "EBC-MOCK",
            None,
        );
        actor.stop_cycle().expect("retry safety stop");
        actor.stop_cycle().expect("deduplicated safety stop");

        let stop_frames = actor
            .sent_frames
            .iter()
            .filter(|frame| matches!(frame, OutboundFrame::Stop))
            .count();
        assert_eq!(stop_frames, 2);
        assert_eq!(actor.cycle.status().state, CycleState::Interrupted);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn server_restart_interrupts_persisted_rest_and_start_failure_is_terminal() {
        let (mut actor, directory) = mock_actor("cycle-restart");
        confirm_inactive(&mut actor);
        actor
            .start_cycle(unnamed_cycle(cycle_recipe(
                vec![crate::core::CycleStep::Rest {
                    duration_seconds: 60,
                }],
                1,
            )))
            .expect("start rest cycle");
        actor.persist_and_publish().expect("persist rest");
        drop(actor);

        let (snapshot_tx, _) = broadcast::channel(4);
        let config = ServerConfig {
            http_addr: "127.0.0.1:0".parse().expect("address"),
            serial_port: "/dev/null".to_owned(),
            data_dir: directory.clone(),
            mock: true,
            static_dir: directory.clone(),
        };
        let mut restarted = DeviceActor::new(config, snapshot_tx).expect("restart actor");
        assert_eq!(restarted.cycle.status().state, CycleState::Interrupted);
        assert_eq!(restarted.cycle.status().step_index, 0);
        assert!(restarted.sent_frames.is_empty());

        confirm_inactive(&mut restarted);
        inject_write_failure(&mut restarted);
        assert!(
            restarted
                .start_cycle(unnamed_cycle(cycle_recipe(vec![device_step()], 1)))
                .is_err()
        );
        assert_eq!(restarted.cycle.status().state, CycleState::Interrupted);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn server_restart_never_resumes_an_active_cycle_step() {
        let (mut actor, directory) = mock_actor("cycle-active-restart");
        confirm_inactive(&mut actor);
        actor
            .start_cycle(unnamed_cycle(cycle_recipe(
                vec![device_step(), device_step()],
                2,
            )))
            .expect("start cycle");
        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            4000,
            1000,
            1,
            ReportState::Active,
            "EBC-MOCK",
            None,
        );
        actor.persist_and_publish().expect("persist active cycle");
        let execution_id = actor
            .cycle
            .status()
            .execution_id
            .clone()
            .expect("execution id");
        drop(actor);

        let (snapshot_tx, _) = broadcast::channel(4);
        let config = ServerConfig {
            http_addr: "127.0.0.1:0".parse().expect("address"),
            serial_port: "/dev/null".to_owned(),
            data_dir: directory.clone(),
            mock: true,
            static_dir: directory.clone(),
        };
        let restarted = DeviceActor::new(config, snapshot_tx).expect("restart actor");
        assert_eq!(restarted.cycle.status().state, CycleState::Interrupted);
        assert_eq!(
            restarted.cycle.status().execution_id.as_deref(),
            Some(execution_id.as_str())
        );
        assert_eq!(restarted.cycle.status().repeat_index, 0);
        assert_eq!(restarted.cycle.status().step_index, 0);
        assert_eq!(
            restarted.controller.test().state,
            TestState::RecoveredUncertain
        );
        assert_eq!(restarted.snapshot.cycle_history.len(), 1);
        assert_eq!(
            restarted.snapshot.cycle_history[0].execution_id,
            execution_id
        );
        assert!(restarted.persistence.cycle_path(&execution_id).is_file());
        assert!(restarted.sent_frames.is_empty());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn legacy_metadata_loads_without_names() {
        let directory = temporary_directory("legacy-name-metadata");
        let persistence = Persistence::new(&directory).expect("create persistence");
        persistence
            .save_metadata(&AuthoritativeSnapshot::default())
            .expect("save metadata");
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(&persistence.metadata_path).expect("read metadata"))
                .expect("parse metadata");
        metadata
            .as_object_mut()
            .expect("metadata object")
            .remove("current_run_name");
        metadata["cycle"]
            .as_object_mut()
            .expect("cycle object")
            .remove("name");
        fs::write(
            &persistence.metadata_path,
            serde_json::to_vec_pretty(&metadata).expect("serialize legacy metadata"),
        )
        .expect("write legacy metadata");
        drop(persistence);

        let mut restarted = Persistence::new(&directory).expect("restart persistence");
        let snapshot = restarted.load().expect("load legacy metadata");
        assert_eq!(snapshot.current_run.name, None);
        assert_eq!(snapshot.cycle.name, None);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one lifecycle test verifies one durable run"
    )]
    fn manual_run_names_archive_rename_clear_and_restart_without_touching_csv() {
        let (mut actor, directory) = mock_actor("manual-run-names");
        confirm_inactive(&mut actor);
        actor
            .start_test(test_config(), Some("  first run  ".to_owned()), None)
            .expect("start named run");
        let first_current_id = actor.persistence.current_run_id.clone();
        assert_eq!(
            actor.current_snapshot().current_run.name.as_deref(),
            Some("first run")
        );
        assert_eq!(
            actor.current_snapshot().current_run.id.as_deref(),
            Some(first_current_id.as_str())
        );
        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            4000,
            1000,
            1,
            ReportState::Active,
            "EBC-MOCK",
            None,
        );
        actor.stop_test().expect("stop first run");
        actor.record_report(
            device::DeviceMode::DischargeConstantCurrent,
            4000,
            0,
            2,
            ReportState::Idle,
            "EBC-MOCK",
            None,
        );
        let archive_snapshot = actor.current_snapshot();
        let archived_while_current = actor
            .persistence
            .archive_current(&archive_snapshot)
            .expect("archive current run")
            .expect("current archive");
        assert_eq!(archived_while_current.id, first_current_id);
        actor.sent_frames.clear();
        actor
            .rename_run(
                &first_current_id,
                RenameRequest {
                    name: Some("first archived".to_owned()),
                },
            )
            .expect("rename current archived run");
        assert!(actor.sent_frames.is_empty());
        assert_eq!(
            actor
                .persistence
                .run_summaries()
                .into_iter()
                .find(|summary| summary.id == first_current_id)
                .expect("current archived summary")
                .name
                .as_deref(),
            Some("first archived")
        );
        actor
            .start_test(test_config(), None, None)
            .expect("start unnamed run");
        assert_eq!(actor.current_snapshot().current_run.name, None);

        let archived = actor
            .persistence
            .run_summaries()
            .into_iter()
            .find(|summary| summary.name.as_deref() == Some("first archived"))
            .expect("named archive");
        let archived_id = archived.id.clone();
        let csv_path = actor
            .persistence
            .runs_dir
            .join(format!("{archived_id}.csv"));
        let csv_before = fs::read(&csv_path).expect("read archived CSV");
        actor.sent_frames.clear();
        actor
            .rename_run(
                &archived_id,
                RenameRequest {
                    name: Some("  renamed archive  ".to_owned()),
                },
            )
            .expect("rename archived run");
        assert!(actor.sent_frames.is_empty());
        assert_eq!(
            fs::read(&csv_path).expect("reread archived CSV"),
            csv_before
        );
        let renamed = actor
            .persistence
            .run_summaries()
            .into_iter()
            .find(|summary| summary.id == archived_id)
            .expect("renamed summary");
        assert_eq!(renamed.name.as_deref(), Some("renamed archive"));
        assert_eq!(renamed.id, archived.id);
        actor
            .rename_run(&archived_id, RenameRequest::default())
            .expect("clear archived name");
        assert_eq!(
            actor
                .persistence
                .run_summaries()
                .into_iter()
                .find(|summary| summary.id == archived_id)
                .expect("cleared summary")
                .name,
            None
        );

        let second_id = actor.persistence.current_run_id.clone();
        actor
            .rename_run(
                &second_id,
                RenameRequest {
                    name: Some("second run".to_owned()),
                },
            )
            .expect("name current run");
        actor.shutdown().expect("persist current name");
        drop(actor);
        let (snapshot_tx, _) = broadcast::channel(4);
        let config = ServerConfig {
            http_addr: "127.0.0.1:0".parse().expect("address"),
            serial_port: "/dev/null".to_owned(),
            data_dir: directory.clone(),
            mock: true,
            static_dir: directory.clone(),
        };
        let mut restarted = DeviceActor::new(config, snapshot_tx).expect("restart actor");
        assert_eq!(
            restarted.current_snapshot().current_run.name.as_deref(),
            Some("second run")
        );
        assert_eq!(restarted.persistence.current_run_id, second_id);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn cycle_names_use_sidecars_survive_restart_and_never_name_child_runs() {
        let (mut actor, directory) = mock_actor("cycle-names");
        confirm_inactive(&mut actor);
        actor
            .start_cycle(StartCycleRequest {
                recipe: cycle_recipe(vec![device_step()], 1),
                name: Some("  formation  ".to_owned()),
            })
            .expect("start named cycle");
        let execution_id = actor
            .cycle
            .status()
            .execution_id
            .clone()
            .expect("execution id");
        let child_id = actor.persistence.current_run_id.clone();
        assert_eq!(
            actor.current_snapshot().cycle.name.as_deref(),
            Some("formation")
        );
        assert_eq!(actor.current_snapshot().current_run.name, None);
        assert!(actor.current_snapshot().current_run.cycle.is_some());
        let metadata = actor
            .persistence
            .load_cycle_metadata(&execution_id)
            .expect("load sidecar")
            .expect("sidecar exists");
        assert_eq!(metadata.name.as_deref(), Some("formation"));

        let cycle_csv = actor.persistence.cycle_path(&execution_id);
        let csv_before = fs::read(&cycle_csv).expect("read cycle CSV");
        actor.sent_frames.clear();
        actor
            .rename_cycle(
                &execution_id,
                RenameRequest {
                    name: Some("renamed cycle".to_owned()),
                },
            )
            .expect("rename cycle");
        assert!(actor.sent_frames.is_empty());
        assert_eq!(fs::read(&cycle_csv).expect("reread cycle CSV"), csv_before);
        assert_eq!(
            actor.current_snapshot().cycle.name.as_deref(),
            Some("renamed cycle")
        );
        assert!(matches!(
            actor.rename_run(&child_id, RenameRequest::default()),
            Err(RenameError::BadRequest(message)) if message.contains("cycle child")
        ));
        assert!(matches!(
            actor.rename_cycle("unknown-cycle", RenameRequest::default()),
            Err(RenameError::NotFound(_))
        ));
        assert!(matches!(
            actor.rename_run("unknown-run", RenameRequest::default()),
            Err(RenameError::NotFound(_))
        ));

        actor.persist_and_publish().expect("persist cycle name");
        drop(actor);
        let (snapshot_tx, _) = broadcast::channel(4);
        let config = ServerConfig {
            http_addr: "127.0.0.1:0".parse().expect("address"),
            serial_port: "/dev/null".to_owned(),
            data_dir: directory.clone(),
            mock: true,
            static_dir: directory.clone(),
        };
        let mut restarted = DeviceActor::new(config, snapshot_tx).expect("restart actor");
        assert_eq!(
            restarted.current_snapshot().cycle.name.as_deref(),
            Some("renamed cycle")
        );
        assert_eq!(
            fs::read(&cycle_csv).expect("read old cycle CSV"),
            csv_before
        );
        confirm_inactive(&mut restarted);
        restarted
            .start_cycle(StartCycleRequest {
                recipe: cycle_recipe(
                    vec![crate::core::CycleStep::Rest {
                        duration_seconds: 1,
                    }],
                    1,
                ),
                name: None,
            })
            .expect("start second cycle");
        assert_eq!(restarted.current_snapshot().cycle.name, None);
        assert!(cycle_csv.is_file());
        assert_eq!(
            restarted
                .persistence
                .load_cycle_metadata(&execution_id)
                .expect("load old sidecar")
                .expect("old sidecar")
                .name
                .as_deref(),
            Some("renamed cycle")
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one persistence scenario covers ordering, conflicts, deletion, and restart identity"
    )]
    fn saved_recipes_persist_sort_conflict_delete_and_reserve_ids() {
        let (mut actor, directory) = mock_actor("saved-recipes");
        let mut events = actor.snapshot_tx.subscribe();
        let beta = actor
            .create_saved_recipe(saved_recipe_request("  beta  "))
            .expect("create beta");
        let upper = actor
            .create_saved_recipe(saved_recipe_request("Alpha"))
            .expect("create upper alpha");
        let lower = actor
            .create_saved_recipe(saved_recipe_request("alpha"))
            .expect("create lower alpha");
        assert!(matches!(
            events.try_recv().expect("recipe event"),
            WebSocketEvent::RecipeUpsert(recipe) if recipe.id == beta.id
        ));

        let recipes = actor.persistence.saved_recipes();
        assert_eq!(recipes[2].id, beta.id);
        let mut alpha_ids = vec![upper.id.clone(), lower.id.clone()];
        alpha_ids.sort();
        assert_eq!(
            recipes[..2]
                .iter()
                .map(|recipe| recipe.id.clone())
                .collect::<Vec<_>>(),
            alpha_ids
        );
        let (response, receiver) = oneshot::channel();
        actor.handle_message(ActorMessage {
            request: ActorRequest::Subscribe,
            response,
        });
        let ActorResponse::Subscription(_, library, _) = receiver
            .blocking_recv()
            .expect("subscription response")
            .expect("subscription")
        else {
            panic!("unexpected subscription response");
        };
        assert_eq!(library, recipes);
        let library_json = serde_json::to_value(WebSocketEvent::RecipeLibrary(library))
            .expect("serialize recipe library event");
        assert_eq!(library_json["event"], "recipe_library");

        let upper_path = actor.persistence.recipe_path(&upper.id);
        let upper_before_conflict = fs::read(&upper_path).expect("read recipe before conflict");
        assert!(matches!(
            actor.update_saved_recipe(
                &upper.id,
                UpdateSavedRecipeRequest {
                    name: "changed".to_owned(),
                    recipe: upper.recipe.clone(),
                    expected_revision: 99,
                },
            ),
            Err(RecipeError::Conflict(_))
        ));
        assert_eq!(
            fs::read(&upper_path).expect("read recipe after update conflict"),
            upper_before_conflict
        );
        let updated = actor
            .update_saved_recipe(
                &upper.id,
                UpdateSavedRecipeRequest {
                    name: "  Changed  ".to_owned(),
                    recipe: upper.recipe.clone(),
                    expected_revision: upper.revision,
                },
            )
            .expect("update recipe");
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.name, "Changed");
        assert_eq!(updated.created_at_utc, upper.created_at_utc);
        assert_ne!(updated.updated_at_utc, upper.updated_at_utc);
        assert!(matches!(
            actor.delete_saved_recipe(
                &updated.id,
                DeleteSavedRecipeRequest {
                    expected_revision: 1,
                },
            ),
            Err(RecipeError::Conflict(_))
        ));
        assert_eq!(
            fs::read(actor.persistence.recipe_path(&updated.id))
                .expect("read recipe after delete conflict"),
            serde_json::to_vec_pretty(&updated)
                .expect("serialize updated recipe")
                .into_iter()
                .chain(std::iter::once(b'\n'))
                .collect::<Vec<_>>()
        );
        let deleted = actor
            .delete_saved_recipe(
                &updated.id,
                DeleteSavedRecipeRequest {
                    expected_revision: updated.revision,
                },
            )
            .expect("delete recipe");
        assert!(!actor.persistence.recipe_path(&deleted.id).exists());
        assert!(
            actor
                .persistence
                .recipes_dir
                .join(format!("{}.deleted", deleted.id))
                .is_file()
        );
        drop(actor);

        let mut restarted = Persistence::new(&directory).expect("restart recipe persistence");
        assert_eq!(restarted.saved_recipes().len(), 2);
        assert!(restarted.reserved_recipe_ids.contains(&deleted.id));
        let fixed = "2026-01-01T00:00:00+00:00";
        let base = restarted.next_recipe_id(fixed);
        restarted.reserved_recipe_ids.insert(base.clone());
        assert_eq!(restarted.next_recipe_id(fixed), format!("{base}-2"));
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn recipe_import_export_and_strict_startup_validation() {
        let (mut actor, directory) = mock_actor("recipe-import");
        let samples_before = fs::read(&actor.persistence.samples_path).unwrap_or_default();
        let export = RecipeExport {
            format: RECIPE_EXPORT_FORMAT.to_owned(),
            version: RECIPE_EXPORT_VERSION,
            name: "  Portable  ".to_owned(),
            recipe: saved_recipe_request("ignored").recipe,
        };
        let imported = actor
            .import_saved_recipe(export)
            .expect("import valid recipe");
        assert_eq!(imported.name, "Portable");
        let persisted_before_export = fs::read(actor.persistence.recipe_path(&imported.id))
            .expect("read imported persistence");
        let exported = actor
            .export_saved_recipe(&imported.id)
            .expect("export recipe");
        exported.validate().expect("valid exported envelope");
        assert_eq!(exported.name, imported.name);
        assert_eq!(exported.recipe, imported.recipe);
        assert_eq!(
            fs::read(actor.persistence.recipe_path(&imported.id))
                .expect("read persistence after export"),
            persisted_before_export
        );
        assert_eq!(
            actor
                .persistence
                .saved_recipe(&imported.id)
                .expect("imported recipe"),
            &imported
        );
        let duplicate_a = actor
            .import_saved_recipe(exported.clone())
            .expect("duplicate import A");
        let duplicate_b = actor
            .import_saved_recipe(exported.clone())
            .expect("duplicate import B");
        assert_ne!(imported.id, duplicate_a.id);
        assert_ne!(duplicate_a.id, duplicate_b.id);
        assert_eq!(duplicate_a.revision, 1);
        assert_eq!(duplicate_b.revision, 1);
        assert_eq!(duplicate_a.name, duplicate_b.name);
        assert_eq!(duplicate_a.recipe, duplicate_b.recipe);
        assert!(actor.sent_frames.is_empty());
        assert_eq!(
            fs::read(&actor.persistence.samples_path).unwrap_or_default(),
            samples_before
        );
        assert!(matches!(
            actor.import_saved_recipe(RecipeExport {
                format: "other".to_owned(),
                ..exported
            }),
            Err(RecipeError::BadRequest(_))
        ));
        drop(actor);

        let invalid_path = directory.join("recipes").join("invalid.json");
        fs::write(&invalid_path, b"{}\n").expect("write invalid recipe");
        assert!(Persistence::new(&directory).is_err());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one lifecycle scenario proves immutable provenance across edit, delete, rename, and restart"
    )]
    fn saved_start_provenance_survives_edit_delete_rename_and_restart() {
        let (mut actor, directory) = mock_actor("saved-provenance");
        let saved = actor
            .create_saved_recipe(saved_recipe_request("Formation"))
            .expect("create recipe");
        confirm_inactive(&mut actor);
        actor.sent_frames.clear();
        actor
            .start_saved_recipe(
                &saved.id,
                StartSavedRecipeRequest {
                    execution_name: Some("  Cell 7  ".to_owned()),
                },
            )
            .expect("start saved recipe");
        assert!(actor.sent_frames.is_empty(), "a first rest sends no frame");
        let status = actor.current_snapshot().cycle;
        let reference = status.saved_recipe.clone().expect("saved provenance");
        assert_eq!(reference.id, saved.id);
        assert_eq!(reference.name, saved.name);
        assert_eq!(reference.revision, saved.revision);
        assert_eq!(status.name.as_deref(), Some("Cell 7"));
        let execution_id = status.execution_id.clone().expect("execution id");

        let recipe_b = cycle_recipe(
            vec![crate::core::CycleStep::Rest {
                duration_seconds: 120,
            }],
            2,
        );
        let updated = actor
            .update_saved_recipe(
                &saved.id,
                UpdateSavedRecipeRequest {
                    name: "Formation v2".to_owned(),
                    recipe: recipe_b.clone(),
                    expected_revision: saved.revision,
                },
            )
            .expect("edit executing recipe");
        assert_eq!(updated.revision, 2);
        assert_eq!(actor.current_snapshot().cycle.recipe, status.recipe);
        assert_eq!(
            actor.current_snapshot().cycle.saved_recipe,
            Some(reference.clone())
        );

        actor.stop_cycle().expect("stop first execution");
        actor
            .start_saved_recipe(
                &saved.id,
                StartSavedRecipeRequest {
                    execution_name: None,
                },
            )
            .expect("start updated recipe");
        let updated_status = actor.current_snapshot().cycle;
        let updated_reference = SavedRecipeReference {
            id: updated.id.clone(),
            name: updated.name.clone(),
            revision: updated.revision,
        };
        assert_eq!(updated_status.recipe.as_ref(), Some(&recipe_b));
        assert_eq!(
            updated_status.saved_recipe.as_ref(),
            Some(&updated_reference)
        );
        let updated_execution_id = updated_status
            .execution_id
            .clone()
            .expect("updated execution id");
        let original_sidecar = actor
            .persistence
            .load_cycle_metadata(&execution_id)
            .expect("load original sidecar")
            .expect("original sidecar");
        assert_eq!(original_sidecar.recipe, status.recipe);
        assert_eq!(original_sidecar.saved_recipe, Some(reference.clone()));

        let state_before_delete = updated_status.state;
        actor
            .delete_saved_recipe(
                &updated.id,
                DeleteSavedRecipeRequest {
                    expected_revision: updated.revision,
                },
            )
            .expect("delete executing recipe");
        let after_delete = actor.current_snapshot().cycle;
        assert_eq!(after_delete.state, state_before_delete);
        assert_eq!(after_delete.recipe.as_ref(), Some(&recipe_b));
        assert_eq!(after_delete.saved_recipe.as_ref(), Some(&updated_reference));
        assert!(matches!(
            actor.start_saved_recipe(
                &updated.id,
                StartSavedRecipeRequest {
                    execution_name: None,
                },
            ),
            Err(StartError::NotFound(_))
        ));
        actor
            .stop_cycle()
            .expect("deleted template execution remains stoppable");
        assert_eq!(actor.current_snapshot().cycle.state, CycleState::Stopped);
        actor
            .rename_cycle(
                &updated_execution_id,
                RenameRequest {
                    name: Some("Renamed execution".to_owned()),
                },
            )
            .expect("rename execution");
        let sidecar = actor
            .persistence
            .load_cycle_metadata(&updated_execution_id)
            .expect("load sidecar")
            .expect("sidecar");
        assert_eq!(sidecar.recipe, Some(recipe_b));
        assert_eq!(sidecar.saved_recipe, Some(updated_reference.clone()));
        assert_eq!(sidecar.started_at_utc, updated_status.started_at_utc);
        assert_eq!(sidecar.name.as_deref(), Some("Renamed execution"));
        assert_eq!(
            original_sidecar,
            actor
                .persistence
                .load_cycle_metadata(&execution_id)
                .expect("reload original sidecar")
                .expect("original sidecar")
        );
        actor.shutdown().expect("persist actor");
        drop(actor);

        let (snapshot_tx, _) = broadcast::channel(4);
        let config = ServerConfig {
            http_addr: "127.0.0.1:0".parse().expect("address"),
            serial_port: "/dev/null".to_owned(),
            data_dir: directory.clone(),
            mock: true,
            static_dir: directory.clone(),
        };
        let mut restarted = DeviceActor::new(config, snapshot_tx).expect("restart actor");
        let recovered = restarted.current_snapshot().cycle;
        assert_eq!(recovered.saved_recipe, Some(updated_reference));
        assert_eq!(recovered.recipe, sidecar.recipe);
        assert_eq!(recovered.name.as_deref(), Some("Renamed execution"));
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn legacy_cycle_sidecar_loads_and_ad_hoc_cycle_has_no_saved_provenance() {
        let (mut actor, directory) = mock_actor("legacy-cycle-sidecar");
        confirm_inactive(&mut actor);
        actor
            .start_cycle(unnamed_cycle(cycle_recipe(
                vec![crate::core::CycleStep::Rest {
                    duration_seconds: 60,
                }],
                1,
            )))
            .expect("start ad-hoc cycle");
        assert_eq!(actor.current_snapshot().cycle.saved_recipe, None);
        let execution_id = actor
            .cycle
            .status()
            .execution_id
            .clone()
            .expect("execution id");
        let path = actor.persistence.cycle_metadata_path(&execution_id);
        fs::write(
            &path,
            format!("{{\"execution_id\":\"{execution_id}\",\"name\":\"Legacy\"}}\n"),
        )
        .expect("write legacy sidecar");
        let legacy = actor
            .persistence
            .load_cycle_metadata(&execution_id)
            .expect("load legacy sidecar")
            .expect("legacy sidecar");
        assert_eq!(legacy.name.as_deref(), Some("Legacy"));
        assert_eq!(legacy.recipe, None);
        assert_eq!(legacy.saved_recipe, None);
        assert_eq!(legacy.started_at_utc, None);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn mutation_header_and_origin_policy_is_strict() {
        let (actor_tx, _actor_rx) = std_mpsc::channel();
        let mut state = AppState {
            actor_tx,
            allowed_origin: None,
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "tester.local".parse().expect("host"));
        assert!(validate_mutation(&headers, &state).is_err());
        headers.insert("x-ebc-command", "1".parse().expect("command header"));
        assert!(validate_mutation(&headers, &state).is_ok());
        headers.insert(header::ORIGIN, "null".parse().expect("origin"));
        assert!(validate_mutation(&headers, &state).is_err());
        headers.insert(
            header::ORIGIN,
            "http://other.local".parse().expect("origin"),
        );
        assert!(validate_mutation(&headers, &state).is_err());
        headers.insert(
            header::ORIGIN,
            "http://tester.local".parse().expect("origin"),
        );
        assert!(validate_mutation(&headers, &state).is_ok());
        headers.insert(
            header::ORIGIN,
            "https://tester.local".parse().expect("HTTPS origin"),
        );
        assert!(validate_mutation(&headers, &state).is_ok());
        assert!(validate_origin(&HeaderMap::new(), &state, true).is_err());

        state.allowed_origin = Some("https://battery.example".to_owned());
        headers.insert(
            header::ORIGIN,
            "https://battery.example".parse().expect("origin"),
        );
        assert!(validate_mutation(&headers, &state).is_ok());
    }
}
