use super::{format_cycle_state, format_duration, format_test_state, format_timestamp, plot};
use crate::backend::{BackendConnectionStatus, HistoryRequest};
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
    rename_pending: Option<(String, Option<String>)>,
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
        while let Some((request, file)) = session.history.pending_exports.pop_front() {
            if let Err(error) = crate::export::save_downloaded_file(&file) {
                session.history.errors.insert(request, error);
            }
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
        });
        ui.horizontal_wrapped(|ui| {
            if self.view != View::Comparison {
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter)
                        .hint_text("Search names or IDs")
                        .desired_width(ui.available_width().clamp(120.0, 360.0)),
                );
            }
            if ui
                .button("Refresh")
                .on_hover_text("Refresh lists, open detail, and selected comparison curves")
                .clicked()
            {
                session.refresh_history();
            }
            if [HistoryRequest::RefreshRuns, HistoryRequest::RefreshCycles]
                .iter()
                .any(|request| session.history.pending(request))
            {
                ui.spinner();
                ui.weak("Refreshing lists…");
            }
        });
        if let Some(error) = session.history.connection_error.as_ref() {
            ui.colored_label(ui.visuals().error_fg_color, error);
        } else if session.remote_status != BackendConnectionStatus::Connected {
            ui.colored_label(ui.visuals().warn_fg_color, "Server is disconnected; loaded history remains available. Reconnect to refresh or retry.");
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
                    } else if let Some(id) = session.history.selected_cycle.clone() {
                        detail_status(HistoryRequest::LoadCycle(id), "cycle", session, ui);
                    }
                }
                Detail::Run | Detail::Child => {
                    if let Some(history) = session.history.loaded_run.take() {
                        self.run_detail(&history, session, ui);
                        session.history.loaded_run = Some(history);
                    } else if let Some(id) = session.history.selected_run.clone() {
                        detail_status(HistoryRequest::LoadRun(id), "run", session, ui);
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
                list_status(
                    session,
                    &HistoryRequest::RefreshRuns,
                    "manual runs",
                    runs.is_empty(),
                    &self.filter,
                    ui,
                );
                for run in runs {
                    ui.group(|ui| {
                        ui.set_min_width(ui.available_width());
                        ui.strong(run.name.as_deref().unwrap_or(&run.id));
                        if run.name.is_some() {
                            ui.small(&run.id);
                        }
                        ui.weak(
                            run.started_at_utc
                                .as_deref()
                                .map_or_else(|| "Start time unknown".to_owned(), format_timestamp),
                        );
                        run_metrics(&run, ui);
                        ui.horizontal_wrapped(|ui| {
                            if ui.button("Open").clicked() {
                                session.history_request(HistoryRequest::LoadRun(run.id.clone()));
                                self.detail = Detail::Run;
                                self.name_draft = None;
                            }
                            comparison_button(&run.id, session, ui);
                        });
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
                list_status(
                    session,
                    &HistoryRequest::RefreshCycles,
                    "cycles",
                    cycles.is_empty(),
                    &self.filter,
                    ui,
                );
                for cycle in cycles {
                    ui.group(|ui| {
                        ui.set_min_width(ui.available_width());
                        ui.strong(cycle.name.as_deref().unwrap_or(&cycle.execution_id));
                        if cycle.name.is_some() {
                            ui.small(&cycle.execution_id);
                        }
                        cycle_metrics(&cycle, ui);
                        if ui.button("Open").clicked() {
                            session.history_request(HistoryRequest::LoadCycle(
                                cycle.execution_id.clone(),
                            ));
                            self.detail = Detail::Cycle;
                            self.name_draft = None;
                        }
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
        if self
            .rename_pending
            .as_ref()
            .is_some_and(|(pending_id, expected)| pending_id == id && name == expected.as_deref())
        {
            self.rename_pending = None;
            self.name_draft = None;
        }
        if let Some(draft) = &mut self.name_draft {
            let response = ui.add(
                egui::TextEdit::singleline(draft)
                    .hint_text("Name (empty clears it)")
                    .desired_width(220.0),
            );
            let save_key =
                response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            let cancel_key = ui.input(|input| input.key_pressed(egui::Key::Escape));
            let draft_text = draft.clone();
            let (save_click, cancel_click) = ui
                .horizontal_wrapped(|ui| {
                    (
                        ui.button("Save name").clicked(),
                        ui.button("Cancel").clicked(),
                    )
                })
                .inner;
            if save_click || save_key {
                self.rename_pending = crate::core::normalize_optional_name(Some(&draft_text))
                    .ok()
                    .map(|name| (id.to_owned(), name));
                session.command_error = None;
                let request = RenameRequest {
                    name: Some(draft_text),
                };
                if cycle {
                    session.rename_cycle(id.to_owned(), request);
                } else {
                    session.rename_run(id.to_owned(), request);
                }
            }
            if cancel_click || cancel_key {
                self.name_draft = None;
                self.rename_pending = None;
            }
            if self
                .rename_pending
                .as_ref()
                .is_some_and(|(pending_id, _)| pending_id == id)
            {
                if let Some(error) = &session.command_error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                } else {
                    ui.weak("Saving name…");
                }
            }
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
        ui.small(format!("Run ID: {}", run.id));
        ui.label(format!(
            "Started: {}",
            run.started_at_utc
                .as_deref()
                .map_or_else(|| "Unknown".to_owned(), format_timestamp)
        ));
        ui.label(format!(
            "Archived: {}",
            format_timestamp(&run.archived_at_utc)
        ));
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
        let load = HistoryRequest::LoadRun(run.id.clone());
        request_feedback(&load, session, ui);
        let export = HistoryRequest::ExportRunCsv(run.id.clone());
        ui.horizontal_wrapped(|ui| {
            if run.cycle.is_none() {
                self.rename(&run.id, run.name.as_deref(), false, session, ui);
            }
            comparison_button(&run.id, session, ui);
            if ui
                .add_enabled(
                    !session.history.pending(&export),
                    egui::Button::new(if session.history.pending(&export) {
                        "Exporting…"
                    } else {
                        "Export CSV"
                    }),
                )
                .clicked()
            {
                session.history_request(export.clone());
            }
        });
        request_feedback(&export, session, ui);
        ui.weak("Plots use bounded presentation samples. CSV contains full resolution telemetry.");
        plot::physical_controls(&mut self.run_plot, ui);
        if history.samples.is_empty() {
            ui.weak("No telemetry samples are available for this run.");
        } else {
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
    }

    fn cycle_detail(
        &mut self,
        history: &CycleHistory,
        session: &mut DeviceSession,
        ui: &mut egui::Ui,
    ) {
        let cycle = &history.summary;
        ui.heading(cycle.name.as_deref().unwrap_or("Cycle execution"));
        ui.small(format!("Execution ID: {}", cycle.execution_id));
        cycle_metrics(cycle, ui);
        ui.label(format!("{} telemetry samples", cycle.sample_count));
        let load = HistoryRequest::LoadCycle(cycle.execution_id.clone());
        request_feedback(&load, session, ui);
        let export = HistoryRequest::ExportCycleCsv(cycle.execution_id.clone());
        ui.horizontal_wrapped(|ui| {
            self.rename(
                &cycle.execution_id,
                cycle.name.as_deref(),
                true,
                session,
                ui,
            );
            if ui
                .add_enabled(
                    !session.history.pending(&export),
                    egui::Button::new(if session.history.pending(&export) {
                        "Exporting…"
                    } else {
                        "Export whole-cycle CSV"
                    }),
                )
                .clicked()
            {
                session.history_request(export.clone());
            }
        });
        request_feedback(&export, session, ui);
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
        if history.samples.is_empty() {
            ui.weak("No whole-cycle telemetry samples are available.");
        } else {
            ui.allocate_ui(egui::vec2(ui.available_width(), 280.0), |ui| {
                plot::cycle_samples_plot(
                    ("history_cycle", &cycle.execution_id),
                    &history.samples,
                    self.cycle_plot.metric,
                    ui,
                );
            });
        }
        ui.heading("Child physical runs");
        if history.child_runs.is_empty() {
            ui.label("No archived physical runs yet");
        }
        for run in &history.child_runs {
            ui.group(|ui| {
                ui.set_min_width(ui.available_width());
                if let Some(context) = &run.cycle {
                    ui.strong(format!(
                        "Repeat {} / Step {}",
                        context.repeat_index + 1,
                        context.step_index + 1
                    ));
                }
                ui.small(&run.id);
                run_metrics(run, ui);
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Open").clicked() {
                        session.history_request(HistoryRequest::LoadRun(run.id.clone()));
                        self.detail = Detail::Child;
                        self.name_draft = None;
                    }
                    comparison_button(&run.id, session, ui);
                });
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
            ui.heading("No runs selected for comparison");
            ui.label("Add a manual run or a cycle child using ‘Add to comparison’.");
            return;
        }
        ui.heading("Physical run comparison");
        ui.horizontal_wrapped(|ui| {
            ui.weak("Selection and curves are temporary.");
            if ui.button("Clear comparison").clicked() {
                session.clear_comparison();
            }
        });
        let summaries: Vec<_> = ids
            .iter()
            .filter_map(|id| {
                session
                    .history
                    .comparison_cache
                    .get(id)
                    .map(|history| &history.summary)
                    .or_else(|| session.history.runs.iter().find(|run| &run.id == id))
                    .or_else(|| {
                        session
                            .history
                            .loaded_cycle
                            .as_ref()?
                            .child_runs
                            .iter()
                            .find(|run| &run.id == id)
                    })
                    .cloned()
            })
            .collect();
        let baseline = summaries.iter().find(|run| run.id == ids[0]);
        let labels = comparison_labels(&summaries, &session.history.cycles);
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
                ui.set_min_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.strong(if index == 0 {
                        "Baseline"
                    } else {
                        "Selected run"
                    });
                    if index > 0 && ui.button("Use as baseline").clicked() {
                        session.use_comparison_baseline(id);
                    }
                    if ui.button("Remove").clicked() {
                        session.remove_comparison_run(id);
                    }
                });
                if let Some(run) = summaries.iter().find(|run| &run.id == id) {
                    ui.strong(labels.get(id).map(String::as_str).unwrap_or("Physical run"));
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
                    ui.label(format!("Energy: {} Wh", format_energy(run.energy_wh)));
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
                    ui.label("Summary unavailable; refresh the run list or retry its curve.");
                }
                if !session.history.comparison_cache.contains_key(id) {
                    let request = HistoryRequest::LoadRun(id.clone());
                    detail_status(request, "run telemetry", session, ui);
                } else {
                    request_feedback(&HistoryRequest::LoadRun(id.clone()), session, ui);
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
                    .filter(|history| !history.samples.is_empty())
                    .map(|history| plot::ComparisonCurve {
                        key: id,
                        label: labels.get(id).map_or(id.as_str(), String::as_str),
                        samples: &history.samples,
                    })
            })
            .collect();
        ui.weak("Curves use bounded presentation samples. Duration, capacity and energy above use authoritative run summaries.");
        if curves.is_empty() {
            ui.weak("No comparison telemetry is available yet.");
        } else {
            ui.allocate_ui(egui::vec2(ui.available_width(), 280.0), |ui| {
                plot::comparison_plot(("comparison", &ids), &curves, self.comparison_plot, ui);
            });
        }
    }
}

fn list_status(
    session: &mut DeviceSession,
    request: &HistoryRequest,
    kind: &str,
    empty: bool,
    filter: &str,
    ui: &mut egui::Ui,
) {
    if let Some(error) = session.history.error(request).map(str::to_owned) {
        ui.colored_label(
            ui.visuals().error_fg_color,
            format!("Could not refresh {kind}: {error}"),
        );
        if ui.button("Retry list").clicked() {
            session.history_request(request.clone());
        }
    }
    if empty {
        if !filter.trim().is_empty() {
            ui.label(format!("No {kind} match “{}”.", filter.trim()));
        } else if session.history.pending(request) {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("Loading {kind}…"));
            });
        } else {
            ui.label(format!("No archived {kind} yet."));
        }
    }
}

