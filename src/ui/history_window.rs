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
    Comparison,
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
    run_plot: plot::PlotOptions,
    cycle_plot: plot::PlotOptions,
    comparison_plot: plot::PlotOptions,
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
            let comparison = ui
                .selectable_value(
                    &mut self.view,
                    View::Comparison,
                    format!("Comparison ({})", session.history.comparison_ids.len()),
                )
                .clicked();
            if cycles || runs || comparison {
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
        if self.view == View::Comparison {
            self.comparison(session, ui);
        } else {
            self.history_list(session, ui);
        }
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
                        comparison_button(&run.id, session, ui);
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
            View::Comparison => {}
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
        comparison_button(&run.id, session, ui);
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
        plot::physical_controls(&mut self.run_plot, ui);
        ui.allocate_ui(egui::vec2(ui.available_width(), 280.0), |ui| {
            plot::physical_samples_plot(
                ("history_run", &run.id),
                &history.samples,
                run.config,
                self.run_plot,
                ui,
            );
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
        plot::metric_controls(&mut self.cycle_plot, ui);
        ui.label("X axis: Time");
        ui.allocate_ui(egui::vec2(ui.available_width(), 280.0), |ui| {
            plot::cycle_samples_plot(
                ("history_cycle", &cycle.execution_id),
                &history.samples,
                self.cycle_plot.metric,
                ui,
            );
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
                comparison_button(&run.id, session, ui);
            });
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "comparison cards and graph share one transient view"
    )]
    fn comparison(&mut self, session: &mut DeviceSession, ui: &mut egui::Ui) {
        let ids = session.history.comparison_ids.clone();
        if ids.is_empty() {
            ui.label("Add physical runs from Manual runs or a cycle's child runs to compare them.");
            return;
        }
        ui.heading("Physical run comparison");
        ui.weak(
            "The first selected run is the baseline. Selection and loaded curves are temporary.",
        );
        let summaries: Vec<_> = ids
            .iter()
            .filter_map(|id| {
                session
                    .history
                    .runs
                    .iter()
                    .find(|run| &run.id == id)
                    .or_else(|| {
                        session
                            .history
                            .loaded_cycle
                            .as_ref()?
                            .child_runs
                            .iter()
                            .find(|run| &run.id == id)
                    })
                    .or_else(|| {
                        session
                            .history
                            .comparison_cache
                            .get(id)
                            .map(|history| &history.summary)
                    })
                    .cloned()
            })
            .collect();
        let baseline = summaries.iter().find(|run| run.id == ids[0]);
        if summaries.iter().any(|run| run.config.is_none()) {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "Configuration unavailable for one or more legacy runs.",
            );
        }
        if let Some(first) = summaries.first() {
            if summaries.iter().any(|run| run.config != first.config) {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "Selected runs use different test configurations.",
                );
            }
            if summaries
                .iter()
                .any(|run| config_mode(run.config) != config_mode(first.config))
            {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "Selected runs contain different operation modes.",
                );
            }
        }
        for (index, id) in ids.iter().enumerate() {
            ui.group(|ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.strong(if index == 0 {
                        "Baseline"
                    } else {
                        "Selected run"
                    });
                    if ui.button("Remove").clicked() {
                        session.remove_comparison_run(id);
                    }
                });
                if let Some(run) = summaries.iter().find(|run| &run.id == id) {
                    ui.label(run.name.as_deref().unwrap_or("Physical run"));
                    ui.small(&run.id);
                    if let Some(context) = &run.cycle {
                        let cycle_name = session
                            .history
                            .cycles
                            .iter()
                            .find(|cycle| cycle.execution_id == context.execution_id)
                            .and_then(|cycle| cycle.name.as_deref());
                        ui.label(format!(
                            "Cycle {} · Repeat {} / Step {}",
                            cycle_name.unwrap_or(&context.execution_id),
                            context.repeat_index + 1,
                            context.step_index + 1
                        ));
                        ui.small(&context.execution_id);
                    }
                    ui.label(
                        run.config
                            .map_or_else(|| "Configuration unavailable".to_owned(), configuration),
                    );
                    let mode = config_mode(run.config)
                        .map(str::to_owned)
                        .or_else(|| {
                            session
                                .history
                                .comparison_cache
                                .get(id)
                                .and_then(|history| history.samples.first())
                                .map(|sample| sample.mode.to_string())
                        })
                        .unwrap_or_else(|| "Unavailable".to_owned());
                    ui.label(format!("Mode: {mode}"));
                    ui.label(format!(
                        "Duration: {}",
                        format_duration(run.elapsed_seconds as f64)
                    ));
                    ui.label(format!(
                        "Capacity: {}",
                        run.capacity_mah
                            .map_or_else(|| "Unavailable".to_owned(), |v| format!("{v} mAh"))
                    ));
                    ui.label(format!("Energy: {:.6} Wh", run.energy_wh));
                    if index > 0
                        && let Some(base) = baseline
                    {
                        ui.label(format!(
                            "Capacity Δ: {}",
                            capacity_delta(base.capacity_mah, run.capacity_mah)
                        ));
                        ui.label(format!(
                            "Energy Δ: {}",
                            energy_delta(base.energy_wh, run.energy_wh)
                        ));
                    }
                } else {
                    ui.label("Summary loading; use Refresh to retry if this persists.");
                }
                if !session.history.comparison_cache.contains_key(id) {
                    ui.label(if session.history.pending_requests > 0 {
                        "Curve loading…"
                    } else {
                        "Curve unavailable; use Refresh to retry."
                    });
                }
            });
        }
        if ids.len() < 2 {
            ui.label("Add at least one more physical run to see an overlay.");
            return;
        }
        plot::physical_controls(&mut self.comparison_plot, ui);
        let curves: Vec<_> = ids
            .iter()
            .filter_map(|id| {
                session
                    .history
                    .comparison_cache
                    .get(id)
                    .map(|history| (id.as_str(), history.samples.as_slice()))
            })
            .collect();
        ui.weak("Curves use bounded presentation samples. Duration, capacity and energy above use authoritative run summaries.");
        if curves.len() < ids.len() && session.history.pending_requests > 0 {
            ui.spinner();
        }
        ui.allocate_ui(egui::vec2(ui.available_width(), 280.0), |ui| {
            plot::comparison_plot(("comparison", &ids), &curves, self.comparison_plot, ui);
        });
    }
}

