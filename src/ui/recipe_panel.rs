use crate::core::{
    CreateSavedRecipeRequest, CycleRecipe, CycleState, CycleStep, CycleStepCompletion,
    DeleteSavedRecipeRequest, RECIPE_EXPORT_FORMAT, RECIPE_EXPORT_VERSION, RecipeExport,
    RenameRequest, SavedRecipe, SavedRecipeReference, StartCycleRequest, TestConfiguration,
    UpdateSavedRecipeRequest, normalize_required_name,
};
use crate::device;
use crate::session::DeviceSession;

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub(crate) struct RecipePanel {
    repeat_count: u32,
    steps: Vec<StepDraft>,
    recipe_name: String,
    execution_name: String,
    local_saved_recipes: Vec<SavedRecipe>,
    #[serde(default = "default_next_local_recipe_id")]
    next_local_recipe_id: u64,
    #[serde(skip)]
    selected_id: Option<String>,
    #[serde(skip)]
    loaded_baseline: Option<LoadedRecipe>,
    #[serde(skip)]
    source_stale: bool,
    #[serde(skip)]
    delete_confirmation: Option<String>,
    #[serde(skip)]
    panel_error: Option<String>,
    #[serde(skip)]
    last_remote_mode: Option<bool>,
    #[cfg(target_arch = "wasm32")]
    #[serde(skip)]
    import_receiver: Option<std::sync::mpsc::Receiver<Result<Option<RecipeExport>, String>>>,
    #[serde(skip)]
    execution_name_edit: String,
    #[serde(skip)]
    synced_execution: Option<(String, Option<String>)>,
}

impl Default for RecipePanel {
    fn default() -> Self {
        Self {
            repeat_count: 1,
            steps: vec![StepDraft::default()],
            recipe_name: String::new(),
            execution_name: String::new(),
            local_saved_recipes: Vec::new(),
            next_local_recipe_id: default_next_local_recipe_id(),
            selected_id: None,
            loaded_baseline: None,
            source_stale: false,
            delete_confirmation: None,
            panel_error: None,
            last_remote_mode: None,
            #[cfg(target_arch = "wasm32")]
            import_receiver: None,
            execution_name_edit: String::new(),
            synced_execution: None,
        }
    }
}

