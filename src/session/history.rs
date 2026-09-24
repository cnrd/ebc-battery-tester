//! Transient server history, deliberately separate from live telemetry and GUI persistence.

use super::DeviceSession;
use crate::backend::{BackendCommand, DownloadedFile, HistoryEvent, HistoryRequest};
use crate::core::{CycleHistory, CycleSummary, RenameRequest, RunHistory, RunSummary};
use std::collections::BTreeMap;

#[derive(Default)]
pub(crate) struct HistoryState {
    pub runs: Vec<RunSummary>,
    pub cycles: Vec<CycleSummary>,
    pub loaded_run: Option<RunHistory>,
    /// Bounded histories for the transient comparison; keyed by immutable run ID.
    pub comparison_cache: BTreeMap<String, RunHistory>,
    pub comparison_ids: Vec<String>,
    pub loaded_cycle: Option<CycleHistory>,
    pub selected_run: Option<String>,
    pub selected_cycle: Option<String>,
    pub pending_export: Option<DownloadedFile>,
    pub error: Option<String>,
    pub pending_requests: usize,
}

impl HistoryState {
    pub(super) fn apply(&mut self, event: HistoryEvent) {
        self.pending_requests = self.pending_requests.saturating_sub(1);
        match event {
            HistoryEvent::Runs(runs) => self.runs = runs,
            HistoryEvent::Cycles(cycles) => self.cycles = cycles,
            HistoryEvent::RunLoaded(history) => {
                if self.selected_run.as_deref() == Some(history.summary.id.as_str()) {
                    self.loaded_run = Some(history.clone());
                }
                if self.comparison_ids.contains(&history.summary.id) {
                    self.comparison_cache
                        .insert(history.summary.id.clone(), history);
                }
            }
            HistoryEvent::CycleLoaded(history) => {
                if self.selected_cycle.as_deref() == Some(history.summary.execution_id.as_str()) {
                    self.loaded_cycle = Some(history);
                }
            }
            HistoryEvent::FileExported(file) => self.pending_export = Some(file),
        }
    }

    pub(super) fn renamed(&mut self, id: &str, cycle: bool, name: Option<String>) {
        if cycle {
            for summary in &mut self.cycles {
                if summary.execution_id == id {
                    summary.name.clone_from(&name);
                }
            }
            if let Some(history) = &mut self.loaded_cycle
                && history.summary.execution_id == id
            {
                history.summary.name = name.clone();
            }
        } else {
            for summary in &mut self.runs {
                if summary.id == id {
                    summary.name.clone_from(&name);
                }
            }
            if let Some(history) = &mut self.loaded_run
                && history.summary.id == id
            {
                history.summary.name = name.clone();
            }
            if let Some(history) = self.comparison_cache.get_mut(id) {
                history.summary.name = name;
            }
        }
    }
}

impl DeviceSession {
    pub(crate) fn add_comparison_run(&mut self, id: &str) -> bool {
        if self
            .history
            .comparison_ids
            .iter()
            .any(|selected| selected == id)
        {
            return false;
        }
        if self.history.comparison_ids.len() == 4 {
            return false;
        }
        self.history.comparison_ids.push(id.to_owned());
        if let Some(history) = self
            .history
            .loaded_run
            .as_ref()
            .filter(|h| h.summary.id == id)
        {
            self.history
                .comparison_cache
                .insert(id.to_owned(), history.clone());
        } else {
            self.comparison_request_run(id.to_owned());
        }
        true
    }

    pub(crate) fn remove_comparison_run(&mut self, id: &str) {
        self.history
            .comparison_ids
            .retain(|selected| selected != id);
        self.history.comparison_cache.remove(id);
    }

    fn comparison_request_run(&mut self, id: String) {
        if self.remote_command_available("history") {
            self.history.pending_requests += 1;
            self.backend
                .command(BackendCommand::History(HistoryRequest::LoadRun(id)));
        }
    }