fn detail_status(
    request: HistoryRequest,
    what: &str,
    session: &mut DeviceSession,
    ui: &mut egui::Ui,
) {
    if session.history.pending(&request) {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!("Loading {what}…"));
        });
    } else {
        if let Some(error) = session.history.error(&request) {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("Could not load {what}: {error}"),
            );
        } else {
            ui.label(format!("{what} is not loaded."));
        }
        if ui.button(format!("Retry {what}")).clicked() {
            session.history_request(request);
        }
    }
}

fn request_feedback(request: &HistoryRequest, session: &mut DeviceSession, ui: &mut egui::Ui) {
    if session.history.pending(request) {
        ui.weak("Refreshing…");
    }
    if let Some(error) = session.history.error(request).map(str::to_owned) {
        ui.colored_label(ui.visuals().error_fg_color, error);
        if ui.button("Retry").clicked() {
            session.history_request(request.clone());
        }
    }
}

fn comparison_labels(
    runs: &[RunSummary],
    cycles: &[CycleSummary],
) -> std::collections::BTreeMap<String, String> {
    let bases: Vec<_> = runs
        .iter()
        .map(|run| {
            let label = if let Some(name) = &run.name {
                name.clone()
            } else if let Some(context) = &run.cycle {
                let parent = cycles
                    .iter()
                    .find(|cycle| cycle.execution_id == context.execution_id)
                    .and_then(|cycle| cycle.name.as_deref())
                    .unwrap_or(&context.execution_id);
                format!(
                    "{parent} · R{}/S{}",
                    context.repeat_index + 1,
                    context.step_index + 1
                )
            } else {
                run.id.clone()
            };
            (run.id.clone(), label)
        })
        .collect();
    let mut labels: std::collections::BTreeMap<_, _> = bases
        .iter()
        .map(|(id, label)| {
            let duplicate = bases.iter().filter(|(_, other)| other == label).count() > 1;
            (
                id.clone(),
                if duplicate {
                    format!("{label} · {id}")
                } else {
                    label.clone()
                },
            )
        })
        .collect();
    // A user name can itself equal another curve's suffixed label. Repeat
    // disambiguation until the final legend/tooltip labels are unique.
    while labels
        .values()
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        != labels.len()
    {
        let current = labels.clone();
        for (id, label) in &current {
            if current.values().filter(|other| *other == label).count() > 1 {
                labels.insert(id.clone(), format!("{label} · {id}"));
            }
        }
    }
    labels
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
        format!(
            "{} Wh (percentage unavailable)",
            format_signed_energy(delta)
        )
    } else {
        format!(
            "{} Wh ({:+.2}%)",
            format_signed_energy(delta),
            delta / base * 100.0
        )
    }
}

