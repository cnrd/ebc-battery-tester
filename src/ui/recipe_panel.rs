use crate::core::{CycleRecipe, CycleState, CycleStep, CycleStepCompletion, TestConfiguration};
use crate::device;
use crate::session::DeviceSession;

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub(crate) struct RecipePanel {
    repeat_count: u32,
    steps: Vec<StepDraft>,
}

impl Default for RecipePanel {
    fn default() -> Self {
        Self {
            repeat_count: 1,
            steps: vec![StepDraft::default()],
        }
    }
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
enum StepDraft {
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
    Rest {
        duration_seconds: u64,
    },
}

impl Default for StepDraft {
    fn default() -> Self {
        Self::DischargeConstantCurrent {
            current_ma: 1_000,
            cutoff_voltage_mv: 3_000,
            cutoff_time_min: 0,
        }
    }
}

impl StepDraft {
    fn kind(&self) -> usize {
        match self {
            Self::DischargeConstantCurrent { .. } => 0,
            Self::DischargeConstantPower { .. } => 1,
            Self::ChargeConstantVoltage { .. } => 2,
            Self::Rest { .. } => 3,
        }
    }

    fn set_kind(&mut self, kind: usize) {
        if self.kind() == kind {
            return;
        }
        *self = match kind {
            0 => Self::default(),
            1 => Self::DischargeConstantPower {
                power_w: 10,
                cutoff_voltage_mv: 3_000,
                cutoff_time_min: 0,
            },
            2 => Self::ChargeConstantVoltage {
                current_ma: 1_000,
                voltage_mv: 4_200,
                cutoff_current_ma: 100,
            },
            _ => Self::Rest {
                duration_seconds: 600,
            },
        };
    }

    fn label(&self) -> &'static str {
        match self {
            Self::DischargeConstantCurrent { .. } => "CC discharge",
            Self::DischargeConstantPower { .. } => "CP discharge",
            Self::ChargeConstantVoltage { .. } => "CV charge",
            Self::Rest { .. } => "Rest",
        }
    }

    fn step(&self) -> CycleStep {
        let completion = CycleStepCompletion::Hardware;
        match *self {
            Self::DischargeConstantCurrent {
                current_ma,
                cutoff_voltage_mv,
                cutoff_time_min,
            } => CycleStep::Device {
                config: TestConfiguration::DischargeConstantCurrent {
                    current_ma,
                    cutoff_voltage_mv,
                    cutoff_time_min,
                },
                completion,
            },
            Self::DischargeConstantPower {
                power_w,
                cutoff_voltage_mv,
                cutoff_time_min,
            } => CycleStep::Device {
                config: TestConfiguration::DischargeConstantPower {
                    power_w,
                    cutoff_voltage_mv,
                    cutoff_time_min,
                },
                completion,
            },
            Self::ChargeConstantVoltage {
                current_ma,
                voltage_mv,
                cutoff_current_ma,
            } => CycleStep::Device {
                config: TestConfiguration::ChargeConstantVoltage {
                    current_ma,
                    voltage_mv,
                    cutoff_current_ma,
                },
                completion,
            },
            Self::Rest { duration_seconds } => CycleStep::Rest { duration_seconds },
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        let mut kind = self.kind();
        egui::ComboBox::from_id_salt("kind")
            .selected_text(self.label())
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut kind, 0, "CC discharge");
                ui.selectable_value(&mut kind, 1, "CP discharge");
                ui.selectable_value(&mut kind, 2, "CV charge");
                ui.selectable_value(&mut kind, 3, "Rest");
            });
        self.set_kind(kind);

        egui::Grid::new("parameters").show(ui, |ui| match self {
            Self::DischargeConstantCurrent {
                current_ma,
                cutoff_voltage_mv,
                cutoff_time_min,
            } => {
                milli_value(
                    ui,
                    "Current",
                    current_ma,
                    device::MIN_DISCHARGE_CURRENT_MA..=device::MAX_DISCHARGE_CURRENT_MA,
                    " mA",
                );
                milli_value(
                    ui,
                    "Cutoff voltage",
                    cutoff_voltage_mv,
                    device::MIN_VOLTAGE_MV..=device::MAX_VOLTAGE_MV,
                    " mV",
                );
                minute_value(ui, cutoff_time_min);
            }
            Self::DischargeConstantPower {
                power_w,
                cutoff_voltage_mv,
                cutoff_time_min,
            } => {
                milli_value(
                    ui,
                    "Power",
                    power_w,
                    device::MIN_POWER_W..=device::MAX_POWER_W,
                    " W",
                );
                milli_value(
                    ui,
                    "Cutoff voltage",
                    cutoff_voltage_mv,
                    device::MIN_VOLTAGE_MV..=device::MAX_VOLTAGE_MV,
                    " mV",
                );
                minute_value(ui, cutoff_time_min);
            }
            Self::ChargeConstantVoltage {
                current_ma,
                voltage_mv,
                cutoff_current_ma,
            } => {
                milli_value(
                    ui,
                    "Current",
                    current_ma,
                    device::MIN_CHARGE_CURRENT_MA..=device::MAX_CHARGE_CURRENT_MA,
                    " mA",
                );
                milli_value(
                    ui,
                    "Voltage",
                    voltage_mv,
                    device::MIN_VOLTAGE_MV..=device::MAX_VOLTAGE_MV,
                    " mV",
                );
                milli_value(
                    ui,
                    "Cutoff current",
                    cutoff_current_ma,
                    device::MIN_CHARGE_CUTOFF_CURRENT_MA..=device::MAX_CHARGE_CUTOFF_CURRENT_MA,
                    " mA",
                );
            }
            Self::Rest { duration_seconds } => {
                ui.label("Duration");
                ui.add(egui::DragValue::new(duration_seconds).suffix(" s").speed(1));
                ui.end_row();
            }
        });
    }
}