    pub(crate) fn history_request(&mut self, request: HistoryRequest) {
        if !self.is_remote() {
            return;
        }
        if !self.remote_command_available("history") {
            self.history.error.clone_from(&self.command_error);
            return;
        }
        match &request {
            HistoryRequest::LoadRun(id) => {
                self.history.selected_run = Some(id.clone());
                self.history.loaded_run = None;
            }
            HistoryRequest::LoadCycle(id) => {
                self.history.selected_cycle = Some(id.clone());
                self.history.loaded_cycle = None;
                self.history.selected_run = None;
                self.history.loaded_run = None;
            }
            _ => {}
        }
        self.history.error = None;
        self.history.pending_requests += 1;
        self.backend.command(BackendCommand::History(request));
    }

    pub(crate) fn refresh_history(&mut self) {
        self.history_request(HistoryRequest::RefreshRuns);
        self.history_request(HistoryRequest::RefreshCycles);
        let selected_run = self.history.selected_run.clone();
        if let Some(id) = self.history.selected_cycle.clone() {
            self.history_request(HistoryRequest::LoadCycle(id));
        }
        if let Some(id) = selected_run {
            self.history_request(HistoryRequest::LoadRun(id));
        }
        self.history.comparison_cache.clear();
        for id in self.history.comparison_ids.clone() {
            self.comparison_request_run(id);
        }
    }

    pub(crate) fn rename_run(&mut self, run_id: String, request: RenameRequest) {
        if self.remote_command_available("run rename") {
            self.backend
                .command(BackendCommand::RenameRun { run_id, request });
        }
    }

    pub(crate) fn rename_cycle(&mut self, execution_id: String, request: RenameRequest) {
        if self.remote_command_available("cycle rename") {
            self.backend.command(BackendCommand::RenameCycle {
                execution_id,
                request,
            });
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "history isolation fixtures fail fast")]
mod tests {
    use super::*;
    use crate::core::{
        AuthoritativeSnapshot, Capabilities, CurrentRunMetadata, CycleRunContext, CycleState,
        CycleStatus, Sample, TestConfiguration, TestStatus,
    };