fn default_next_local_recipe_id() -> u64 {
    1
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LoadedRecipe {
    id: String,
    revision: u64,
    name: String,
    recipe: CycleRecipe,
}

#[derive(Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
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
    fn from_step(step: &CycleStep) -> Self {
        match step {
            CycleStep::Device { config, .. } => match *config {
                TestConfiguration::DischargeConstantCurrent {
                    current_ma,
                    cutoff_voltage_mv,
                    cutoff_time_min,
                } => Self::DischargeConstantCurrent {
                    current_ma,
                    cutoff_voltage_mv,
                    cutoff_time_min,
                },
                TestConfiguration::DischargeConstantPower {
                    power_w,
                    cutoff_voltage_mv,
                    cutoff_time_min,
                } => Self::DischargeConstantPower {
                    power_w,
                    cutoff_voltage_mv,
                    cutoff_time_min,
                },
                TestConfiguration::ChargeConstantVoltage {
                    current_ma,
                    voltage_mv,
                    cutoff_current_ma,
                } => Self::ChargeConstantVoltage {
                    current_ma,
                    voltage_mv,
                    cutoff_current_ma,
                },
            },
            CycleStep::Rest { duration_seconds } => Self::Rest {
                duration_seconds: *duration_seconds,
            },
        }
    }

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
    #[expect(
        clippy::too_many_lines,
        reason = "the flat recipe editor keeps its existing controls and ordering together"
    )]
    pub(crate) fn ui(&mut self, session: &mut DeviceSession, ui: &mut egui::Ui) {
        self.reconcile_library(session);
        if let Some(created) = session.take_created_recipe() {
            self.load(&created);
        }
        #[cfg(target_arch = "wasm32")]
        self.poll_import(session);
        if let Some(export) = session.take_recipe_export()
            && let Err(error) = crate::export::save_recipe_to_file(&export)
        {
            self.panel_error = Some(error);
        }

        ui.separator();
        ui.heading("Cycle / Recipe");
        cycle_progress(session, ui);
        self.execution_name_ui(session, ui);

        self.library_ui(session, ui);

        let executing = session.cycle_owns_orchestration();
        ui.add_enabled_ui(!executing, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Recipe name");
                ui.text_edit_singleline(&mut self.recipe_name);
                if self.is_dirty() {
                    ui.weak("Modified");
                }
                if self.source_stale {
                    ui.colored_label(ui.visuals().warn_fg_color, "Source changed or was deleted");
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Execution name (optional)");
                ui.text_edit_singleline(&mut self.execution_name);
            });
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

        self.recipe_actions_ui(session, ui);

        ui.horizontal_wrapped(|ui| {
            let recipe = self.recipe();
            let valid = recipe.validate().is_ok();
            let starts_saved =
                !self.is_dirty() && !self.source_stale && self.loaded_baseline.is_some();
            if ui
                .add_enabled(
                    !executing && session.can_start() && valid,
                    egui::Button::new(if starts_saved {
                        "Start saved recipe"
                    } else {
                        "Start draft"
                    }),
                )
                .clicked()
            {
                let execution_name = Some(self.execution_name.clone());
                if starts_saved && let Some(loaded) = self.loaded_baseline.clone() {
                    if session.is_remote() {
                        session.start_saved_recipe(loaded.id, execution_name);
                    } else {
                        session.start_local_saved_recipe(
                            loaded.recipe,
                            SavedRecipeReference {
                                id: loaded.id,
                                name: loaded.name,
                                revision: loaded.revision,
                            },
                            execution_name,
                        );
                    }
                } else {
                    session.start_cycle(StartCycleRequest {
                        recipe,
                        name: execution_name,
                    });
                }
            }
            if ui
                .add_enabled(executing, egui::Button::new("Stop recipe"))
                .clicked()
            {
                session.stop_cycle();
            }
        });

        if let Some(error) = &self.panel_error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        if let Some(error) = &session.command_error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
    }

    fn recipe(&self) -> CycleRecipe {
        CycleRecipe {
            steps: self.steps.iter().map(StepDraft::step).collect(),
            repeat_count: self.repeat_count,
        }
    }

    fn load(&mut self, saved: &SavedRecipe) {
        self.selected_id = Some(saved.id.clone());
        self.recipe_name.clone_from(&saved.name);
        self.repeat_count = saved.recipe.repeat_count;
        self.steps = saved
            .recipe
            .steps
            .iter()
            .map(StepDraft::from_step)
            .collect();
        self.loaded_baseline = Some(LoadedRecipe {
            id: saved.id.clone(),
            revision: saved.revision,
            name: saved.name.clone(),
            recipe: saved.recipe.clone(),
        });
        self.source_stale = false;
        self.delete_confirmation = None;
        self.panel_error = None;
    }

    fn is_dirty(&self) -> bool {
        self.loaded_baseline
            .as_ref()
            .is_none_or(|loaded| self.recipe_name != loaded.name || self.recipe() != loaded.recipe)
    }

    fn reconcile_library(&mut self, session: &DeviceSession) {
        let remote = session.is_remote();
        if self
            .last_remote_mode
            .replace(remote)
            .is_some_and(|old| old != remote)
        {
            self.selected_id = None;
            self.loaded_baseline = None;
            self.source_stale = false;
            self.delete_confirmation = None;
        }
        if !remote {
            return;
        }
        self.reconcile_remote_recipes(&session.saved_recipes);
    }

    fn reconcile_remote_recipes(&mut self, recipes: &[SavedRecipe]) {
        let Some(loaded) = self.loaded_baseline.clone() else {
            return;
        };
        let canonical = recipes.iter().find(|recipe| recipe.id == loaded.id);
        match canonical {
            Some(canonical)
                if canonical.revision != loaded.revision
                    || canonical.name != loaded.name
                    || canonical.recipe != loaded.recipe =>
            {
                if canonical.name == self.recipe_name && canonical.recipe == self.recipe() {
                    self.load(canonical);
                } else if self.is_dirty() {
                    self.source_stale = true;
                } else {
                    self.load(canonical);
                }
            }
            Some(_) => {}
            None => self.source_stale = true,
        }
    }

    fn library_ui(&mut self, session: &mut DeviceSession, ui: &mut egui::Ui) {
        ui.collapsing("Saved recipes", |ui| {
            if session.is_remote() && ui.button("Refresh library").clicked() {
                session.refresh_recipes();
            }
            let recipes = if session.is_remote() {
                session.saved_recipes.clone()
            } else {
                self.local_saved_recipes.clone()
            };
            if recipes.is_empty() {
                ui.weak(if session.is_remote() {
                    "No recipes on the remote server"
                } else {
                    "No local saved recipes"
                });
            }
            for saved in &recipes {
                let selected = self.selected_id.as_deref() == Some(saved.id.as_str());
                let label = format!(
                    "{}  |  rev {}  |  {} steps x{}",
                    saved.name,
                    saved.revision,
                    saved.recipe.steps.len(),
                    saved.recipe.repeat_count
                );
                if ui.selectable_label(selected, label).clicked() {
                    self.load(saved);
                }
            }
        });
    }

    fn recipe_actions_ui(&mut self, session: &mut DeviceSession, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("Save as new").clicked() {
                self.save_as_new(session);
            }
            if ui
                .add_enabled(
                    self.loaded_baseline.is_some(),
                    egui::Button::new("Update selected"),
                )
                .clicked()
            {
                self.update_selected(session);
            }
            if let Some(selected) = self.selected_id.clone() {
                if self.delete_confirmation.as_ref() == Some(&selected) {
                    if ui.button("Confirm delete").clicked() {
                        self.delete_selected(session);
                    }
                    if ui.button("Cancel").clicked() {
                        self.delete_confirmation = None;
                    }
                } else if ui.button("Delete selected").clicked() {
                    self.delete_confirmation = Some(selected);
                }
                if ui.button("Export selected").clicked() {
                    self.export_selected(session);
                }
            }
            if ui.button("Import recipe").clicked() {
                self.begin_import(session);
            }
        });
    }

    fn validated_draft(&self) -> Result<(String, CycleRecipe), String> {
        let name = normalize_required_name(&self.recipe_name).map_err(|error| error.to_string())?;
        let recipe = self.recipe();
        recipe.validate().map_err(|error| error.to_string())?;
        Ok((name, recipe))
    }

    fn save_as_new(&mut self, session: &mut DeviceSession) {
        let result = self.validated_draft();
        let (name, recipe) = match result {
            Ok(value) => value,
            Err(error) => {
                self.panel_error = Some(error);
                return;
            }
        };
        if session.is_remote() {
            session.create_saved_recipe(CreateSavedRecipeRequest { name, recipe });
            return;
        }
        match self.create_local(name, recipe) {
            Ok(saved) => self.load(&saved),
            Err(error) => self.panel_error = Some(error),
        }
    }

    fn create_local(&mut self, name: String, recipe: CycleRecipe) -> Result<SavedRecipe, String> {
        let id = loop {
            let number = self.next_local_recipe_id;
            self.next_local_recipe_id = number
                .checked_add(1)
                .ok_or_else(|| "local recipe IDs are exhausted".to_owned())?;
            let candidate = format!("local-recipe-{number}");
            if self
                .local_saved_recipes
                .iter()
                .all(|recipe| recipe.id != candidate)
            {
                break candidate;
            }
        };
        let now = timestamp_utc();
        let saved = SavedRecipe {
            id,
            name,
            recipe,
            revision: 1,
            created_at_utc: now.clone(),
            updated_at_utc: now,
        };
        self.local_saved_recipes.push(saved.clone());
        sort_recipes(&mut self.local_saved_recipes);
        Ok(saved)
    }

    fn update_selected(&mut self, session: &mut DeviceSession) {
        let Some(loaded) = self.loaded_baseline.clone() else {
            return;
        };
        let (name, recipe) = match self.validated_draft() {
            Ok(value) => value,
            Err(error) => {
                self.panel_error = Some(error);
                return;
            }
        };
        let request = UpdateSavedRecipeRequest {
            name,
            recipe,
            expected_revision: loaded.revision,
        };
        if session.is_remote() {
            session.update_saved_recipe(loaded.id, request);
            return;
        }
        let Some(saved) = self
            .local_saved_recipes
            .iter_mut()
            .find(|saved| saved.id == loaded.id)
        else {
            self.source_stale = true;
            self.panel_error = Some("selected local recipe no longer exists".to_owned());
            return;
        };
        if saved.revision != request.expected_revision {
            self.source_stale = true;
            self.panel_error = Some(format!(
                "saved recipe revision is {}; expected {}",
                saved.revision, request.expected_revision
            ));
            return;
        }
        let Some(revision) = saved.revision.checked_add(1) else {
            self.panel_error = Some("saved recipe revision is exhausted".to_owned());
            return;
        };
        saved.name = request.name;
        saved.recipe = request.recipe;
        saved.revision = revision;
        saved.updated_at_utc = timestamp_utc();
        let updated = saved.clone();
        sort_recipes(&mut self.local_saved_recipes);
        self.load(&updated);
    }

    fn delete_selected(&mut self, session: &mut DeviceSession) {
        let Some(loaded) = self.loaded_baseline.clone() else {
            return;
        };
        if session.is_remote() {
            session.delete_saved_recipe(
                loaded.id,
                DeleteSavedRecipeRequest {
                    expected_revision: loaded.revision,
                },
            );
            self.delete_confirmation = None;
            return;
        }
        let Some(index) = self
            .local_saved_recipes
            .iter()
            .position(|saved| saved.id == loaded.id)
        else {
            self.panel_error = Some("selected local recipe no longer exists".to_owned());
            return;
        };
        if self.local_saved_recipes[index].revision != loaded.revision {
            self.source_stale = true;
            self.panel_error = Some("selected local recipe has changed".to_owned());
            return;
        }
        self.local_saved_recipes.remove(index);
        self.selected_id = None;
        self.loaded_baseline = None;
        self.source_stale = false;
        self.delete_confirmation = None;
    }

    fn export_selected(&mut self, session: &mut DeviceSession) {
        let Some(loaded) = self.loaded_baseline.clone() else {
            return;
        };
        if session.is_remote() {
            session.export_recipe(loaded.id);
            return;
        }
        let export = RecipeExport {
            format: RECIPE_EXPORT_FORMAT.to_owned(),
            version: RECIPE_EXPORT_VERSION,
            name: loaded.name,
            recipe: loaded.recipe,
        };
        if let Err(error) = crate::export::save_recipe_to_file(&export) {
            self.panel_error = Some(error);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn begin_import(&mut self, session: &mut DeviceSession) {
        match crate::export::load_recipe_from_file() {
            Ok(Some(export)) => self.import_export(session, export),
            Ok(None) => {}
            Err(error) => self.panel_error = Some(error),
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn begin_import(&mut self, _session: &mut DeviceSession) {
        if self.import_receiver.is_none() {
            self.import_receiver = Some(crate::export::load_recipe_from_file());
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn poll_import(&mut self, session: &mut DeviceSession) {
        let result = self
            .import_receiver
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok());
        if let Some(result) = result {
            self.import_receiver = None;
            match result {
                Ok(Some(export)) => self.import_export(session, export),
                Ok(None) => {}
                Err(error) => self.panel_error = Some(error),
            }
        }
    }

    fn import_export(&mut self, session: &mut DeviceSession, export: RecipeExport) {
        if let Err(error) = export.validate() {
            self.panel_error = Some(format!("invalid recipe: {error}"));
            return;
        }
        let name = match normalize_required_name(&export.name) {
            Ok(name) => name,
            Err(error) => {
                self.panel_error = Some(format!("invalid recipe: {error}"));
                return;
            }
        };
        if session.is_remote() {
            session.import_recipe(export);
            return;
        }
        match self.create_local(name, export.recipe) {
            Ok(saved) => self.load(&saved),
            Err(error) => self.panel_error = Some(error),
        }
    }

    fn execution_name_ui(&mut self, session: &mut DeviceSession, ui: &mut egui::Ui) {
        let Some(execution_id) = session.cycle.execution_id.clone() else {
            self.synced_execution = None;
            self.execution_name_edit.clear();
            return;
        };
        let authoritative = (execution_id, session.cycle.name.clone());
        if self.synced_execution.as_ref() != Some(&authoritative) {
            self.execution_name_edit = authoritative.1.clone().unwrap_or_default();
            self.synced_execution = Some(authoritative);
        }
        ui.horizontal_wrapped(|ui| {
            ui.label("Edit execution name");
            ui.text_edit_singleline(&mut self.execution_name_edit);
            if ui.button("Rename").clicked() {
                session.rename_current_cycle(RenameRequest {
                    name: Some(self.execution_name_edit.clone()),
                });
            }
        });
    }
}

fn sort_recipes(recipes: &mut [SavedRecipe]) {
    recipes.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn timestamp_utc() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(target_arch = "wasm32")]
fn timestamp_utc() -> String {
    js_sys::Date::new_0()
        .to_iso_string()
        .as_string()
        .unwrap_or_default()
}

enum StepOperation {
    Up,
    Down,
    Delete,
}

fn cycle_progress(session: &DeviceSession, ui: &mut egui::Ui) {
    let status = &session.cycle;
    if let Some(execution_id) = &status.execution_id {
        ui.strong(format!(
            "Execution: {}",
            status.name.as_deref().unwrap_or(execution_id)
        ));
    }
    if let Some(saved) = &status.saved_recipe {
        ui.weak(format!(
            "Saved recipe: {} (rev {}, {})",
            saved.name, saved.revision, saved.id
        ));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(id: &str, name: &str, revision: u64, recipe: CycleRecipe) -> SavedRecipe {
        SavedRecipe {
            id: id.to_owned(),
            name: name.to_owned(),
            recipe,
            revision,
            created_at_utc: "2026-01-01T00:00:00Z".to_owned(),
            updated_at_utc: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn local_library_and_allocator_survive_persistence() {
        let mut panel = RecipePanel::default();
        let recipe = panel.recipe();
        let Ok(created) = panel.create_local("First".to_owned(), recipe) else {
            panic!("local create failed");
        };
        let Ok(encoded) = serde_json::to_string(&panel) else {
            panic!("panel serialization failed");
        };
        let Ok(restored): Result<RecipePanel, _> = serde_json::from_str(&encoded) else {
            panic!("panel deserialization failed");
        };

        assert_eq!(restored.local_saved_recipes, vec![created]);
        assert_eq!(restored.next_local_recipe_id, 2);
        assert!(restored.selected_id.is_none());
        assert!(restored.loaded_baseline.is_none());
    }

    #[test]
    fn deleted_local_ids_are_not_reused() {
        let mut panel = RecipePanel::default();
        let Ok(first) = panel.create_local("First".to_owned(), panel.recipe()) else {
            panic!("first create failed");
        };
        panel.local_saved_recipes.clear();
        let Ok(second) = panel.create_local("Second".to_owned(), panel.recipe()) else {
            panic!("second create failed");
        };

        assert_eq!(first.id, "local-recipe-1");
        assert_eq!(second.id, "local-recipe-2");
    }

    #[test]
    fn local_import_creates_new_identity() {
        let mut panel = RecipePanel::default();
        let mut session = DeviceSession::default();
        let recipe = panel.recipe();
        panel.import_export(
            &mut session,
            RecipeExport {
                format: RECIPE_EXPORT_FORMAT.to_owned(),
                version: RECIPE_EXPORT_VERSION,
                name: "Imported".to_owned(),
                recipe: recipe.clone(),
            },
        );

        let imported = &panel.local_saved_recipes[0];
        assert_eq!(imported.id, "local-recipe-1");
        assert_eq!(imported.revision, 1);
        assert_eq!(imported.name, "Imported");
        assert_eq!(imported.recipe, recipe);
    }

    #[test]
    fn dirty_editor_survives_remote_update_and_becomes_stale() {
        let mut panel = RecipePanel::default();
        let original = saved("recipe-1", "Original", 1, panel.recipe());
        panel.load(&original);
        panel.recipe_name = "My edits".to_owned();
        let changed = saved("recipe-1", "Server edit", 2, original.recipe);

        panel.reconcile_remote_recipes(&[changed]);

        assert_eq!(panel.recipe_name, "My edits");
        assert!(panel.source_stale);
        assert_eq!(
            panel.loaded_baseline.as_ref().map(|item| item.revision),
            Some(1)
        );
    }

    #[test]
    fn clean_editor_refreshes_from_remote_update() {
        let mut panel = RecipePanel::default();
        let original = saved("recipe-1", "Original", 1, panel.recipe());
        panel.load(&original);
        let mut updated_recipe = original.recipe;
        updated_recipe.repeat_count = 4;
        let changed = saved("recipe-1", "Server edit", 2, updated_recipe);

        panel.reconcile_remote_recipes(&[changed]);

        assert_eq!(panel.recipe_name, "Server edit");
        assert_eq!(panel.repeat_count, 4);
        assert!(!panel.source_stale);
        assert_eq!(
            panel.loaded_baseline.as_ref().map(|item| item.revision),
            Some(2)
        );
    }

    #[test]
    fn matching_remote_update_acknowledges_saved_edits() {
        let mut panel = RecipePanel::default();
        let original = saved("recipe-1", "Original", 1, panel.recipe());
        panel.load(&original);
        panel.recipe_name = "Edited".to_owned();
        let changed = saved("recipe-1", "Edited", 2, original.recipe);

        panel.reconcile_remote_recipes(&[changed]);

        assert!(!panel.is_dirty());
        assert!(!panel.source_stale);
        assert_eq!(
            panel.loaded_baseline.as_ref().map(|item| item.revision),
            Some(2)
        );
    }
}