fn milli_value(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut u16,
    range: std::ops::RangeInclusive<u16>,
    suffix: &str,
) {
    ui.label(label);
    ui.add(egui::DragValue::new(value).range(range).suffix(suffix));
    ui.end_row();
}

fn minute_value(ui: &mut egui::Ui, value: &mut u16) {
    milli_value(
        ui,
        "Cutoff time",
        value,
        device::MIN_CUTOFF_TIME_MIN..=device::MAX_CUTOFF_TIME_MIN,
        " min",
    );
}

impl RecipePanel {
    pub(crate) fn ui(&mut self, session: &mut DeviceSession, ui: &mut egui::Ui) {
        ui.separator();
        ui.heading("Cycle / Recipe");
        cycle_progress(session, ui);

        let executing = session.cycle_owns_orchestration();
        ui.add_enabled_ui(!executing, |ui| {
            ui.horizontal(|ui| {
                ui.label("Repeat whole recipe");
                ui.add(
                    egui::DragValue::new(&mut self.repeat_count)
                        .range(1..=u32::MAX)
                        .speed(1),
                );
                ui.label("times");
            });

            let mut operation = None;
            let step_count = self.steps.len();
            for (index, step) in self.steps.iter_mut().enumerate() {
                ui.push_id(index, |ui| {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.strong(format!("Step {}", index + 1));
                            if ui.add_enabled(index > 0, egui::Button::new("Up")).clicked() {
                                operation = Some((index, StepOperation::Up));
                            }
                            if ui
                                .add_enabled(index + 1 < step_count, egui::Button::new("Down"))
                                .clicked()
                            {
                                operation = Some((index, StepOperation::Down));
                            }
                            if ui.button("Delete").clicked() {
                                operation = Some((index, StepOperation::Delete));
                            }
                        });
                        step.ui(ui);
                    });
                });
            }
            if let Some((index, operation)) = operation {
                match operation {
                    StepOperation::Up => self.steps.swap(index, index - 1),
                    StepOperation::Down => self.steps.swap(index, index + 1),
                    StepOperation::Delete => {
                        self.steps.remove(index);
                    }
                }
            }
            if ui.button("Add step").clicked() {
                self.steps.push(StepDraft::default());
            }
        });

        ui.horizontal_wrapped(|ui| {
            let recipe = self.recipe();
            let valid = recipe.validate().is_ok();
            if ui
                .add_enabled(
                    !executing && session.can_start() && valid,
                    egui::Button::new("Start recipe"),
                )
                .clicked()
            {
                session.start_cycle(recipe);
            }
            if ui
                .add_enabled(executing, egui::Button::new("Stop recipe"))
                .clicked()
            {
                session.stop_cycle();
            }
        });
    }

    fn recipe(&self) -> CycleRecipe {
        CycleRecipe {
            steps: self.steps.iter().map(StepDraft::step).collect(),
            repeat_count: self.repeat_count,
        }
    }
}

enum StepOperation {
    Up,
    Down,
    Delete,
}

fn cycle_progress(session: &DeviceSession, ui: &mut egui::Ui) {
    let status = &session.cycle;
    let Some(recipe) = &status.recipe else {
        ui.weak("No cycle has been run");
        return;
    };
    let progress = format!(
        "Repeat {} / {}  |  Step {} / {}",
        status.repeat_index + 1,
        recipe.repeat_count,
        status.step_index + 1,
        recipe.steps.len()
    );
    match status.state {
        CycleState::RunningStep | CycleState::StartingStep => {
            let label = recipe
                .steps
                .get(status.step_index)
                .map(step_label)
                .unwrap_or("unknown step");
            ui.strong(format!("Running cycle: {progress} - {label}"));
        }
        CycleState::Settling => {
            ui.strong(format!(
                "Settling after step {} / {}",
                status.step_index + 1,
                recipe.steps.len()
            ));
        }
        CycleState::Resting => {
            ui.strong(format!(
                "Resting - {} remaining ({progress})",
                crate::ui::format_duration(status.rest_remaining_seconds.unwrap_or(0) as f64)
            ));
        }
        _ => {
            ui.label(format!("{:?}: {progress}", status.state));
        }
    }
    if let Some(result) = &status.result {
        ui.weak(result);
    }
}

fn step_label(step: &CycleStep) -> &'static str {
    match step {
        CycleStep::Device { config, .. } => match config {
            TestConfiguration::DischargeConstantCurrent { .. } => "CC discharge",
            TestConfiguration::DischargeConstantPower { .. } => "CP discharge",
            TestConfiguration::ChargeConstantVoltage { .. } => "CV charge",
        },
        CycleStep::Rest { .. } => "Rest",
    }
}