fn comparison_button(id: &str, session: &mut DeviceSession, ui: &mut egui::Ui) {
    if session
        .history
        .comparison_ids
        .iter()
        .any(|selected| selected == id)
    {
        if ui.button("Remove from comparison").clicked() {
            session.remove_comparison_run(id);
        }
    } else {
        let enabled = session.history.comparison_ids.len() < 4;
        if ui
            .add_enabled(enabled, egui::Button::new("Add to comparison"))
            .clicked()
        {
            session.add_comparison_run(id);
        }
        if !enabled {
            ui.weak("Maximum: 4 runs");
        }
    }
}

fn config_mode(config: Option<TestConfiguration>) -> Option<&'static str> {
    match config {
        Some(TestConfiguration::DischargeConstantCurrent { .. }) => Some("CC"),
        Some(TestConfiguration::DischargeConstantPower { .. }) => Some("CP"),
        Some(TestConfiguration::ChargeConstantVoltage { .. }) => Some("CV"),
        None => None,
    }
}

// Presentation samples are bounded, so exact deltas must use RunSummary fields.
fn capacity_delta(base: Option<u64>, value: Option<u64>) -> String {
    match (base, value) {
        (Some(base), Some(value)) => {
            let delta = i128::from(value) - i128::from(base);
            if base == 0 {
                format!("{delta:+} mAh (percentage unavailable)")
            } else {
                format!(
                    "{delta:+} mAh ({:+.2}%)",
                    delta as f64 / base as f64 * 100.0
                )
            }
        }
        _ => "Unavailable".to_owned(),
    }
}

fn energy_delta(base: f64, value: f64) -> String {
    let delta = value - base;
    if base == 0.0 {
        format!("{delta:+.6} Wh (percentage unavailable)")
    } else {
        format!("{delta:+.6} Wh ({:+.2}%)", delta / base * 100.0)
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
    fn summary_deltas_handle_sign_missing_and_zero_baseline() {
        assert_eq!(capacity_delta(Some(100), Some(125)), "+25 mAh (+25.00%)");
        assert_eq!(capacity_delta(Some(100), Some(75)), "-25 mAh (-25.00%)");
        assert_eq!(capacity_delta(Some(100), Some(100)), "+0 mAh (+0.00%)");
        assert_eq!(capacity_delta(None, Some(100)), "Unavailable");
        assert_eq!(
            capacity_delta(Some(0), Some(10)),
            "+10 mAh (percentage unavailable)"
        );
        assert_eq!(energy_delta(10.0, 9.5), "-0.500000 Wh (-5.00%)");
        assert_eq!(
            energy_delta(0.0, 1.0),
            "+1.000000 Wh (percentage unavailable)"
        );
    }

    #[test]
    fn configuration_mode_warnings_can_distinguish_modes_and_missing_legacy_data() {
        let cc = Some(TestConfiguration::DischargeConstantCurrent {
            current_ma: 1000,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 10,
        });
        let cc_other = Some(TestConfiguration::DischargeConstantCurrent {
            current_ma: 2000,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 10,
        });
        let cp = Some(TestConfiguration::DischargeConstantPower {
            power_w: 5,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 10,
        });
        assert_eq!(cc, cc);
        assert_ne!(cc, cc_other);
        assert_eq!(config_mode(cc), config_mode(cc_other));
        assert_ne!(config_mode(cc), config_mode(cp));
        assert_eq!(config_mode(None), None);
    }

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
