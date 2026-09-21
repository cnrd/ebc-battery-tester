use serde::{Deserialize, Serialize};

pub const VENDOR_ID: u16 = 0x1A86;

pub const INBOUND_FRAME_SIZE: usize = 19;
pub const OUTBOUND_FRAME_SIZE: usize = 10;

// Start of Frame (SOF) and End of Frame (EOF) bytes.
pub const START_BYTE: u8 = 0xfa;
pub const END_BYTE: u8 = 0xf8;

pub const MIN_DISCHARGE_CURRENT_MA: u16 = 10;
pub const MAX_DISCHARGE_CURRENT_MA: u16 = 20000;
pub const MIN_CHARGE_CURRENT_MA: u16 = 10;
pub const MAX_CHARGE_CURRENT_MA: u16 = 5000;
pub const MIN_CHARGE_CUTOFF_CURRENT_MA: u16 = 10;
pub const MAX_CHARGE_CUTOFF_CURRENT_MA: u16 = 9990;
pub const MIN_POWER_W: u16 = 1;
pub const MAX_POWER_W: u16 = 999;
pub const MIN_VOLTAGE_MV: u16 = 10;
pub const MAX_VOLTAGE_MV: u16 = 30000;
pub const MIN_CUTOFF_TIME_MIN: u16 = 0;
pub const MAX_CUTOFF_TIME_MIN: u16 = 999;
pub const MAX_TIMER_SYNC_MINUTES: u16 = 57_599;
// Max minutes to wait between charge and discharge cycle.
pub const AUTO_MODE_TIME_MIN_MINS: u16 = 0;
pub const AUTO_MODE_TIME_MAX_MINS: u16 = 10;

// ZKETECH EBC model codes sent from the device.
enum DeviceType {
    EbcA05 = 0x05,
    EbcA10H = 0x06,
    EbcA20 = 0x09,
}

