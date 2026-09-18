//! Native headless server and the single-owner device actor.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read as _, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path as AxumPath, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
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
use crate::core::{
    ApiCommand, AuthoritativeSnapshot, CalibrationCommand, RunSummary, Sample,
    ServerConnectionState, SnapshotUpdate, TestConfiguration, TestState, WebSocketEvent,
};
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
    snapshot_tx: broadcast::Sender<WebSocketEvent>,
    allowed_origin: Option<String>,
}

enum ActorRequest {
    Snapshot,
    CompleteSnapshot,
    History,
    HistoryCsv,
    Runs,
    RunCsv(String),
    Command(ApiCommand),
    Shutdown,
}

enum ActorResponse {
    Snapshot(AuthoritativeSnapshot),
    History(Vec<Sample>),
    Runs(Vec<RunSummary>),
    Export(ExportDescriptor),
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
    next_sequence: u64,
    raw_sample_count: usize,
    sample_writer: Option<BufWriter<File>>,
    last_sample_sync: Instant,
}

#[derive(Serialize, Deserialize)]
struct Metadata {
    connection: ServerConnectionState,
    connection_error: Option<String>,
    device: crate::core::DeviceState,
    test: crate::core::TestStatus,
    #[serde(default)]
    archived_run_id: Option<String>,
    #[serde(default)]
    current_run_id: String,
    #[serde(default)]
    next_sequence: u64,
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
        let mut persistence = Self {
            metadata_path: data_dir.join("session.json"),
            samples_path: data_dir.join("samples.csv"),
            runs_dir,
            runs: Vec::new(),
            archived_run_id: None,
            current_run_id: String::new(),
            next_sequence: 0,
            raw_sample_count: 0,
            sample_writer: None,
            last_sample_sync: Instant::now(),
        };
        persistence.runs = persistence.load_run_summaries()?;
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
            self.next_sequence = metadata.next_sequence;
            AuthoritativeSnapshot {
                connection: ServerConnectionState::Disconnected,
                connection_error: None,
                device: metadata.device,
                test: metadata.test,
                capabilities: Default::default(),
                history: Vec::new(),
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
        Ok(snapshot)
    }

    fn save_metadata(&self, snapshot: &AuthoritativeSnapshot) -> Result<(), String> {
        let metadata = Metadata {
            connection: snapshot.connection.clone(),
            connection_error: snapshot.connection_error.clone(),
            device: snapshot.device.clone(),
            test: snapshot.test.clone(),
            archived_run_id: self.archived_run_id.clone(),
            current_run_id: self.current_run_id.clone(),
            next_sequence: self.next_sequence,
        };
        let temporary = self.metadata_path.with_extension("json.tmp");
        let mut file = File::create(&temporary).map_err(|error| error.to_string())?;
        serde_json::to_writer_pretty(&mut file, &metadata).map_err(|error| error.to_string())?;
        file.write_all(b"\n").map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::rename(temporary, &self.metadata_path).map_err(|error| error.to_string())?;
        sync_parent(&self.metadata_path)
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

    fn begin_current_run(&mut self, run_id: String) {
        self.archived_run_id = None;
        self.current_run_id = run_id;
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
        if let Some(id) = &self.archived_run_id {
            return Ok(self.runs.iter().find(|run| &run.id == id).cloned());
        }

        self.flush_samples()?;
        let id = self.next_run_id(snapshot.test.started_at_utc.as_deref());
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
        self.runs.push(summary.clone());
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
            let summary: RunSummary = match serde_json::from_slice(&bytes) {
                Ok(summary) => summary,
                Err(error) => {
                    log::warn!("ignoring invalid run summary {}: {error}", path.display());
                    continue;
                }
            };
            if summary.id == id {
                runs.push(summary);
            }
        }
        runs.sort_by(|left, right| right.id.cmp(&left.id));
        Ok(runs)
    }

