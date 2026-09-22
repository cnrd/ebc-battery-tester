//! GUI-independent data and API types shared by clients and the server.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::device::{
    DeviceMode, MAX_CHARGE_CURRENT_MA, MAX_CHARGE_CUTOFF_CURRENT_MA, MAX_CUTOFF_TIME_MIN,
    MAX_DISCHARGE_CURRENT_MA, MAX_POWER_W, MAX_VOLTAGE_MV, MIN_CHARGE_CURRENT_MA,
    MIN_CHARGE_CUTOFF_CURRENT_MA, MIN_DISCHARGE_CURRENT_MA, MIN_POWER_W, MIN_VOLTAGE_MV,
};

pub const MAX_EXECUTION_NAME_CHARS: usize = 120;

/// Trims and validates an optional user-facing execution name.
///
/// # Errors
/// Returns an error when the name contains a control character or exceeds the
/// maximum number of Unicode scalar values.
pub fn normalize_execution_name(input: Option<&str>) -> Result<Option<String>, ValidationError> {
    if input.is_some_and(|name| name.chars().any(char::is_control)) {
        return Err(ValidationError {
            field: "name".to_owned(),
            message: "must not contain control characters".to_owned(),
        });
    }
    let Some(name) = input.map(str::trim).filter(|name| !name.is_empty()) else {
        return Ok(None);
    };
    if name.chars().count() > MAX_EXECUTION_NAME_CHARS {
        return Err(ValidationError {
            field: "name".to_owned(),
            message: format!("must be at most {MAX_EXECUTION_NAME_CHARS} characters"),
        });
    }
    Ok(Some(name.to_owned()))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub sequence: u64,
    pub timestamp_utc: String,
    pub elapsed_seconds: u64,
    pub voltage_mv: u16,
    pub current_ma: u16,
    pub capacity_mah: u64,
    #[serde(default)]
    pub energy_wh: f64,
    pub mode: DeviceMode,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CycleSample {
    pub execution_id: String,
    pub sequence: u64,
    pub timestamp_utc: String,
    pub elapsed_milliseconds: u64,
    pub repeat_index: u32,
    pub step_index: usize,
    pub cycle_state: CycleState,
    pub test_state: TestState,
    pub mode: DeviceMode,
    pub activity_known: bool,
    pub active: bool,
    pub voltage_mv: u16,
    pub current_ma: u16,
    pub device_capacity_mah: u16,
    pub test_capacity_mah: Option<u64>,
    pub test_energy_wh: f64,
}

pub(crate) fn cycle_presentation_history(
    samples: &[CycleSample],
    limit: usize,
) -> Vec<CycleSample> {
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Error,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestState {
    #[default]
    Idle,
    Starting,
    Running,
    Stopping,
    Stopped,
    Completed,
    RecoveredUncertain,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceState {
    pub model: Option<String>,
    pub firmware_version: Option<String>,
    pub mode: Option<DeviceMode>,
    #[serde(default)]
    pub activity_known: bool,
    pub active: bool,
    pub voltage_mv: Option<u16>,
    pub current_ma: Option<u16>,
    pub capacity_mah: Option<u16>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TestStatus {
    pub state: TestState,
    pub config: Option<TestConfiguration>,
    pub started_at_utc: Option<String>,
    pub elapsed_seconds: u64,
    pub result: Option<String>,
    pub capacity_mah: Option<u64>,
    #[serde(default)]
    pub energy_wh: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub start: bool,
    #[serde(default)]
    pub resume: bool,
    #[serde(default)]
    pub stop: bool,
    #[serde(default)]
    pub show_stop: bool,
    #[serde(default)]
    pub adjust: bool,
    #[serde(default)]
    pub calibrate_voltage: bool,
    #[serde(default)]
    pub calibrate_current: bool,
    #[serde(default)]
    pub confirm_calibration: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AuthoritativeSnapshot {
    pub connection: ServerConnectionState,
    pub connection_error: Option<String>,
    pub device: DeviceState,
    pub test: TestStatus,
    #[serde(default)]
    pub cycle: CycleStatus,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub current_run: CurrentRunMetadata,
    pub history: Vec<Sample>,
    #[serde(default)]
    pub cycle_history: Vec<CycleSample>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SnapshotUpdate {
    pub connection: ServerConnectionState,
    pub connection_error: Option<String>,
    pub device: DeviceState,
    pub test: TestStatus,
    #[serde(default)]
    pub cycle: CycleStatus,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub current_run: CurrentRunMetadata,
}

impl From<&AuthoritativeSnapshot> for SnapshotUpdate {
    fn from(snapshot: &AuthoritativeSnapshot) -> Self {
        Self {
            connection: snapshot.connection.clone(),
            connection_error: snapshot.connection_error.clone(),
            device: snapshot.device.clone(),
            test: snapshot.test.clone(),
            cycle: snapshot.cycle.clone(),
            capabilities: snapshot.capabilities,
            current_run: snapshot.current_run.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentRunMetadata {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub cycle: Option<CycleRunContext>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "payload", rename_all = "snake_case")]
pub enum WebSocketEvent {
    Snapshot(AuthoritativeSnapshot),
    Update(SnapshotUpdate),
    Sample(Sample),
    CycleSample(CycleSample),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub started_at_utc: Option<String>,
    pub archived_at_utc: String,
    pub state: TestState,
    pub config: Option<TestConfiguration>,
    pub elapsed_seconds: u64,
    pub result: Option<String>,
    pub capacity_mah: Option<u64>,
    #[serde(default)]
    pub energy_wh: f64,
    pub model: Option<String>,
    pub firmware_version: Option<String>,
    pub sample_count: usize,
    #[serde(default)]
    pub cycle: Option<CycleRunContext>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TestConfiguration {
    DischargeConstantCurrent {
        current_ma: u16,
        cutoff_voltage_mv: u16,
        cutoff_time_min: u16,
    },
    DischargeConstantPower {
        power_w: u16,
        cutoff_voltage_mv: u16,
        cutoff_time_min: u16,
    },
    ChargeConstantVoltage {
        current_ma: u16,
        voltage_mv: u16,
        cutoff_current_ma: u16,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartTestRequest {
    pub config: TestConfiguration,
    #[serde(default)]
    pub name: Option<String>,
}

impl TestConfiguration {
    /// Checks device limits and the resolution representable on the wire.
    ///
    /// # Errors
    /// Returns the first field that is outside its accepted range or cannot be
    /// represented without quantization.
    pub fn validate(&self) -> Result<(), ValidationError> {
        match *self {
            Self::DischargeConstantCurrent {
                current_ma,
                cutoff_voltage_mv,
                cutoff_time_min,
            } => {
                range(
                    "current_ma",
                    current_ma,
                    MIN_DISCHARGE_CURRENT_MA,
                    MAX_DISCHARGE_CURRENT_MA,
                )?;
                step_10("current_ma", current_ma)?;
                voltage(cutoff_voltage_mv)?;
                cutoff_time(cutoff_time_min)
            }
            Self::DischargeConstantPower {
                power_w,
                cutoff_voltage_mv,
                cutoff_time_min,
            } => {
                range("power_w", power_w, MIN_POWER_W, MAX_POWER_W)?;
                voltage(cutoff_voltage_mv)?;
                cutoff_time(cutoff_time_min)
            }
            Self::ChargeConstantVoltage {
                current_ma,
                voltage_mv,
                cutoff_current_ma,
            } => {
                range(
                    "current_ma",
                    current_ma,
                    MIN_CHARGE_CURRENT_MA,
                    MAX_CHARGE_CURRENT_MA,
                )?;
                step_10("current_ma", current_ma)?;
                voltage(voltage_mv)?;
                range(
                    "cutoff_current_ma",
                    cutoff_current_ma,
                    MIN_CHARGE_CUTOFF_CURRENT_MA,
                    MAX_CHARGE_CUTOFF_CURRENT_MA,
                )?;
                step_10("cutoff_current_ma", cutoff_current_ma)
            }
        }
    }

    pub fn mode(&self) -> DeviceMode {
        match self {
            Self::DischargeConstantCurrent { .. } => DeviceMode::DischargeConstantCurrent,
            Self::DischargeConstantPower { .. } => DeviceMode::DischargeConstantPower,
            Self::ChargeConstantVoltage { .. } => DeviceMode::ChargeConstantVoltage,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CycleRecipe {
    pub steps: Vec<CycleStep>,
    pub repeat_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartCycleRequest {
    pub recipe: CycleRecipe,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameRequest {
    #[serde(default)]
    pub name: Option<String>,
}

impl CycleRecipe {
    /// Validates that the recipe can be executed by the cycle engine.
    ///
    /// # Errors
    /// Returns an error for an empty recipe, a zero repeat count, or an invalid
    /// device configuration.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.steps.is_empty() {
            return Err(ValidationError {
                field: "steps".to_owned(),
                message: "must contain at least one step".to_owned(),
            });
        }
        if self.repeat_count == 0 {
            return Err(ValidationError {
                field: "repeat_count".to_owned(),
                message: "must be at least 1".to_owned(),
            });
        }

        for (index, step) in self.steps.iter().enumerate() {
            match step {
                CycleStep::Device { config, .. } => {
                    config.validate().map_err(|error| ValidationError {
                        field: format!("steps[{index}].{}", error.field),
                        message: error.message,
                    })?;
                }
                CycleStep::Rest {
                    duration_seconds: 0,
                } => {
                    return Err(ValidationError {
                        field: format!("steps[{index}].duration_seconds"),
                        message: "must be at least 1".to_owned(),
                    });
                }
                CycleStep::Rest { .. } => {}
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CycleStep {
    Device {
        config: TestConfiguration,
        completion: CycleStepCompletion,
    },
    Rest {
        duration_seconds: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CycleStepCompletion {
    Hardware,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CycleState {
    #[default]
    Idle,
    Preparing,
    StartingStep,
    RunningStep,
    Settling,
    Resting,
    Stopping,
    Completed,
    Stopped,
    Interrupted,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CycleStatus {
    pub state: CycleState,
    pub recipe: Option<CycleRecipe>,
    pub execution_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    pub repeat_index: u32,
    pub step_index: usize,
    pub started_at_utc: Option<String>,
    pub result: Option<String>,
    pub rest_remaining_seconds: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CycleRunContext {
    pub execution_id: String,
    pub repeat_index: u32,
    pub step_index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", content = "value", rename_all = "snake_case")]
pub enum CalibrationCommand {
    VoltageLow(u16),
    VoltageHigh(u16),
    CurrentLow(u16),
    CurrentHigh(u16),
    Confirm,
}

impl CalibrationCommand {
    /// Checks that a calibration reference is within the GUI/device limits.
    ///
    /// # Errors
    /// Returns an error when the reference value is outside its accepted range.
    pub fn validate(&self) -> Result<(), ValidationError> {
        match *self {
            Self::VoltageLow(value) | Self::VoltageHigh(value) => {
                range("value", value, 0, MAX_VOLTAGE_MV)
            }
            Self::CurrentLow(value) | Self::CurrentHigh(value) => {
                range("value", value, 0, MAX_CHARGE_CURRENT_MA)
            }
            Self::Confirm => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", content = "payload", rename_all = "snake_case")]
pub enum ApiCommand {
    Connect,
    Disconnect,
    Start(TestConfiguration),
    Adjust(TestConfiguration),
    Stop,
    Resume,
    Calibration(CalibrationCommand),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for ValidationError {}

fn range(field: &str, value: u16, min: u16, max: u16) -> Result<(), ValidationError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(ValidationError {
            field: field.to_owned(),
            message: format!("must be between {min} and {max}"),
        })
    }
}

fn step_10(field: &str, value: u16) -> Result<(), ValidationError> {
    if value.is_multiple_of(10) {
        Ok(())
    } else {
        Err(ValidationError {
            field: field.to_owned(),
            message: "must be a multiple of 10".to_owned(),
        })
    }
}

fn voltage(value: u16) -> Result<(), ValidationError> {
    range("voltage_mv", value, MIN_VOLTAGE_MV, MAX_VOLTAGE_MV)?;
    step_10("voltage_mv", value)
}

fn cutoff_time(value: u16) -> Result<(), ValidationError> {
    range("cutoff_time_min", value, 0, MAX_CUTOFF_TIME_MIN)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "test setup and assertions should fail fast"
)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_optional_execution_names() {
        assert_eq!(normalize_execution_name(None), Ok(None));
        assert_eq!(normalize_execution_name(Some("")), Ok(None));
        assert_eq!(normalize_execution_name(Some("   ")), Ok(None));
        assert_eq!(
            normalize_execution_name(Some("  Cell α  ")),
            Ok(Some("Cell α".to_owned()))
        );

        let maximum = "🪫".repeat(MAX_EXECUTION_NAME_CHARS);
        assert_eq!(
            normalize_execution_name(Some(&maximum)),
            Ok(Some(maximum.clone()))
        );
        let too_long = "界".repeat(MAX_EXECUTION_NAME_CHARS + 1);
        let error = normalize_execution_name(Some(&too_long)).expect_err("name is too long");
        assert_eq!(error.field, "name");
        assert!(error.message.contains("120"));
    }

    #[test]
    fn rejects_control_characters_but_not_duplicate_names() {
        for name in ["line\nbreak", "column\tbreak", "nul\0byte"] {
            let error = normalize_execution_name(Some(name)).expect_err("control character");
            assert_eq!(error.field, "name");
            assert!(error.message.contains("control"));
        }

        assert_eq!(
            normalize_execution_name(Some("Repeated")),
            Ok(Some("Repeated".to_owned()))
        );
        assert_eq!(
            normalize_execution_name(Some("Repeated")),
            Ok(Some("Repeated".to_owned()))
        );
    }

    #[test]
    fn validates_limits_and_protocol_resolution() {
        let valid = TestConfiguration::DischargeConstantCurrent {
            current_ma: MIN_DISCHARGE_CURRENT_MA,
            cutoff_voltage_mv: MAX_VOLTAGE_MV,
            cutoff_time_min: MAX_CUTOFF_TIME_MIN,
        };
        assert_eq!(valid.validate(), Ok(()));

        let invalid = TestConfiguration::DischargeConstantCurrent {
            current_ma: 11,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 0,
        };
        assert_eq!(
            invalid.validate().expect_err("invalid step").field,
            "current_ma"
        );
    }

    #[test]
    fn validates_cycle_recipe_shape_and_device_steps() {
        let empty = CycleRecipe {
            steps: Vec::new(),
            repeat_count: 1,
        };
        assert_eq!(empty.validate().expect_err("empty recipe").field, "steps");

        let no_repeats = CycleRecipe {
            steps: vec![CycleStep::Rest {
                duration_seconds: 0,
            }],
            repeat_count: 0,
        };
        assert_eq!(
            no_repeats.validate().expect_err("zero repeats").field,
            "repeat_count"
        );

        let zero_rest = CycleRecipe {
            steps: vec![CycleStep::Rest {
                duration_seconds: 0,
            }],
            repeat_count: 1,
        };
        let zero_rest_error = zero_rest.validate().expect_err("zero-duration rest");
        assert_eq!(zero_rest_error.field, "steps[0].duration_seconds");
        assert_eq!(zero_rest_error.message, "must be at least 1");

        let invalid_device = CycleRecipe {
            steps: vec![CycleStep::Device {
                config: TestConfiguration::DischargeConstantCurrent {
                    current_ma: 11,
                    cutoff_voltage_mv: 3000,
                    cutoff_time_min: 0,
                },
                completion: CycleStepCompletion::Hardware,
            }],
            repeat_count: 1,
        };
        assert_eq!(
            invalid_device
                .validate()
                .expect_err("invalid device step")
                .field,
            "steps[0].current_ma"
        );

        let maximum_rest = CycleRecipe {
            steps: vec![CycleStep::Rest {
                duration_seconds: u64::MAX,
            }],
            repeat_count: 1,
        };
        assert_eq!(maximum_rest.validate(), Ok(()));
    }

    #[cfg(feature = "server")]
    #[test]
    fn api_snapshot_round_trips_json() {
        let snapshot = AuthoritativeSnapshot::default();
        let json = serde_json::to_string(&snapshot).expect("serialize snapshot");
        let decoded: AuthoritativeSnapshot =
            serde_json::from_str(&json).expect("deserialize snapshot");
        assert_eq!(decoded, snapshot);
    }

    #[cfg(feature = "server")]
    #[test]
    fn naming_requests_use_canonical_json_shapes() {
        let config = TestConfiguration::DischargeConstantPower {
            power_w: 20,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 45,
        };
        let test = StartTestRequest {
            config,
            name: Some("Capacity check".to_owned()),
        };
        assert_eq!(
            serde_json::to_value(test).expect("serialize test request"),
            serde_json::json!({
                "config": {
                    "mode": "discharge_constant_power",
                    "power_w": 20,
                    "cutoff_voltage_mv": 3000,
                    "cutoff_time_min": 45
                },
                "name": "Capacity check"
            })
        );

        let cycle = StartCycleRequest {
            recipe: CycleRecipe {
                steps: vec![CycleStep::Rest {
                    duration_seconds: 5,
                }],
                repeat_count: 2,
            },
            name: None,
        };
        assert_eq!(
            serde_json::to_value(cycle).expect("serialize cycle request"),
            serde_json::json!({
                "recipe": {
                    "steps": [{"type": "rest", "duration_seconds": 5}],
                    "repeat_count": 2
                },
                "name": null
            })
        );
        assert_eq!(
            serde_json::to_value(RenameRequest::default()).expect("serialize rename request"),
            serde_json::json!({"name": null})
        );

        let legacy: StartTestRequest = serde_json::from_value(serde_json::json!({
            "config": {
                "mode": "discharge_constant_power",
                "power_w": 20,
                "cutoff_voltage_mv": 3000,
                "cutoff_time_min": 45
            }
        }))
        .expect("deserialize legacy test request");
        assert_eq!(legacy.name, None);
    }

    #[cfg(feature = "server")]
    #[test]
    fn missing_wire_capabilities_default_to_denied() {
        let mut value = serde_json::to_value(SnapshotUpdate::default()).expect("serialize update");
        value
            .as_object_mut()
            .expect("update object")
            .remove("capabilities");
        let update: SnapshotUpdate =
            serde_json::from_value(value).expect("deserialize legacy update");

        assert_eq!(update.capabilities, Capabilities::default());
    }

    #[cfg(feature = "server")]
    #[test]
    fn missing_current_run_and_cycle_name_default_to_none() {
        let mut snapshot_value =
            serde_json::to_value(AuthoritativeSnapshot::default()).expect("serialize snapshot");
        snapshot_value
            .as_object_mut()
            .expect("snapshot object")
            .remove("current_run");
        let snapshot: AuthoritativeSnapshot =
            serde_json::from_value(snapshot_value).expect("deserialize legacy snapshot");
        assert_eq!(snapshot.current_run, CurrentRunMetadata::default());

        let mut update_value =
            serde_json::to_value(SnapshotUpdate::default()).expect("serialize update");
        update_value
            .as_object_mut()
            .expect("update object")
            .remove("current_run");
        let update: SnapshotUpdate =
            serde_json::from_value(update_value).expect("deserialize legacy update");
        assert_eq!(update.current_run, CurrentRunMetadata::default());

        let status: CycleStatus = serde_json::from_str(
            r#"{"state":"completed","recipe":null,"execution_id":"cycle-1","repeat_index":0,"step_index":0,"started_at_utc":null,"result":null,"rest_remaining_seconds":null}"#,
        )
        .expect("deserialize legacy cycle status");
        assert_eq!(status.name, None);
    }

    #[cfg(feature = "server")]
    #[test]
    fn old_run_summary_without_name_defaults_to_none() {
        let summary: RunSummary = serde_json::from_value(serde_json::json!({
            "id": "run-1",
            "started_at_utc": null,
            "archived_at_utc": "2026-01-01T00:00:00Z",
            "state": "completed",
            "config": null,
            "elapsed_seconds": 10,
            "result": null,
            "capacity_mah": 1,
            "energy_wh": 0.01,
            "model": null,
            "firmware_version": null,
            "sample_count": 1,
            "cycle": null
        }))
        .expect("deserialize legacy run summary");
        assert_eq!(summary.name, None);
    }

    #[cfg(feature = "server")]
    #[test]
    fn missing_wire_cycle_defaults_to_idle() {
        let mut value = serde_json::to_value(SnapshotUpdate::default()).expect("serialize update");
        value
            .as_object_mut()
            .expect("update object")
            .remove("cycle");
        let update: SnapshotUpdate =
            serde_json::from_value(value).expect("deserialize legacy update");

        assert_eq!(update.cycle, CycleStatus::default());
    }

    #[cfg(feature = "server")]
    #[test]
    fn missing_cycle_history_defaults_empty_and_cycle_sample_round_trips() {
        let mut value =
            serde_json::to_value(AuthoritativeSnapshot::default()).expect("serialize snapshot");
        value
            .as_object_mut()
            .expect("snapshot object")
            .remove("cycle_history");
        let snapshot: AuthoritativeSnapshot =
            serde_json::from_value(value).expect("deserialize legacy snapshot");
        assert!(snapshot.cycle_history.is_empty());

        let sample = CycleSample {
            execution_id: "cycle-1".to_owned(),
            sequence: 7,
            timestamp_utc: "2026-01-01T00:00:00Z".to_owned(),
            elapsed_milliseconds: 1234,
            repeat_index: 1,
            step_index: 2,
            cycle_state: CycleState::Settling,
            test_state: TestState::Completed,
            mode: DeviceMode::ChargeConstantVoltage,
            activity_known: true,
            active: false,
            voltage_mv: 4020,
            current_ma: 450,
            device_capacity_mah: 4,
            test_capacity_mah: Some(4),
            test_energy_wh: 0.014,
        };
        let json = serde_json::to_string(&sample).expect("serialize cycle sample");
        assert_eq!(
            serde_json::from_str::<CycleSample>(&json).expect("deserialize cycle sample"),
            sample
        );
    }

    #[cfg(feature = "server")]
    #[test]
    fn websocket_events_and_remote_commands_round_trip() {
        let command = ApiCommand::Start(TestConfiguration::DischargeConstantPower {
            power_w: 20,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 45,
        });
        let json = serde_json::to_string(&command).expect("serialize command");
        let decoded: ApiCommand = serde_json::from_str(&json).expect("deserialize command");
        assert_eq!(decoded, command);

        let event = WebSocketEvent::Snapshot(AuthoritativeSnapshot::default());
        let json = serde_json::to_string(&event).expect("serialize event");
        let decoded: WebSocketEvent = serde_json::from_str(&json).expect("deserialize event");
        assert_eq!(decoded, event);
    }

    #[cfg(feature = "server")]
    #[test]
    fn old_json_without_energy_uses_zero() {
        let sample: Sample = serde_json::from_str(
            r#"{"timestamp_utc":"2026-01-01T00:00:00Z","elapsed_seconds":0,"voltage_mv":4000,"current_ma":1000,"capacity_mah":0,"mode":"DischargeConstantCurrent"}"#,
        )
        .expect("deserialize old sample");
        let status: TestStatus = serde_json::from_str(
            r#"{"state":"idle","config":null,"started_at_utc":null,"elapsed_seconds":0,"result":null,"capacity_mah":null}"#,
        )
        .expect("deserialize old test status");
        assert!((sample.energy_wh - 0.0).abs() < f64::EPSILON);
        assert!((status.energy_wh - 0.0).abs() < f64::EPSILON);
    }
}
