use super::{format_duration, plot};
use crate::backend::HistoryRequest;
use crate::core::{
    CycleHistory, CycleStep, CycleSummary, RenameRequest, RunHistory, RunSummary, TestConfiguration,
};
use crate::session::DeviceSession;

#[derive(Default, PartialEq, Eq)]
enum View {
    #[default]
    Cycles,
    ManualRuns,
}

#[derive(Default, PartialEq, Eq)]
enum Detail {
    #[default]
    List,
    Cycle,
    Run,
    Child,
}

#[derive(Default)]
pub(crate) struct HistoryWindow {
    pub open: bool,
    view: View,
    detail: Detail,
    filter: String,
    name_draft: Option<String>,
}

fn manual_matches(run: &RunSummary, filter: &str) -> bool {
    run.cycle.is_none()
        && (run.id.to_lowercase().contains(filter)
            || run
                .name
                .as_deref()
                .unwrap_or_default()
                .to_lowercase()
                .contains(filter))
}

fn cycle_matches(cycle: &CycleSummary, filter: &str) -> bool {
    cycle.execution_id.to_lowercase().contains(filter)
        || cycle
            .name
            .as_deref()
            .unwrap_or_default()
            .to_lowercase()
            .contains(filter)
        || cycle
            .saved_recipe
            .as_ref()
            .is_some_and(|recipe| recipe.name.to_lowercase().contains(filter))
}

impl HistoryWindow {
    pub(crate) fn open(&mut self, session: &mut DeviceSession) {
        self.open = true;
        session.refresh_history();
    }

    pub(crate) fn ui(&mut self, session: &mut DeviceSession, ui: &egui::Ui) {
        if let Some(file) = session.history.pending_export.take()
            && let Err(error) = crate::export::save_downloaded_file(&file)
        {
            session.history.error = Some(error);
        }
        let mut open = self.open;
        let size = ui.ctx().content_rect().size();
        egui::Window::new("History")
            .open(&mut open)
            .default_width(700.0)
            .min_width(240.0)
            .max_width((size.x - 24.0).max(240.0))
            .max_height((size.y - 40.0).max(200.0))
            .vscroll(true)
            .show(ui.ctx(), |ui| self.contents(session, ui));
        self.open = open;
    }