    fn run(id: &str) -> RunSummary {
        serde_json::from_value(serde_json::json!({
            "id": id, "archived_at_utc": "2026-01-01T00:00:00Z", "state": "completed",
            "elapsed_seconds": 10, "sample_count": 1
        }))
        .expect("run fixture")
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "assert all live state survives historical events"
    )]
    fn loaded_history_and_renames_leave_all_live_telemetry_and_identity_unchanged() {
        let mut session = DeviceSession::default();
        let sample: Sample = serde_json::from_value(serde_json::json!({
            "run_id": "live", "sequence": 1, "timestamp_utc": "now", "elapsed_seconds": 1,
            "voltage_mv": 4000, "current_ma": 1000, "capacity_mah": 10, "mode": "DischargeConstantCurrent"
        })).expect("live sample");
        let cycle_sample = crate::core::CycleSample {
            execution_id: "live-cycle".to_owned(),
            sequence: 0,
            timestamp_utc: "now".to_owned(),
            elapsed_milliseconds: 1000,
            repeat_index: 0,
            step_index: 0,
            cycle_state: CycleState::RunningStep,
            test_state: crate::core::TestState::Running,
            mode: sample.mode,
            activity_known: true,
            active: true,
            voltage_mv: 4000,
            current_ma: 1000,
            device_capacity_mah: 10,
            test_capacity_mah: Some(10),
            test_energy_wh: 0.1,
        };
        let snapshot = AuthoritativeSnapshot {
            test: TestStatus {
                config: Some(TestConfiguration::DischargeConstantCurrent {
                    current_ma: 1000,
                    cutoff_voltage_mv: 3000,
                    cutoff_time_min: 10,
                }),
                ..TestStatus::default()
            },
            capabilities: Capabilities {
                start: true,
                ..Capabilities::default()
            },
            current_run: CurrentRunMetadata {
                id: Some("live".to_owned()),
                name: Some("Live".to_owned()),
                cycle: None,
            },
            cycle: CycleStatus {
                execution_id: Some("live-cycle".to_owned()),
                state: CycleState::RunningStep,
                ..CycleStatus::default()
            },
            history: vec![sample.clone()],
            cycle_history: vec![cycle_sample.clone()],
            ..AuthoritativeSnapshot::default()
        };
        session.apply_snapshot(snapshot.clone());
        session.history.comparison_ids.push("old".to_owned());
        session.history.selected_run = Some("old".to_owned());
        session.history.selected_cycle = Some("old-cycle".to_owned());
        let mut child = run("child");
        child.cycle = Some(CycleRunContext {
            execution_id: "old-cycle".to_owned(),
            repeat_index: 1,
            step_index: 2,
        });
        let summary: CycleSummary = serde_json::from_value(serde_json::json!({ "execution_id": "old-cycle", "sample_count": 1, "child_run_count": 1 })).expect("cycle summary");
        session
            .history
            .apply(HistoryEvent::Runs(vec![run("old"), child.clone()]));
        session
            .history
            .apply(HistoryEvent::Cycles(vec![summary.clone()]));
        session.history.apply(HistoryEvent::RunLoaded(RunHistory {
            summary: run("old"),
            samples: vec![sample.clone()],
        }));
        session
            .history
            .apply(HistoryEvent::CycleLoaded(CycleHistory {
                summary,
                samples: vec![cycle_sample.clone()],
                child_runs: vec![child],
            }));
        session
            .history
            .renamed("old", false, Some("Renamed".to_owned()));
        session
            .history
            .renamed("old-cycle", true, Some("Renamed cycle".to_owned()));
        assert_eq!(
            session
                .history
                .loaded_run
                .as_ref()
                .expect("loaded run")
                .summary
                .name
                .as_deref(),
            Some("Renamed")
        );
        assert_eq!(
            session
                .history
                .loaded_cycle
                .as_ref()
                .expect("loaded cycle")
                .summary
                .name
                .as_deref(),
            Some("Renamed cycle")
        );
        // An older response arriving after selection changed cannot replace the selection.
        session.history.apply(HistoryEvent::RunLoaded(RunHistory {
            summary: run("stale"),
            samples: Vec::new(),
        }));
        assert_eq!(
            session
                .history
                .loaded_run
                .as_ref()
                .expect("selected history")
                .summary
                .id,
            "old"
        );
        assert_eq!(session.samples, snapshot.history);
        assert_eq!(session.cycle_samples, snapshot.cycle_history);
        assert_eq!(session.current_run, snapshot.current_run);
        assert_eq!(session.cycle, snapshot.cycle);
        assert_eq!(session.current_test_config, snapshot.test.config);
        assert_eq!(session.capabilities, snapshot.capabilities);
        assert!(session.history.comparison_cache.contains_key("old"));
        session.remove_comparison_run("old");
        assert_eq!(session.samples, snapshot.history);
        assert_eq!(session.current_test_config, snapshot.test.config);
    }

    #[test]
    fn direct_history_requests_do_not_queue_or_clear_live_samples() {
        let mut session = DeviceSession::default();
        session.refresh_history();
        session.history_request(HistoryRequest::LoadRun("old".to_owned()));
        assert_eq!(session.history.pending_requests, 0);
        assert!(session.history.selected_run.is_none());
        assert!(session.command_error.is_none());
    }

    #[test]
    fn comparison_selection_is_ordered_bounded_and_cache_accepts_out_of_order_responses() {
        let mut session = DeviceSession::default();
        for id in ["a", "b", "c", "d"] {
            assert!(session.add_comparison_run(id));
        }
        assert!(!session.add_comparison_run("b"));
        assert!(!session.add_comparison_run("e"));
        for id in ["a", "c", "b", "d"] {
            session.history.apply(HistoryEvent::RunLoaded(RunHistory {
                summary: run(id),
                samples: Vec::new(),
            }));
        }
        assert_eq!(session.history.comparison_cache.len(), 4);
        assert_eq!(session.history.comparison_ids, ["a", "b", "c", "d"]);
        session.remove_comparison_run("a");
        assert_eq!(session.history.comparison_ids[0], "b");
        assert!(!session.history.comparison_cache.contains_key("a"));
        assert!(session.add_comparison_run("e"));
    }
}