fn format_energy(value: f64) -> String {
    if value != 0.0 && value.abs() < 0.001 {
        format!("{value:.6}")
    } else {
        format!("{value:.3}")
    }
}

fn format_signed_energy(value: f64) -> String {
    if value != 0.0 && value.abs() < 0.001 {
        format!("{value:+.6}")
    } else {
        format!("{value:+.3}")
    }
}

fn run_metrics(run: &RunSummary, ui: &mut egui::Ui) {
    if let Some(config) = run.config {
        ui.label(configuration(config));
    }
    ui.label(format!(
        "{} · {}",
        format_test_state(&run.state),
        format_duration(run.elapsed_seconds as f64)
    ));
    if let Some(result) = &run.result {
        ui.label(result);
    }
    ui.label(format!(
        "{} mAh · {} Wh",
        run.capacity_mah
            .map_or_else(|| "Unknown".to_owned(), |value| value.to_string()),
        format_energy(run.energy_wh),
    ));
    ui.small(format!("{} samples", run.sample_count));
}

fn cycle_metrics(cycle: &CycleSummary, ui: &mut egui::Ui) {
    ui.label(
        cycle
            .started_at_utc
            .as_deref()
            .map_or_else(|| "Start time unknown".to_owned(), format_timestamp),
    );
    ui.label(format!(
        "{} · {} · {} child runs",
        cycle.state.map_or_else(
            || "State unknown".to_owned(),
            |state| format_cycle_state(state).to_owned()
        ),
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
            "CC discharge · {:.3} A · cutoff {:.3} V / {cutoff_time_min} min",
            f64::from(current_ma) / 1000.0,
            f64::from(cutoff_voltage_mv) / 1000.0
        ),
        TestConfiguration::DischargeConstantPower {
            power_w,
            cutoff_voltage_mv,
            cutoff_time_min,
        } => format!(
            "CP discharge · {power_w} W · cutoff {:.3} V / {cutoff_time_min} min",
            f64::from(cutoff_voltage_mv) / 1000.0
        ),
        TestConfiguration::ChargeConstantVoltage {
            current_ma,
            voltage_mv,
            cutoff_current_ma,
        } => {
            format!(
                "CV charge · {:.3} A / {:.3} V · cutoff {:.3} A",
                f64::from(current_ma) / 1000.0,
                f64::from(voltage_mv) / 1000.0,
                f64::from(cutoff_current_ma) / 1000.0
            )
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

    fn run(id: &str, name: Option<&str>, cycle: Option<CycleRunContext>) -> RunSummary {
        let mut run: RunSummary = serde_json::from_value(serde_json::json!({"id":id,"archived_at_utc":"now","state":"completed","elapsed_seconds":1,"sample_count":0})).expect("run");
        run.name = name.map(str::to_owned);
        run.cycle = cycle;
        run
    }

    #[test]
    fn comparison_labels_use_names_context_and_unambiguous_ids() {
        let context = |execution_id: &str| CycleRunContext {
            execution_id: execution_id.to_owned(),
            repeat_index: 1,
            step_index: 2,
        };
        let cycles = vec![serde_json::from_value(serde_json::json!({"execution_id":"cycle-named","name":"Formation","sample_count":0,"child_run_count":2})).expect("cycle")];
        let runs = vec![
            run("manual-a", Some("Cell A"), None),
            run("manual-b", Some("Cell A"), None),
            run("manual-unnamed", None, None),
            run("child-a", None, Some(context("cycle-named"))),
            run("child-b", None, Some(context("cycle-named"))),
            run("child-unnamed", None, Some(context("cycle-unnamed"))),
        ];
        let labels = comparison_labels(&runs, &cycles);
        assert_eq!(labels["manual-a"], "Cell A · manual-a");
        assert_eq!(labels["manual-b"], "Cell A · manual-b");
        assert_eq!(labels["manual-unnamed"], "manual-unnamed");
        assert_eq!(labels["child-a"], "Formation · R2/S3 · child-a");
        assert_eq!(labels["child-b"], "Formation · R2/S3 · child-b");
        assert_eq!(labels["child-unnamed"], "cycle-unnamed · R2/S3");
        assert!(
            runs.iter()
                .filter(|run| run.cycle.is_some())
                .all(|run| run.name.is_none())
        );
    }

    #[test]
    fn comparison_labels_resolve_names_that_match_generated_suffixes() {
        let runs = vec![
            run("id-a", Some("Cell A"), None),
            run("id-b", Some("Cell A"), None),
            run("id-c", Some("Cell A · id-a"), None),
        ];
        let labels = comparison_labels(&runs, &[]);
        assert_eq!(labels.len(), 3);
        assert_eq!(
            labels
                .values()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3
        );
    }

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
        assert_eq!(energy_delta(10.0, 9.5), "-0.500 Wh (-5.00%)");
        assert_eq!(energy_delta(0.0, 1.0), "+1.000 Wh (percentage unavailable)");
        assert_eq!(format_energy(0.000047), "0.000047");
        assert_eq!(energy_delta(0.000047, 0.000094), "+0.000047 Wh (+100.00%)");
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
