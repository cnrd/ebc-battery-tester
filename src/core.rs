//! GUI-independent data and API types shared by clients and the server.

use serde::{Deserialize, Serialize};

use crate::device::{
    DeviceMode, MAX_CHARGE_CURRENT_MA, MAX_CHARGE_CUTOFF_CURRENT_MA, MAX_CUTOFF_TIME_MIN,
    MAX_DISCHARGE_CURRENT_MA, MAX_POWER_W, MAX_VOLTAGE_MV, MIN_CHARGE_CURRENT_MA,
    MIN_CHARGE_CUTOFF_CURRENT_MA, MIN_DISCHARGE_CURRENT_MA, MIN_POWER_W, MIN_VOLTAGE_MV,
};

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
    pub capabilities: Capabilities,
    pub history: Vec<Sample>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SnapshotUpdate {
    pub connection: ServerConnectionState,
    pub connection_error: Option<String>,
    pub device: DeviceState,
    pub test: TestStatus,
    #[serde(default)]
    pub capabilities: Capabilities,
}

impl From<&AuthoritativeSnapshot> for SnapshotUpdate {
    fn from(snapshot: &AuthoritativeSnapshot) -> Self {
        Self {
            connection: snapshot.connection.clone(),
            connection_error: snapshot.connection_error.clone(),
            device: snapshot.device.clone(),
            test: snapshot.test.clone(),
            capabilities: snapshot.capabilities,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "payload", rename_all = "snake_case")]
pub enum WebSocketEvent {
    Snapshot(AuthoritativeSnapshot),
    Update(SnapshotUpdate),
    Sample(Sample),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub id: String,
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