    fn contents(&mut self, session: &mut DeviceSession, ui: &mut egui::Ui) {
        if !session.is_remote() {
            ui.label("Persistent history is stored by the server backend. Connect using Remote mode to browse archived runs and cycles.");
            return;
        }
        ui.horizontal_wrapped(|ui| {
            let cycles = ui
                .selectable_value(&mut self.view, View::Cycles, "Cycles")
                .clicked();
            let runs = ui
                .selectable_value(&mut self.view, View::ManualRuns, "Manual runs")
                .clicked();
            if cycles || runs {
                self.detail = Detail::List;
                self.name_draft = None;
            }
            if ui.button("Refresh").clicked() {
                session.refresh_history();
            }
            if session.history.pending_requests > 0 {
                ui.spinner();
            }
        });
        if let Some(error) = session
            .history
            .error
            .as_ref()
            .or(session.command_error.as_ref())
        {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        ui.separator();
        if self.detail != Detail::List {
            if ui
                .button(if self.detail == Detail::Child {
                    "← Cycle"
                } else {
                    "← History list"
                })
                .clicked()
            {
                self.detail = if self.detail == Detail::Child {
                    Detail::Cycle
                } else {
                    Detail::List
                };
                self.name_draft = None;
            }
            match self.detail {
                Detail::Cycle => {
                    if let Some(history) = session.history.loaded_cycle.take() {
                        self.cycle_detail(&history, session, ui);
                        session.history.loaded_cycle = Some(history);
                    } else {
                        ui.label("Cycle detail is not loaded. Use Refresh to retry.");
                    }
                }
                Detail::Run | Detail::Child => {
                    if let Some(history) = session.history.loaded_run.take() {
                        self.run_detail(&history, session, ui);
                        session.history.loaded_run = Some(history);
                    } else {
                        ui.label("Run detail is not loaded. Use Refresh to retry.");
                    }
                }
                Detail::List => {}
            }
            if self.detail != Detail::List {
                return;
            }
        }
        self.history_list(session, ui);
    }

    fn history_list(&mut self, session: &mut DeviceSession, ui: &mut egui::Ui) {
        ui.add(
            egui::TextEdit::singleline(&mut self.filter)
                .hint_text("Search names or IDs")
                .desired_width(f32::INFINITY),
        );
        let filter = self.filter.trim().to_lowercase();
        match self.view {
            View::ManualRuns => {
                let runs: Vec<_> = session
                    .history
                    .runs
                    .iter()
                    .filter(|run| manual_matches(run, &filter))
                    .cloned()
                    .collect();
                ui.weak(format!("{} manual runs", runs.len()));
                for run in runs {
                    ui.group(|ui| {
                        if ui.button(run.name.as_deref().unwrap_or(&run.id)).clicked() {
                            session.history_request(HistoryRequest::LoadRun(run.id.clone()));
                            self.detail = Detail::Run;
                            self.name_draft = None;
                        }
                        ui.small(&run.id);
                        ui.label(
                            run.started_at_utc
                                .as_deref()
                                .unwrap_or("Start time unknown"),
                        );
                        run_metrics(&run, ui);
                    });
                }
            }
            View::Cycles => {
                let cycles: Vec<_> = session
                    .history
                    .cycles
                    .iter()
                    .filter(|cycle| cycle_matches(cycle, &filter))
                    .cloned()
                    .collect();
                ui.weak(format!("{} cycles", cycles.len()));
                for cycle in cycles {
                    ui.group(|ui| {
                        if ui
                            .button(cycle.name.as_deref().unwrap_or(&cycle.execution_id))
                            .clicked()
                        {
                            session.history_request(HistoryRequest::LoadCycle(
                                cycle.execution_id.clone(),
                            ));
                            self.detail = Detail::Cycle;
                            self.name_draft = None;
                        }
                        ui.small(&cycle.execution_id);
                        cycle_metrics(&cycle, ui);
                    });
                }
            }
        }
    }

    fn rename(
        &mut self,
        id: &str,
        name: Option<&str>,
        cycle: bool,
        session: &mut DeviceSession,
        ui: &mut egui::Ui,
    ) {
        if let Some(draft) = &mut self.name_draft {
            ui.add(
                egui::TextEdit::singleline(draft)
                    .hint_text("Name (empty clears it)")
                    .desired_width(f32::INFINITY),
            );
            let request = RenameRequest {
                name: Some(draft.clone()),
            };
            ui.horizontal_wrapped(|ui| {
                if ui.button("Save name").clicked() {
                    if cycle {
                        session.rename_cycle(id.to_owned(), request);
                    } else {
                        session.rename_run(id.to_owned(), request);
                    }
                    self.name_draft = None;
                }
                if ui.button("Cancel").clicked() {
                    self.name_draft = None;
                }
            });
        } else if ui
            .button(if cycle { "Rename cycle" } else { "Rename" })
            .clicked()
        {
            self.name_draft = Some(name.unwrap_or_default().to_owned());
        }
    }

    fn run_detail(&mut self, history: &RunHistory, session: &mut DeviceSession, ui: &mut egui::Ui) {
        let run = &history.summary;
        ui.heading(run.name.as_deref().unwrap_or("Physical run"));
        ui.label(format!("Run ID: {}", run.id));
        ui.label(format!(
            "Started: {}",
            run.started_at_utc.as_deref().unwrap_or("Unknown")
        ));
        ui.label(format!("Archived: {}", run.archived_at_utc));
        if let Some(context) = &run.cycle {
            ui.label(format!(
                "Cycle: {} · Repeat {} / Step {}",
                context.execution_id,
                context.repeat_index + 1,
                context.step_index + 1
            ));
        }
        run_metrics(run, ui);
        if let Some(model) = &run.model {
            ui.label(format!("Model: {model}"));
        }
        if let Some(firmware) = &run.firmware_version {
            ui.label(format!("Firmware: {firmware}"));
        }
        if run.cycle.is_none() {
            self.rename(&run.id, run.name.as_deref(), false, session, ui);
        }
        if ui.button("Export CSV").clicked() {
            session.history_request(HistoryRequest::ExportRunCsv(run.id.clone()));
        }
        ui.weak("Plots use bounded presentation samples. CSV contains full resolution telemetry.");
        ui.allocate_ui(egui::vec2(ui.available_width(), 280.0), |ui| {
            plot::physical_samples_plot(("history_run", &run.id), &history.samples, ui);
        });
    }

    fn cycle_detail(
        &mut self,
        history: &CycleHistory,
        session: &mut DeviceSession,
        ui: &mut egui::Ui,
    ) {
        let cycle = &history.summary;
        ui.heading(cycle.name.as_deref().unwrap_or("Cycle execution"));
        ui.label(format!("Execution ID: {}", cycle.execution_id));
        cycle_metrics(cycle, ui);
        ui.label(format!("{} telemetry samples", cycle.sample_count));
        self.rename(
            &cycle.execution_id,
            cycle.name.as_deref(),
            true,
            session,
            ui,
        );
        if ui.button("Export whole-cycle CSV").clicked() {
            session.history_request(HistoryRequest::ExportCycleCsv(cycle.execution_id.clone()));
        }
        if let Some(source) = &cycle.saved_recipe {
            ui.label(format!(
                "Saved recipe: {} · revision {}",
                source.name, source.revision
            ));
            ui.label(format!("Recipe ID: {}", source.id));
        } else {
            ui.weak("No saved-recipe provenance recorded");
        }
        ui.collapsing("Recipe snapshot", |ui| {
            if let Some(recipe) = &cycle.recipe {
                ui.label(format!("{} repeats", recipe.repeat_count));
                for (index, step) in recipe.steps.iter().enumerate() {
                    ui.label(format!(
                        "Step {}: {}",
                        index + 1,
                        match step {
                            CycleStep::Device { config, .. } =>
                                format!("{} · hardware completion", configuration(*config)),
                            CycleStep::Rest { duration_seconds } =>
                                format!("Rest {}", format_duration(*duration_seconds as f64)),
                        }
                    ));
                }
            } else {
                ui.label("Recipe snapshot unavailable for this legacy execution");
            }
        });
        ui.weak("Plots use bounded presentation samples. CSV contains full resolution telemetry.");
        ui.allocate_ui(egui::vec2(ui.available_width(), 280.0), |ui| {
            plot::cycle_samples_plot(("history_cycle", &cycle.execution_id), &history.samples, ui);
        });
        ui.heading("Child physical runs");
        if history.child_runs.is_empty() {
            ui.label("No archived physical runs yet");
        }
        for run in &history.child_runs {
            ui.group(|ui| {
                if let Some(context) = &run.cycle
                    && ui
                        .button(format!(
                            "Repeat {} / Step {}",
                            context.repeat_index + 1,
                            context.step_index + 1
                        ))
                        .clicked()
                {
                    session.history_request(HistoryRequest::LoadRun(run.id.clone()));
                    self.detail = Detail::Child;
                    self.name_draft = None;
                }
                ui.small(&run.id);
                run_metrics(run, ui);
            });
        }
    }
}

fn run_metrics(run: &RunSummary, ui: &mut egui::Ui) {
    if let Some(config) = run.config {
        ui.label(configuration(config));
    }
    ui.label(format!(
        "{:?} · {}",
        run.state,
        format_duration(run.elapsed_seconds as f64)
    ));
    if let Some(result) = &run.result {
        ui.label(result);
    }
    ui.label(format!(
        "{} mAh · {:.3} Wh · {} samples",
        run.capacity_mah
            .map_or_else(|| "Unknown".to_owned(), |value| value.to_string()),
        run.energy_wh,
        run.sample_count
    ));
}

fn cycle_metrics(cycle: &CycleSummary, ui: &mut egui::Ui) {
    ui.label(
        cycle
            .started_at_utc
            .as_deref()
            .unwrap_or("Start time unknown"),
    );
    ui.label(format!(
        "{} · {} · {} child runs",
        cycle
            .state
            .map_or_else(|| "State unknown".to_owned(), |state| format!("{state:?}")),
        cycle.elapsed_milliseconds.map_or_else(
            || "Duration unknown".to_owned(),
            |value| format_duration(value as f64 / 1000.0)
        ),
        cycle.child_run_count
    ));
    if let Some(result) = &cycle.result {
        ui.label(result);
    }
    if let Some(source) = &cycle.saved_recipe {
        ui.label(format!("{} · revision {}", source.name, source.revision));
    }
    if let Some(recipe) = &cycle.recipe {
        ui.label(format!(
            "{} steps × {} repeats",
            recipe.steps.len(),
            recipe.repeat_count
        ));
    }
}

fn configuration(config: TestConfiguration) -> String {
    match config {
        TestConfiguration::DischargeConstantCurrent {
            current_ma,
            cutoff_voltage_mv,
            cutoff_time_min,
        } => format!(
            "CC discharge · {current_ma} mA · cutoff {cutoff_voltage_mv} mV / {cutoff_time_min} min"
        ),
        TestConfiguration::DischargeConstantPower {
            power_w,
            cutoff_voltage_mv,
            cutoff_time_min,
        } => format!(
            "CP discharge · {power_w} W · cutoff {cutoff_voltage_mv} mV / {cutoff_time_min} min"
        ),
        TestConfiguration::ChargeConstantVoltage {
            current_ma,
            voltage_mv,
            cutoff_current_ma,
        } => {
            format!("CV charge · {current_ma} mA / {voltage_mv} mV · cutoff {cutoff_current_ma} mA")
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "history presentation fixtures fail fast"
)]
mod tests {
    use super::*;
    use crate::core::{CycleRunContext, SavedRecipeReference};

    #[test]
    fn top_level_history_filters_manual_runs_and_searches_names_ids_and_provenance() {
        let mut run: RunSummary = serde_json::from_value(serde_json::json!({
            "id": "Run-123", "name": "My BATTERY", "archived_at_utc": "now", "state": "completed", "elapsed_seconds": 1, "sample_count": 1
        })).expect("run");
        assert!(manual_matches(&run, "battery"));
        assert!(manual_matches(&run, "run-123"));
        assert!(!manual_matches(&run, "other"));
        run.cycle = Some(CycleRunContext {
            execution_id: "Cycle-456".to_owned(),
            repeat_index: 0,
            step_index: 0,
        });
        assert!(!manual_matches(&run, ""));
        let mut cycle: CycleSummary = serde_json::from_value(serde_json::json!({
            "execution_id": "Cycle-456", "name": "Formation", "sample_count": 1, "child_run_count": 1
        })).expect("cycle");
        cycle.saved_recipe = Some(SavedRecipeReference {
            id: "template".to_owned(),
            name: "Saved TEMPLATE".to_owned(),
            revision: 2,
        });
        assert!(cycle_matches(&cycle, "formation"));
        assert!(cycle_matches(&cycle, "cycle-456"));
        assert!(cycle_matches(&cycle, "template"));
        assert!(!cycle_matches(&cycle, "other"));
    }
}