fn get_device_model_name(device_type_code: u8) -> String {
    match device_type_code {
        x if x == DeviceType::EbcA05 as u8 => "EBC-A05".to_owned(),
        x if x == DeviceType::EbcA10H as u8 => "EBC-A10H".to_owned(),
        x if x == DeviceType::EbcA20 as u8 => "EBC-A20".to_owned(),
        _ => format!("Unknown ({device_type_code:#04x})"),
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbDeviceInfo {
    pub product_name: String,
    pub manufacturer_name: String,
    pub vendor_id: u16,
    pub product_id: u16,
}

impl std::fmt::Display for UsbDeviceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.product_name.is_empty() {
            write!(
                f,
                "Unknown ({:04x}:{:04x})",
                self.vendor_id, self.product_id
            )
        } else {
            write!(
                f,
                "{} ({:04x}:{:04x})",
                self.product_name, self.vendor_id, self.product_id
            )
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

#[derive(Clone, Debug, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceMode {
    DischargeConstantCurrent,
    DischargeConstantPower,
    ChargeConstantVoltage,
}

impl std::fmt::Display for DeviceMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DischargeConstantCurrent => write!(f, "Discharge Constant Current"),
            Self::DischargeConstantPower => write!(f, "Discharge Constant Power"),
            Self::ChargeConstantVoltage => write!(f, "Charge Constant Voltage"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum OutboundFrame {
    // Send connect command to the device. This will display '-PC-' on the LCD
    // screen. The usize is the index of the device to connect to.
    Connect(usize),
    // Send disconnect command to the device. After this '-PC-' disappears from
    // LCD screen.
    Disconnect,
    // Stop ongoing discharge or charge mode.
    Stop,
    // Start constant current discharge mode with given discharge current in mA,
    // cutoff voltage in mV, and cutoff time in minutes. If cutoff time is 0,
    // means indefinite. The cutoff
    // current voltage values are quantized to 10mA and 10mV. This is because
    // the device only allows setting the value in steps of 10 minimum. Maximum
    // current is 20A, maximum voltage is 30V, and maximum cutoff time is 999
    // minutes. These are also same limits the device has.
    StartConstantCurrentDischarge(u16, u16, u16),
    // Same parameters as StartConstantCurrentDischarge.
    AdjustConstantCurrentDischarge(u16, u16, u16),
    // Same parameters as StartConstantCurrentDischarge.
    ContinueConstantCurrentDischarge(u16, u16, u16),
    // Start constant power discharge mode with given power in W, cutoff voltage
    // in mV, and cutoff time in minutes. If cutoff time is 0, means indefinite.
    // The cutoff voltage value is quantized to 10mV. This is
    // because the device only allows setting the value in steps of 10 minimum.
    // Maximum power is 200W, maximum voltage is 30V, and maximum cutoff time is
    // 999 minutes. These are also same limits the device has.
    StartConstantPowerDischarge(u16, u16, u16),
    // Same parameters as StartConstantPowerDischarge.
    ContinueConstantPowerDischarge(u16, u16, u16),
    // Start constant voltage charge mode with given charge current in mA,
    // charge voltage in mV and cutoff current in mA. The charge voltage and
    // current values are quantized to 10mV and 10mA. This is because the device
    // only allows setting the value in steps of 10 minimum. Maximum charge
    // current is 5A, maximum voltage is 30V, and maximum cutoff current is
    // 9990mA. These are also same limits the device has.
    StartConstantVoltageCharge(u16, u16, u16),
    // Same parameters as StartConstantVoltageCharge.
    ContinueConstantVoltageCharge(u16, u16, u16),
    // Elapsed minutes since the current mode started, sent once per minute.
    TimerSync(u16),
    // Calibration sub-commands (command byte 0x04). Values are in full mV or mA,
    // not divided by 10 like other commands.
    CalibrateVoltageLow(u16),  // low voltage reference in mV
    CalibrateVoltageHigh(u16), // high voltage reference in mV
    CalibrateCurrentLow(u16),  // low current reference in mA
    CalibrateCurrentHigh(u16), // high current reference in mA
    // Writes all four reference values to device storage. If not sent after
    // calibration commands, the new calibration values will not be saved and
    // lost after device is turned off.
    CalibrateConfirm,
}

impl std::convert::From<OutboundFrame> for [u8; OUTBOUND_FRAME_SIZE] {
    fn from(frame: OutboundFrame) -> Self {
        match frame {
            OutboundFrame::Connect(_) => connect_command(),
            OutboundFrame::Disconnect => disconnect_command(),
            OutboundFrame::Stop => stop_command(),
            OutboundFrame::StartConstantCurrentDischarge(
                current_ma,
                cutoff_mv,
                cutoff_time_min,
            ) => start_constant_current_discharge_command(current_ma, cutoff_mv, cutoff_time_min),
            OutboundFrame::AdjustConstantCurrentDischarge(
                current_ma,
                cutoff_mv,
                cutoff_time_min,
            ) => adjust_constant_current_discharge_command(current_ma, cutoff_mv, cutoff_time_min),
            OutboundFrame::ContinueConstantCurrentDischarge(
                current_ma,
                cutoff_mv,
                cutoff_time_min,
            ) => {
                continue_constant_current_discharge_command(current_ma, cutoff_mv, cutoff_time_min)
            }
            OutboundFrame::StartConstantPowerDischarge(power_w, cutoff_mv, cutoff_time_min) => {
                start_constant_power_discharge_command(power_w, cutoff_mv, cutoff_time_min)
            }
            OutboundFrame::ContinueConstantPowerDischarge(power_w, cutoff_mv, cutoff_time_min) => {
                continue_constant_power_discharge_command(power_w, cutoff_mv, cutoff_time_min)
            }
            OutboundFrame::StartConstantVoltageCharge(
                current_ma,
                charge_voltage_mv,
                cutoff_current_ma,
            ) => start_constant_voltage_charge_command(
                current_ma,
                charge_voltage_mv,
                cutoff_current_ma,
            ),
            OutboundFrame::ContinueConstantVoltageCharge(
                current_ma,
                charge_voltage_mv,
                cutoff_current_ma,
            ) => continue_constant_voltage_charge_command(
                current_ma,
                charge_voltage_mv,
                cutoff_current_ma,
            ),
            OutboundFrame::TimerSync(minutes) => timer_sync_command(minutes),
            OutboundFrame::CalibrateVoltageLow(mv) => calibration_command(0x00, mv),
            OutboundFrame::CalibrateVoltageHigh(mv) => calibration_command(0x01, mv),
            OutboundFrame::CalibrateCurrentLow(ma) => calibration_command(0x02, ma),
            OutboundFrame::CalibrateCurrentHigh(ma) => calibration_command(0x03, ma),
            OutboundFrame::CalibrateConfirm => calibration_command(0x04, 0),
        }
    }
}

enum CommmandType {
    Connect = 0x05,
    Disconnect = 0x06,
    Stop = 0x02,
    StartConstantCurrentDischarge = 0x01,
    AdjustConstantCurrentDischarge = 0x07,
    ContinueConstantCurrentDischarge = 0x08,
    StartConstantPowerDischarge = 0x11,
    ContinueConstantPowerDischarge = 0x18,
    StartConstantVoltageCharge = 0x21,
    ContinueConstantVoltageCharge = 0x28,
    TimerSync = 0x0A,
    Calibration = 0x04,
}

enum StatusReportType {
    DischargeConstantCurrentOnReport = 0x0A,
    DischargeConstantCurrentOnFirmwareReport = 0x6E,
    DischargeConstantCurrentOffReport = 0x00,
    DischargeConstantCurrentOffFirmwareReport = 0x64,
    DischargeConstantCurrentEnd = 0x14,

    DischargeConstantPowerOnReport = 0x0B,
    DischargeConstantPowerOnFirmwareReport = 0x6F,
    DischargeConstantPowerOffReport = 0x01,
    DischargeConstantPowerOffFirmwareReport = 0x65,
    DischargeConstantPowerEnd = 0x15,

    ChargeConstantCurrentOnReport = 0x0C,
    ChargeConstantCurrentOnFirmwareReport = 0x70,
    ChargeConstantCurrentOffReport = 0x02,
    ChargeConstantCurrentOffFirmwareReport = 0x66,
    ChargeConstantCurrentEnd = 0x16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FirmwareReport {
    pub device_mode: DeviceMode,
    pub in_progress: bool,
    pub current_ma: u16,
    pub voltage_mv: u16,
    pub milli_ampere_hours: u16,
    pub unknown: u16, // Always 0.
    pub firmware_version: String,
    // Calibration parameters, offset and gain maybe?
    pub unknown1: u16, // Always 2988
    pub unknown2: u16, // Always 2087
    pub device_type: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeReportState {
    Idle,
    Active,
    Finished,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChargeReport {
    pub state: ModeReportState,
    pub current_ma: u16,
    pub voltage_mv: u16,
    pub milli_ampere_hours: u16,
    pub unknown: u16, // Always 0.
    pub charge_current_ma: u16,
    pub charge_voltage_mv: u16,
    pub cutoff_current_ma: u16,
    pub device_type: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DischargeConstantCurrentReport {
    pub state: ModeReportState,
    pub current_ma: u16,
    pub voltage_mv: u16,
    pub milli_ampere_hours: u16,
    pub unknown: u16, // Always 0.
    pub discharge_current_ma: u16,
    pub cutoff_voltage_mv: u16,
    pub cutoff_time_min: u16,
    pub device_type: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DischargeConstantPowerReport {
    pub state: ModeReportState,
    pub current_ma: u16,
    pub voltage_mv: u16,
    pub milli_ampere_hours: u16,
    pub unknown: u16, // Always 0.
    pub discharge_power_w: u16,
    pub cutoff_voltage_mv: u16,
    pub cutoff_time_min: u16,
    pub device_type: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum InboundFrame {
    Firmware(FirmwareReport),
    DischargeConstantCurrent(DischargeConstantCurrentReport),
    DischargeConstantPower(DischargeConstantPowerReport),
    Charge(ChargeReport),
}

impl InboundFrame {
    fn construct_firmware_report(payload: &[u8]) -> Self {
        let command_byte = payload[0];
        let in_progress = command_byte
            == StatusReportType::ChargeConstantCurrentOnFirmwareReport as u8
            || command_byte == StatusReportType::DischargeConstantPowerOnFirmwareReport as u8
            || command_byte == StatusReportType::DischargeConstantCurrentOnFirmwareReport as u8;
        let device_mode = if command_byte
            == StatusReportType::ChargeConstantCurrentOnFirmwareReport as u8
            || command_byte == StatusReportType::ChargeConstantCurrentOffFirmwareReport as u8
        {
            DeviceMode::ChargeConstantVoltage
        } else if command_byte == StatusReportType::DischargeConstantPowerOnFirmwareReport as u8
            || command_byte == StatusReportType::DischargeConstantPowerOffFirmwareReport as u8
        {
            DeviceMode::DischargeConstantPower
        } else {
            DeviceMode::DischargeConstantCurrent
        };
        let version = decode_base240(payload[9], payload[10]);
        let major = version / 100;
        let minor = (version % 100) / 10;
        let patch = version % 10;

        Self::Firmware(FirmwareReport {
            device_mode,
            in_progress,
            current_ma: decode_base240(payload[1], payload[2]) * 10,
            voltage_mv: decode_base240(payload[3], payload[4]),
            milli_ampere_hours: decode_base240(payload[5], payload[6]),
            unknown: decode_base240(payload[7], payload[8]),
            firmware_version: format!("{major}.{minor}.{patch}"),
            unknown1: decode_base240(payload[11], payload[12]),
            unknown2: decode_base240(payload[13], payload[14]),
            device_type: get_device_model_name(payload[15]),
        })
    }

    fn construct_charge_report(payload: &[u8]) -> Self {
        let command_byte = payload[0];
        Self::Charge(ChargeReport {
            state: mode_report_state(
                command_byte,
                StatusReportType::ChargeConstantCurrentOnReport,
                StatusReportType::ChargeConstantCurrentEnd,
            ),
            current_ma: decode_base240(payload[1], payload[2]) * 10,
            voltage_mv: decode_base240(payload[3], payload[4]),
            milli_ampere_hours: decode_base240(payload[5], payload[6]),
            unknown: decode_base240(payload[7], payload[8]),
            charge_current_ma: decode_base240(payload[9], payload[10]) * 10,
            charge_voltage_mv: decode_base240(payload[11], payload[12]) * 10,
            cutoff_current_ma: decode_base240(payload[13], payload[14]) * 10,
            device_type: get_device_model_name(payload[15]),
        })
    }

    fn construct_discharge_constant_current_report(payload: &[u8]) -> Self {
        let command_byte = payload[0];
        Self::DischargeConstantCurrent(DischargeConstantCurrentReport {
            state: mode_report_state(
                command_byte,
                StatusReportType::DischargeConstantCurrentOnReport,
                StatusReportType::DischargeConstantCurrentEnd,
            ),
            current_ma: decode_base240(payload[1], payload[2]) * 10,
            voltage_mv: decode_base240(payload[3], payload[4]),
            milli_ampere_hours: decode_base240(payload[5], payload[6]),
            unknown: decode_base240(payload[7], payload[8]),
            discharge_current_ma: decode_base240(payload[9], payload[10]) * 10,
            cutoff_voltage_mv: decode_base240(payload[11], payload[12]) * 10,
            cutoff_time_min: decode_base240(payload[13], payload[14]),
            device_type: get_device_model_name(payload[15]),
        })
    }

    fn construct_discharge_constant_power_report(payload: &[u8]) -> Self {
        let command_byte = payload[0];
        Self::DischargeConstantPower(DischargeConstantPowerReport {
            state: mode_report_state(
                command_byte,
                StatusReportType::DischargeConstantPowerOnReport,
                StatusReportType::DischargeConstantPowerEnd,
            ),
            current_ma: decode_base240(payload[1], payload[2]) * 10,
            voltage_mv: decode_base240(payload[3], payload[4]),
            milli_ampere_hours: decode_base240(payload[5], payload[6]),
            unknown: decode_base240(payload[7], payload[8]),
            discharge_power_w: decode_base240(payload[9], payload[10]),
            cutoff_voltage_mv: decode_base240(payload[11], payload[12]) * 10,
            cutoff_time_min: decode_base240(payload[13], payload[14]),
            device_type: get_device_model_name(payload[15]),
        })
    }
}

fn mode_report_state(
    command_byte: u8,
    active: StatusReportType,
    finished: StatusReportType,
) -> ModeReportState {
    if command_byte == active as u8 {
        ModeReportState::Active
    } else if command_byte == finished as u8 {
        ModeReportState::Finished
    } else {
        ModeReportState::Idle
    }
}

impl TryFrom<&[u8]> for InboundFrame {
    type Error = String;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        if value.len() != INBOUND_FRAME_SIZE {
            return Err(format!(
                "Frame length mismatch: expected {}, got {}",
                INBOUND_FRAME_SIZE,
                value.len()
            ));
        }
        if value[0] != START_BYTE {
            return Err(format!(
                "Invalid start byte: expected {START_BYTE:#04x}, got {:#04x}",
                value[0]
            ));
        }
        if value[value.len() - 1] != END_BYTE {
            return Err(format!(
                "Invalid end byte: expected {END_BYTE:#04x}, got {:#04x}",
                value[value.len() - 1]
            ));
        }
        let payload = &value[1..value.len() - 2];
        let checksum = value[value.len() - 2];
        let calculated_checksum = xor_checksum(payload);
        // Captured inbound frames encode XOR values >= 0xf0 either verbatim or
        // reduced by 0xf0. The reason for this device behavior is unknown.
        if !inbound_checksum_valid(calculated_checksum, checksum) {
            log::warn!(
                "Invalid checksum: expected {calculated_checksum:#04x}, got {checksum:#04x}"
            );
        }
        let command_byte = payload[0];
        match command_byte {
            // Charging, discharge (constant power and current) and idle
            // firmware reports have same frame structure. The difference is
            // that when charge is idle. It will send idle firmware report. When
            // charging, firmware report is sent for few seconds and not after
            // that. starting charge.
            x if x == StatusReportType::ChargeConstantCurrentOnFirmwareReport as u8
                || x == StatusReportType::ChargeConstantCurrentOffFirmwareReport as u8
                || x == StatusReportType::DischargeConstantPowerOnFirmwareReport as u8
                || x == StatusReportType::DischargeConstantPowerOffFirmwareReport as u8
                || x == StatusReportType::DischargeConstantCurrentOnFirmwareReport as u8
                || x == StatusReportType::DischargeConstantCurrentOffFirmwareReport as u8 =>
            {
                Ok(Self::construct_firmware_report(payload))
            }
            x if x == StatusReportType::ChargeConstantCurrentOnReport as u8
                || x == StatusReportType::ChargeConstantCurrentOffReport as u8
                || x == StatusReportType::ChargeConstantCurrentEnd as u8 =>
            {
                Ok(Self::construct_charge_report(payload))
            }
            x if x == StatusReportType::DischargeConstantCurrentOnReport as u8
                || x == StatusReportType::DischargeConstantCurrentOffReport as u8
                || x == StatusReportType::DischargeConstantCurrentEnd as u8 =>
            {
                Ok(Self::construct_discharge_constant_current_report(payload))
            }
            x if x == StatusReportType::DischargeConstantPowerOnReport as u8
                || x == StatusReportType::DischargeConstantPowerOffReport as u8
                || x == StatusReportType::DischargeConstantPowerEnd as u8 =>
            {
                Ok(Self::construct_discharge_constant_power_report(payload))
            }
            _ => Err(format!("Unknown command byte: {command_byte:#04x}")),
        }
    }
}

pub fn process_buffer(buf: &mut Vec<u8>) -> Vec<(InboundFrame, Vec<u8>)> {
    let mut frames = Vec::new();
    loop {
        if let Some(start) = buf.iter().position(|&b| b == START_BYTE) {
            if start > 0 {
                buf.drain(..start);
            }
        } else {
            buf.clear();
            break;
        }
        if buf.len() < INBOUND_FRAME_SIZE {
            break;
        }
        if buf[INBOUND_FRAME_SIZE - 1] != END_BYTE {
            buf.drain(..1);
            continue;
        }
        let raw = buf[..INBOUND_FRAME_SIZE].to_vec();
        match InboundFrame::try_from(raw.as_slice()) {
            Ok(frame) => {
                buf.drain(..INBOUND_FRAME_SIZE);
                frames.push((frame, raw));
            }
            Err(error) => {
                log::warn!("Failed to parse frame: {error}");
                buf.drain(..1);
            }
        }
    }
    frames
}

// Encoding to prevent bytes > 240 in the byte stream, allowing 0xfa and 0xf8
// to be safely used as SOF and EOF markers.
fn encode_base240(value: u16) -> (u8, u8) {
    debug_assert!(
        value < 0xf0 * 0xf0 + 0xf0,
        "Value too large to encode in base240: {value}"
    );
    let h = (value / 0xf0) as u8;
    let l = (value % 0xf0) as u8;
    (h, l)
}

fn decode_base240(h: u8, l: u8) -> u16 {
    0xf0 * h as u16 + l as u16
}

fn xor_checksum(data: &[u8]) -> u8 {
    data.iter().fold(0, |acc, &b| acc ^ b)
}

fn inbound_checksum_valid(calculated: u8, actual: u8) -> bool {
    actual == calculated || (calculated >= 0xf0 && actual == calculated - 0xf0)
}

fn build_frame(payload: [u8; 7]) -> [u8; OUTBOUND_FRAME_SIZE] {
    let mut frame = [0u8; OUTBOUND_FRAME_SIZE];
    frame[0] = START_BYTE;
    frame[1..8].copy_from_slice(&payload);
    frame[8] = xor_checksum(&payload);
    frame[9] = END_BYTE;
    frame
}

fn connect_command() -> [u8; OUTBOUND_FRAME_SIZE] {
    build_frame([
        CommmandType::Connect as u8,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
    ])
}

fn disconnect_command() -> [u8; OUTBOUND_FRAME_SIZE] {
    build_frame([
        CommmandType::Disconnect as u8,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
    ])
}

fn stop_command() -> [u8; OUTBOUND_FRAME_SIZE] {
    build_frame([CommmandType::Stop as u8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00])
}

fn start_constant_current_discharge_command(
    current_ma: u16,
    cutoff_mv: u16,
    cutoff_time_min: u16,
) -> [u8; OUTBOUND_FRAME_SIZE] {
    assert!(
        (MIN_DISCHARGE_CURRENT_MA..=MAX_DISCHARGE_CURRENT_MA).contains(&current_ma),
        "Current must be between {MIN_DISCHARGE_CURRENT_MA}mA and {MAX_DISCHARGE_CURRENT_MA}mA"
    );
    assert!(
        (MIN_VOLTAGE_MV..=MAX_VOLTAGE_MV).contains(&cutoff_mv),
        "Cutoff voltage must be between {MIN_VOLTAGE_MV}mV and {MAX_VOLTAGE_MV}mV"
    );
    assert!(
        (cutoff_time_min <= MAX_CUTOFF_TIME_MIN),
        "Cutoff time must be between 0 and {MAX_CUTOFF_TIME_MIN} minutes"
    );

    let (current_h, current_l) = encode_base240(current_ma / 10);
    let (cutoff_h, cutoff_l) = encode_base240(cutoff_mv / 10);
    let (time_h, time_l) = encode_base240(cutoff_time_min);
    build_frame([
        CommmandType::StartConstantCurrentDischarge as u8,
        current_h,
        current_l,
        cutoff_h,
        cutoff_l,
        time_h,
        time_l,
    ])
}

fn adjust_constant_current_discharge_command(
    current_ma: u16,
    cutoff_mv: u16,
    cutoff_time_min: u16,
) -> [u8; OUTBOUND_FRAME_SIZE] {
    assert!(
        (MIN_DISCHARGE_CURRENT_MA..=MAX_DISCHARGE_CURRENT_MA).contains(&current_ma),
        "Current must be between {MIN_DISCHARGE_CURRENT_MA}mA and {MAX_DISCHARGE_CURRENT_MA}mA"
    );
    assert!(
        (MIN_VOLTAGE_MV..=MAX_VOLTAGE_MV).contains(&cutoff_mv),
        "Cutoff voltage must be between {MIN_VOLTAGE_MV}mV and {MAX_VOLTAGE_MV}mV"
    );
    assert!(
        (cutoff_time_min <= MAX_CUTOFF_TIME_MIN),
        "Cutoff time must be between 0 and {MAX_CUTOFF_TIME_MIN} minutes"
    );
    let (current_h, current_l) = encode_base240(current_ma / 10);
    let (cutoff_h, cutoff_l) = encode_base240(cutoff_mv / 10);
    let (time_h, time_l) = encode_base240(cutoff_time_min);
    build_frame([
        CommmandType::AdjustConstantCurrentDischarge as u8,
        current_h,
        current_l,
        cutoff_h,
        cutoff_l,
        time_h,
        time_l,
    ])
}

fn continue_constant_current_discharge_command(
    current_ma: u16,
    cutoff_mv: u16,
    cutoff_time_min: u16,
) -> [u8; OUTBOUND_FRAME_SIZE] {
    assert!(
        (MIN_DISCHARGE_CURRENT_MA..=MAX_DISCHARGE_CURRENT_MA).contains(&current_ma),
        "Current must be between {MIN_DISCHARGE_CURRENT_MA}mA and {MAX_DISCHARGE_CURRENT_MA}mA"
    );
    assert!(
        (MIN_VOLTAGE_MV..=MAX_VOLTAGE_MV).contains(&cutoff_mv),
        "Cutoff voltage must be between {MIN_VOLTAGE_MV}mV and {MAX_VOLTAGE_MV}mV"
    );
    assert!(
        (cutoff_time_min <= MAX_CUTOFF_TIME_MIN),
        "Cutoff time must be between 0 and {MAX_CUTOFF_TIME_MIN} minutes"
    );
    let (current_h, current_l) = encode_base240(current_ma / 10);
    let (cutoff_h, cutoff_l) = encode_base240(cutoff_mv / 10);
    let (time_h, time_l) = encode_base240(cutoff_time_min);
    build_frame([
        CommmandType::ContinueConstantCurrentDischarge as u8,
        current_h,
        current_l,
        cutoff_h,
        cutoff_l,
        time_h,
        time_l,
    ])
}

fn timer_sync_command(minutes: u16) -> [u8; OUTBOUND_FRAME_SIZE] {
    assert!(
        minutes <= MAX_TIMER_SYNC_MINUTES,
        "Timer sync must not exceed {MAX_TIMER_SYNC_MINUTES} minutes"
    );
    let (min_h, min_l) = encode_base240(minutes);
    build_frame([
        CommmandType::TimerSync as u8,
        min_h,
        min_l,
        0x00,
        0x00,
        0x00,
        0x00,
    ])
}

fn calibration_command(sub: u8, value: u16) -> [u8; OUTBOUND_FRAME_SIZE] {
    let (val_h, val_l) = encode_base240(value);
    build_frame([
        CommmandType::Calibration as u8,
        sub,
        val_h,
        val_l,
        0x00,
        0x00,
        0x00,
    ])
}

fn start_constant_power_discharge_command(
    power_w: u16,
    cutoff_mv: u16,
    cutoff_time_min: u16,
) -> [u8; OUTBOUND_FRAME_SIZE] {
    assert!(
        (MIN_POWER_W..=MAX_POWER_W).contains(&power_w),
        "Watts must be between {MIN_POWER_W}W and {MAX_POWER_W}W"
    );
    assert!(
        (MIN_VOLTAGE_MV..=MAX_VOLTAGE_MV).contains(&cutoff_mv),
        "Cutoff voltage must be between {MIN_VOLTAGE_MV}mV and {MAX_VOLTAGE_MV}mV"
    );
    assert!(
        (cutoff_time_min <= MAX_CUTOFF_TIME_MIN),
        "Cutoff time must be between 0 and {MAX_CUTOFF_TIME_MIN} minutes"
    );

    let (power_h, power_l) = encode_base240(power_w);
    let (cutoff_h, cutoff_l) = encode_base240(cutoff_mv / 10);
    let (time_h, time_l) = encode_base240(cutoff_time_min);
    build_frame([
        CommmandType::StartConstantPowerDischarge as u8,
        power_h,
        power_l,
        cutoff_h,
        cutoff_l,
        time_h,
        time_l,
    ])
}

fn continue_constant_power_discharge_command(
    power_w: u16,
    cutoff_mv: u16,
    cutoff_time_min: u16,
) -> [u8; OUTBOUND_FRAME_SIZE] {
    assert!(
        (MIN_POWER_W..=MAX_POWER_W).contains(&power_w),
        "Watts must be between {MIN_POWER_W}W and {MAX_POWER_W}W"
    );
    assert!(
        (MIN_VOLTAGE_MV..=MAX_VOLTAGE_MV).contains(&cutoff_mv),
        "Cutoff voltage must be between {MIN_VOLTAGE_MV}mV and {MAX_VOLTAGE_MV}mV"
    );
    assert!(
        (cutoff_time_min <= MAX_CUTOFF_TIME_MIN),
        "Cutoff time must be between 0 and {MAX_CUTOFF_TIME_MIN} minutes"
    );
    let (power_h, power_l) = encode_base240(power_w);
    let (cutoff_h, cutoff_l) = encode_base240(cutoff_mv / 10);
    let (time_h, time_l) = encode_base240(cutoff_time_min);
    build_frame([
        CommmandType::ContinueConstantPowerDischarge as u8,
        power_h,
        power_l,
        cutoff_h,
        cutoff_l,
        time_h,
        time_l,
    ])
}

fn start_constant_voltage_charge_command(
    current_ma: u16,
    charge_voltage_mv: u16,
    cutoff_current_ma: u16,
) -> [u8; OUTBOUND_FRAME_SIZE] {
    assert!(
        (MIN_CHARGE_CURRENT_MA..=MAX_CHARGE_CURRENT_MA).contains(&current_ma),
        "Current must be between {MIN_CHARGE_CURRENT_MA}mA and {MAX_CHARGE_CURRENT_MA}mA"
    );
    assert!(
        (MIN_VOLTAGE_MV..=MAX_VOLTAGE_MV).contains(&charge_voltage_mv),
        "Charge voltage must be between {MIN_VOLTAGE_MV}mV and {MAX_VOLTAGE_MV}mV"
    );
    assert!(
        (MIN_CHARGE_CUTOFF_CURRENT_MA..=MAX_CHARGE_CUTOFF_CURRENT_MA).contains(&cutoff_current_ma),
        "Cutoff current must be between {MIN_CHARGE_CUTOFF_CURRENT_MA}mA and {MAX_CHARGE_CUTOFF_CURRENT_MA}mA"
    );

    let (current_h, current_l) = encode_base240(current_ma / 10);
    let (charge_voltage_h, charge_voltage_l) = encode_base240(charge_voltage_mv / 10);
    let (cutoff_current_h, cutoff_current_l) = encode_base240(cutoff_current_ma / 10);
    build_frame([
        CommmandType::StartConstantVoltageCharge as u8,
        current_h,
        current_l,
        charge_voltage_h,
        charge_voltage_l,
        cutoff_current_h,
        cutoff_current_l,
    ])
}

fn continue_constant_voltage_charge_command(
    current_ma: u16,
    charge_voltage_mv: u16,
    cutoff_current_ma: u16,
) -> [u8; OUTBOUND_FRAME_SIZE] {
    assert!(
        (MIN_CHARGE_CURRENT_MA..=MAX_CHARGE_CURRENT_MA).contains(&current_ma),
        "Current must be between {MIN_CHARGE_CURRENT_MA}mA and {MAX_CHARGE_CURRENT_MA}mA"
    );
    assert!(
        (MIN_VOLTAGE_MV..=MAX_VOLTAGE_MV).contains(&charge_voltage_mv),
        "Charge voltage must be between {MIN_VOLTAGE_MV}mV and {MAX_VOLTAGE_MV}mV"
    );
    assert!(
        (MIN_CHARGE_CUTOFF_CURRENT_MA..=MAX_CHARGE_CUTOFF_CURRENT_MA).contains(&cutoff_current_ma),
        "Cutoff current must be between {MIN_CHARGE_CUTOFF_CURRENT_MA}mA and {MAX_CHARGE_CUTOFF_CURRENT_MA}mA"
    );
    let (current_h, current_l) = encode_base240(current_ma / 10);
    let (charge_voltage_h, charge_voltage_l) = encode_base240(charge_voltage_mv / 10);
    let (cutoff_current_h, cutoff_current_l) = encode_base240(cutoff_current_ma / 10);
    build_frame([
        CommmandType::ContinueConstantVoltageCharge as u8,
        current_h,
        current_l,
        charge_voltage_h,
        charge_voltage_l,
        cutoff_current_h,
        cutoff_current_l,
    ])
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test parsing should fail fast")]
mod tests {
    use super::*;

    fn inbound_frame(payload: [u8; 16]) -> Vec<u8> {
        let mut frame = vec![START_BYTE];
        frame.extend_from_slice(&payload);
        frame.push(xor_checksum(&payload));
        frame.push(END_BYTE);
        frame
    }

    fn set_base240(payload: &mut [u8; 16], offset: usize, value: u16) {
        let (high, low) = encode_base240(value);
        payload[offset] = high;
        payload[offset + 1] = low;
    }

    fn report_frame(command: u8) -> Vec<u8> {
        let mut payload = [0_u8; 16];
        payload[0] = command;
        inbound_frame(payload)
    }

    #[test]
    fn mode_reports_preserve_idle_active_and_finished_states() {
        for (command, expected) in [
            (0x00, ModeReportState::Idle),
            (0x0a, ModeReportState::Active),
            (0x14, ModeReportState::Finished),
            (0x01, ModeReportState::Idle),
            (0x0b, ModeReportState::Active),
            (0x15, ModeReportState::Finished),
            (0x02, ModeReportState::Idle),
            (0x0c, ModeReportState::Active),
            (0x16, ModeReportState::Finished),
        ] {
            let mut payload = [0_u8; 16];
            payload[0] = command;
            let frame = inbound_frame(payload);

            let state = match InboundFrame::try_from(frame.as_slice()).expect("parse report") {
                InboundFrame::Charge(report) => report.state,
                InboundFrame::DischargeConstantCurrent(report) => report.state,
                InboundFrame::DischargeConstantPower(report) => report.state,
                InboundFrame::Firmware(_) => panic!("expected normal mode report"),
            };
            assert_eq!(state, expected, "command {command:#04x}");
        }
    }

    #[test]
    fn normal_reports_decode_all_configured_values_in_protocol_units() {
        let mut charge = [0_u8; 16];
        charge[0] = StatusReportType::ChargeConstantCurrentOnReport as u8;
        set_base240(&mut charge, 9, 200);
        set_base240(&mut charge, 11, 420);
        set_base240(&mut charge, 13, 50);
        let InboundFrame::Charge(charge) =
            InboundFrame::try_from(inbound_frame(charge).as_slice()).expect("parse charge report")
        else {
            panic!("expected charge report");
        };
        assert_eq!(charge.charge_current_ma, 2_000);
        assert_eq!(charge.charge_voltage_mv, 4_200);
        assert_eq!(charge.cutoff_current_ma, 500);

        let mut current = [0_u8; 16];
        current[0] = StatusReportType::DischargeConstantCurrentOnReport as u8;
        set_base240(&mut current, 9, 150);
        set_base240(&mut current, 11, 390);
        set_base240(&mut current, 13, 30);
        let InboundFrame::DischargeConstantCurrent(current) =
            InboundFrame::try_from(inbound_frame(current).as_slice()).expect("parse CC report")
        else {
            panic!("expected CC report");
        };
        assert_eq!(current.discharge_current_ma, 1_500);
        assert_eq!(current.cutoff_voltage_mv, 3_900);
        assert_eq!(current.cutoff_time_min, 30);

        let mut power = [0_u8; 16];
        power[0] = StatusReportType::DischargeConstantPowerOnReport as u8;
        set_base240(&mut power, 9, 100);
        set_base240(&mut power, 11, 300);
        set_base240(&mut power, 13, 45);
        let InboundFrame::DischargeConstantPower(power) =
            InboundFrame::try_from(inbound_frame(power).as_slice()).expect("parse CP report")
        else {
            panic!("expected CP report");
        };
        assert_eq!(power.discharge_power_w, 100);
        assert_eq!(power.cutoff_voltage_mv, 3_000);
        assert_eq!(power.cutoff_time_min, 45);
    }

    #[test]
    fn inbound_checksum_accepts_observed_high_range_encoding() {
        assert!(inbound_checksum_valid(0xef, 0xef));
        for (calculated, actual) in [
            (0xf0, 0x00),
            (0xf7, 0x07),
            (0xfa, 0x0a),
            (0xfe, 0x0e),
            (0xff, 0x0f),
        ] {
            assert!(inbound_checksum_valid(calculated, calculated));
            assert!(inbound_checksum_valid(calculated, actual));
        }
        assert!(!inbound_checksum_valid(0xfa, 0x0b));
    }

    #[test]
    fn outbound_checksum_keeps_ordinary_high_xor() {
        let frame = calibration_command(0x00, 5_039);
        assert_eq!(xor_checksum(&frame[1..8]), 0xff);
        assert_eq!(frame[8], 0xff);
    }

    #[test]
    fn invalid_inbound_checksum_remains_tolerated() {
        let mut frame = report_frame(StatusReportType::DischargeConstantCurrentOffReport as u8);
        frame[INBOUND_FRAME_SIZE - 2] ^= 0x01;
        let payload = &frame[1..INBOUND_FRAME_SIZE - 2];
        assert!(!inbound_checksum_valid(
            xor_checksum(payload),
            frame[INBOUND_FRAME_SIZE - 2]
        ));
        assert!(InboundFrame::try_from(frame.as_slice()).is_ok());
    }

    #[test]
    fn process_buffer_extracts_one_complete_frame() {
        let expected = report_frame(StatusReportType::DischargeConstantCurrentOffReport as u8);
        let mut buffer = expected.clone();
        let frames = process_buffer(&mut buffer);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].1, expected);
        assert!(buffer.is_empty());
    }

    #[test]
    fn process_buffer_retains_fragmented_and_incomplete_frames() {
        let expected = report_frame(StatusReportType::DischargeConstantCurrentOffReport as u8);
        let mut buffer = expected[..7].to_vec();
        assert!(process_buffer(&mut buffer).is_empty());
        assert_eq!(buffer, expected[..7]);
        buffer.extend_from_slice(&expected[7..15]);
        assert!(process_buffer(&mut buffer).is_empty());
        assert_eq!(buffer, expected[..15]);
        buffer.extend_from_slice(&expected[15..]);
        let frames = process_buffer(&mut buffer);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].1, expected);
        assert!(buffer.is_empty());
    }

    #[test]
    fn process_buffer_extracts_concatenated_frames() {
        let first = report_frame(StatusReportType::DischargeConstantCurrentOffReport as u8);
        let second = report_frame(StatusReportType::DischargeConstantPowerOffReport as u8);
        let mut buffer = first.clone();
        buffer.extend_from_slice(&second);
        let frames = process_buffer(&mut buffer);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].1, first);
        assert_eq!(frames[1].1, second);
        assert!(buffer.is_empty());
    }

    #[test]
    fn process_buffer_discards_garbage_before_valid_frame() {
        let expected = report_frame(StatusReportType::ChargeConstantCurrentOffReport as u8);
        let mut buffer = vec![0x00, 0x01, END_BYTE, 0x02];
        buffer.extend_from_slice(&expected);
        let frames = process_buffer(&mut buffer);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].1, expected);
        assert!(buffer.is_empty());
    }

    #[test]
    fn process_buffer_resynchronizes_after_false_start() {
        let expected = report_frame(StatusReportType::DischargeConstantCurrentOffReport as u8);
        let mut buffer = vec![START_BYTE, 0x01, 0x02, 0x03, 0x04];
        buffer.extend_from_slice(&expected);
        let frames = process_buffer(&mut buffer);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].1, expected);
        assert!(buffer.is_empty());
    }

    #[test]
    fn process_buffer_allows_end_byte_in_checksum_position() {
        let mut payload = [0_u8; 16];
        payload[0] = StatusReportType::DischargeConstantCurrentOffReport as u8;
        payload[15] = xor_checksum(&payload) ^ END_BYTE;
        let expected = inbound_frame(payload);
        assert_eq!(expected[INBOUND_FRAME_SIZE - 2], END_BYTE);
        assert_eq!(expected[INBOUND_FRAME_SIZE - 1], END_BYTE);
        let mut buffer = expected.clone();
        let frames = process_buffer(&mut buffer);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].1, expected);
    }

    #[test]
    fn process_buffer_preserves_nested_frame_with_end_byte_checksum() {
        let mut payload = [0_u8; 16];
        payload[0] = StatusReportType::DischargeConstantCurrentOffReport as u8;
        payload[15] = xor_checksum(&payload) ^ END_BYTE;
        let expected = inbound_frame(payload);
        let mut buffer = vec![START_BYTE];
        buffer.extend_from_slice(&expected);

        let frames = process_buffer(&mut buffer);

        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].1, expected);
        assert!(buffer.is_empty());
    }

    #[test]
    fn process_buffer_recovers_after_malformed_complete_frame() {
        let mut malformed_payload = [0_u8; 16];
        malformed_payload[0] = 0xff;
        let malformed = inbound_frame(malformed_payload);
        let expected = report_frame(StatusReportType::DischargeConstantPowerOffReport as u8);
        let mut buffer = malformed;
        buffer.extend_from_slice(&expected);
        let frames = process_buffer(&mut buffer);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].1, expected);
        assert!(buffer.is_empty());
    }
}