    fn run_summaries(&self) -> Vec<RunSummary> {
        self.runs.clone()
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
        if !self.samples_path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(&self.samples_path).map_err(|error| error.to_string())?;
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
                        .open(&self.samples_path)
                        .map_err(|error| error.to_string())?;
                    file.set_len(u64::try_from(length).map_err(|error| error.to_string())?)
                        .map_err(|error| error.to_string())?;
                    file.sync_all().map_err(|error| error.to_string())?;
                    sync_parent(&self.samples_path)?;
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        if !terminated && rows.last().is_some_and(|line| parse_sample(line).is_ok()) {
            let mut file = OpenOptions::new()
                .append(true)
                .open(&self.samples_path)
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

fn valid_run_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 120
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
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

struct DeviceActor {
    config: ServerConfig,
    snapshot: AuthoritativeSnapshot,
    controller: TestController,
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
    start_metadata_failure: Option<String>,
}

struct StartPreparationRollback {
    history: Vec<Sample>,
    archived_run_id: Option<String>,
    current_run_id: String,
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
        let controller = TestController::from_state(
            ControllerMode::Server,
            snapshot.device.clone(),
            snapshot.test.clone(),
            snapshot.history.last(),
        );
        if snapshot.history.len() > SNAPSHOT_SAMPLE_LIMIT {
            snapshot.history = presentation_history(&snapshot.history, SNAPSHOT_SAMPLE_LIMIT);
        }
        Ok(Self {
            config,
            snapshot,
            controller,
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
        self.persistence.save_metadata(&self.snapshot)
    }

    fn handle_message(&mut self, message: ActorMessage) {
        let result = match message.request {
            ActorRequest::Snapshot => Ok(ActorResponse::Snapshot(self.current_snapshot())),
            ActorRequest::CompleteSnapshot => {
                self.sync_controller_state();
                Ok(ActorResponse::Snapshot(self.snapshot_for_clients()))
            }
            ActorRequest::History => Ok(ActorResponse::History(presentation_history(
                &self.snapshot.history,
                SNAPSHOT_SAMPLE_LIMIT,
            ))),
            ActorRequest::HistoryCsv => self.persistence.live_export().map(ActorResponse::Export),
            ActorRequest::Runs => Ok(ActorResponse::Runs(self.persistence.run_summaries())),
            ActorRequest::RunCsv(id) => self.persistence.run_export(&id).map(ActorResponse::Export),
            ActorRequest::Command(command) => self
                .handle_command(command)
                .map(|()| ActorResponse::Snapshot(self.current_snapshot())),
            ActorRequest::Shutdown => Err("shutdown must be handled by the actor loop".to_owned()),
        };
        let _response_sent = message.response.send(result);
    }

    fn handle_command(&mut self, command: ApiCommand) -> Result<(), String> {
        match command {
            ApiCommand::Connect => {
                if let Err(error) = self.connect() {
                    self.set_connection_error(&error);
                    return Err(error);
                }
            }
            ApiCommand::Disconnect => self.disconnect()?,
            ApiCommand::Start(config) => self.start_test(config)?,
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
        if let Some(port) = &mut self.port
            && let Err(error) = write_frame(port, OutboundFrame::Disconnect)
        {
            self.handle_protocol_write_failure(None, &error);
            return Err(error);
        }
        self.port = None;
        self.serial_buffer.clear();
        self.mock_idle_report_due = None;
        self.snapshot.connection = ServerConnectionState::Disconnected;
        self.controller
            .disconnect("device disconnected; physical test state is unknown");
        self.sync_controller_state();
        Ok(())
    }

    fn start_test(&mut self, config: TestConfiguration) -> Result<(), String> {
        let prepared = self.controller.prepare_command(ApiCommand::Start(config))?;
        self.sync_controller_state();
        self.persistence.archive_current(&self.snapshot)?;
        let rollback = StartPreparationRollback {
            history: self.snapshot.history.clone(),
            archived_run_id: self.persistence.archived_run_id.clone(),
            current_run_id: self.persistence.current_run_id.clone(),
            next_sequence: self.persistence.next_sequence,
            raw_sample_count: self.persistence.raw_sample_count,
        };
        if let Err(error) = self.persistence.reset_samples() {
            return Err(self.restore_start_preparation(rollback, &error));
        }
        self.snapshot.history.clear();
        let run_id = self.persistence.new_run_id();
        self.persistence.begin_current_run(run_id);
        self.sync_controller_state();
        if let Err(error) = self.save_start_metadata() {
            return Err(self.restore_start_preparation(rollback, &error));
        }
        self.send_command_frame(prepared)?;
        self.controller
            .commit_command(prepared, Some(Utc::now().to_rfc3339()));
        self.sync_controller_state();
        if self.config.mock {
            self.controller.set_current_ma(mock_current(&config));
            self.sync_controller_state();
        }
        Ok(())
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
        if let Err(persistence_error) = self.persistence.save_metadata(&self.snapshot) {
            log::error!("failed to persist protocol write failure: {persistence_error}");
        }
        self.publish();
    }

    fn tick(&mut self) {
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
            self.record_report(mode, 4200, 0, 0, ReportState::Idle, "EBC-MOCK", None);
            return;
        }
        if self.controller.is_stopping() {
            let mode = self
                .snapshot
                .device
                .mode
                .unwrap_or(device::DeviceMode::DischargeConstantCurrent);
            self.record_report(
                mode,
                self.controller.device().voltage_mv.unwrap_or(4200),
                0,
                self.controller.device().capacity_mah.unwrap_or(0),
                ReportState::Idle,
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
        self.record_report(
            config.mode(),
            voltage,
            mock_current(&config),
            capacity,
            ReportState::Active,
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
            InboundFrame::Firmware(report) => self.record_report(
                report.device_mode,
                report.voltage_mv,
                report.current_ma,
                report.milli_ampere_hours,
                if report.in_progress {
                    ReportState::Active
                } else {
                    ReportState::InactiveUnknown
                },
                &report.device_type,
                Some(report.firmware_version),
            ),
            InboundFrame::Charge(report) => self.record_report(
                device::DeviceMode::ChargeConstantVoltage,
                report.voltage_mv,
                report.current_ma,
                report.milli_ampere_hours,
                report.state.into(),
                &report.device_type,
                None,
            ),
            InboundFrame::DischargeConstantCurrent(report) => self.record_report(
                device::DeviceMode::DischargeConstantCurrent,
                report.voltage_mv,
                report.current_ma,
                report.milli_ampere_hours,
                report.state.into(),
                &report.device_type,
                None,
            ),
            InboundFrame::DischargeConstantPower(report) => self.record_report(
                device::DeviceMode::DischargeConstantPower,
                report.voltage_mv,
                report.current_ma,
                report.milli_ampere_hours,
                report.state.into(),
                &report.device_type,
                None,
            ),
        }
    }

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
        let active = report_state == ReportState::Active;
        let (outcome, measurement) = self.controller.report(DeviceReport {
            mode,
            state: report_state,
            voltage_mv,
            current_ma,
            capacity_mah,
            model: model.to_owned(),
            firmware_version: firmware,
        });
        self.sync_controller_state();
        if let Some(measurement) = measurement {
            let sample = Sample {
                run_id: self.persistence.current_run_id.clone(),
                sequence: self.persistence.next_sequence,
                timestamp_utc: Utc::now().to_rfc3339(),
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
        let persistence_result = if !active {
            self.persist_and_publish()
        } else {
            self.persist_report_and_publish()
        };
        if let Err(error) = persistence_result {
            log::error!("failed to persist device report: {error}");
        }
    }

    fn set_connection_error(&mut self, error: &str) {
        self.port = None;
        self.serial_buffer.clear();
        self.mock_idle_report_due = None;
        self.snapshot.connection = ServerConnectionState::Error;
        self.snapshot.connection_error = Some(error.to_owned());
        self.controller.disconnect(&format!(
            "device connection failed; physical test state is unknown: {error}"
        ));
        self.sync_controller_state();
        if let Err(persistence_error) = self.persistence.save_metadata(&self.snapshot) {
            log::error!("failed to persist connection failure: {persistence_error}");
        }
        self.publish();
    }

    fn persist_and_publish(&mut self) -> Result<(), String> {
        self.sync_controller_state();
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
        let update = SnapshotUpdate::from(&self.snapshot);
        let _receivers = self.snapshot_tx.send(WebSocketEvent::Update(update));
    }

    fn sync_controller_state(&mut self) {
        self.controller.update_elapsed();
        self.snapshot.device = self.controller.device().clone();
        self.snapshot.test = self.controller.test().clone();
        self.snapshot.capabilities = self.controller.capabilities();
    }

    fn snapshot_for_clients(&self) -> AuthoritativeSnapshot {
        let mut snapshot = self.snapshot.clone();
        snapshot.history = presentation_history(&snapshot.history, SNAPSHOT_SAMPLE_LIMIT);
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
        snapshot_tx,
        allowed_origin: std::env::var("EBC_ALLOWED_ORIGIN").ok(),
    };
    let api = Router::new()
        .route("/status", get(get_status))
        .route("/history", get(get_history))
        .route("/history.csv", get(get_history_csv))
        .route("/runs", get(get_runs))
        .route("/runs/{file}", get(get_run_csv))
        .route("/connect", post(connect))
        .route("/disconnect", post(disconnect))
        .route("/test/start", post(start_test))
        .route("/test/adjust", post(adjust_test))
        .route("/test/stop", post(stop_test))
        .route("/test/resume", post(resume_test))
        .route("/calibration", post(calibration))
        .route("/ws", get(websocket));
    let static_files = ServeDir::new(&config.static_dir)
        .not_found_service(ServeFile::new(config.static_dir.join("index.html")));
    let app = Router::new()
        .nest("/api", api)
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

async fn get_runs(State(state): State<AppState>) -> Result<Json<Vec<RunSummary>>, ApiError> {
    match request(&state, ActorRequest::Runs).await? {
        ActorResponse::Runs(runs) => Ok(Json(runs)),
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}

async fn get_run_csv(
    State(state): State<AppState>,
    AxumPath(file): AxumPath<String>,
) -> Result<Response, ApiError> {
    let id = file
        .strip_suffix(".csv")
        .ok_or_else(|| ApiError::bad_request("archived run path must end in .csv"))?;
    if !valid_run_id(id) {
        return Err(ApiError::bad_request("invalid run id"));
    }
    match request(&state, ActorRequest::RunCsv(id.to_owned())).await? {
        ActorResponse::Export(export) => export_response(export),
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
    Json(config): Json<TestConfiguration>,
) -> Result<Json<AuthoritativeSnapshot>, ApiError> {
    validate_mutation(&headers, &state)?;
    config
        .validate()
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    command(&state, ApiCommand::Start(config)).await
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
    let mut updates = state.snapshot_tx.subscribe();
    if let Ok(ActorResponse::Snapshot(snapshot)) =
        request(&state, ActorRequest::CompleteSnapshot).await
        && send_event(&mut socket, &WebSocketEvent::Snapshot(snapshot))
            .await
            .is_err()
    {
        return;
    }
    loop {
        let event = match updates.recv().await {
            Ok(event) => event,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                match request(&state, ActorRequest::CompleteSnapshot).await {
                    Ok(ActorResponse::Snapshot(snapshot)) => WebSocketEvent::Snapshot(snapshot),
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

    fn mock_actor(name: &str) -> (DeviceActor, PathBuf) {
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

    fn read_export(mut export: ExportDescriptor) -> String {
        let mut contents = String::new();
        (&mut export.file)
            .take(export.length)
            .read_to_string(&mut contents)
            .expect("read export");
        contents
    }

    fn test_config() -> TestConfiguration {
        TestConfiguration::DischargeConstantCurrent {
            current_ma: 1000,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 0,
        }
    }

    fn confirm_inactive(actor: &mut DeviceActor) {
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

    fn confirm_running(actor: &mut DeviceActor) {
        actor.start_test(test_config()).expect("start test");
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

    fn numbered_sample(sequence: u64, voltage_mv: u16, current_ma: u16) -> Sample {
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
        actor.start_test(test_config()).expect("start command");
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
    fn failed_start_write_is_persisted_as_uncertain_and_revokes_transport() {
        let (mut actor, directory) = mock_actor("failed-start-write");
        confirm_inactive(&mut actor);
        inject_write_failure(&mut actor);

        let error = actor
            .handle_command(ApiCommand::Start(test_config()))
            .expect_err("start write fails");

        assert!(error.contains("injected serial write failure"));
        assert_failed_transport(&actor);
        assert_eq!(actor.snapshot.test.state, TestState::RecoveredUncertain);
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
            .start_test(test_config())
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
    fn late_start_metadata_failure_restores_archived_run_for_retry() {
        let (mut actor, directory) = mock_actor("late-start-metadata-failure");
        confirm_inactive(&mut actor);
        confirm_running(&mut actor);
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
        let previous_sample_count = actor.persistence.raw_sample_count;
        actor.start_metadata_failure = Some("injected metadata failure".to_owned());
        actor.write_failure = Some("physical send must not run".to_owned());

        actor
            .start_test(test_config())
            .expect_err("metadata write fails");

        assert_eq!(actor.snapshot.test.state, TestState::Stopped);
        assert_eq!(actor.snapshot.history, previous_history);
        assert_eq!(actor.persistence.current_run_id, previous_run_id);
        assert_eq!(actor.persistence.raw_sample_count, previous_sample_count);
        assert_eq!(actor.persistence.runs.len(), 1);
        assert!(
            actor.write_failure.is_some(),
            "physical send was not attempted"
        );

        actor.write_failure = None;
        actor.start_metadata_failure = None;
        actor.start_test(test_config()).expect("retry start");
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
    fn mutation_header_and_origin_policy_is_strict() {
        let (actor_tx, _actor_rx) = std_mpsc::channel();
        let (snapshot_tx, _) = broadcast::channel(1);
        let mut state = AppState {
            actor_tx,
            snapshot_tx,
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
