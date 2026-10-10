//! Transient server history, deliberately separate from live telemetry and GUI persistence.

use super::DeviceSession;
use crate::backend::{BackendCommand, DownloadedFile, HistoryEvent, HistoryRequest};
use crate::core::{CycleHistory, CycleSummary, RenameRequest, RunHistory, RunSummary};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

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
    pub pending_exports: VecDeque<(HistoryRequest, DownloadedFile)>,
    pub pending: BTreeSet<HistoryRequest>,
    pub errors: BTreeMap<HistoryRequest, String>,
    pub connection_error: Option<String>,
}

impl HistoryState {
    pub(super) fn apply_result(
        &mut self,
        request: HistoryRequest,
        result: Result<HistoryEvent, String>,
    ) {
        // Ignore completions from a request cancelled by disconnect.
        if !self.pending.remove(&request) {
            return;
        }
        let event = match result {
            Ok(event) => {
                self.errors.remove(&request);
                event
            }
            Err(error) => {
                self.errors.insert(request, error);
                return;
            }
        };
        let matches_request = match (&request, &event) {
            (HistoryRequest::RefreshRuns, HistoryEvent::Runs(_))
            | (HistoryRequest::RefreshCycles, HistoryEvent::Cycles(_))
            | (
                HistoryRequest::ExportRunCsv(_) | HistoryRequest::ExportCycleCsv(_),
                HistoryEvent::FileExported(_),
            ) => true,
            (HistoryRequest::LoadRun(id), HistoryEvent::RunLoaded(history)) => {
                *id == history.summary.id
            }
            (HistoryRequest::LoadCycle(id), HistoryEvent::CycleLoaded(history)) => {
                *id == history.summary.execution_id
            }
            _ => false,
        };
        if !matches_request {
            self.errors.insert(
                request,
                "History response did not match the requested record.".to_owned(),
            );
            return;
        }
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
            HistoryEvent::FileExported(file) => self.pending_exports.push_back((request, file)),
        }
    }

    pub(super) fn disconnect(&mut self) {
        for request in std::mem::take(&mut self.pending) {
            self.errors.insert(
                request,
                "Server disconnected. Reconnect and retry.".to_owned(),
            );
        }
        self.connection_error =
            Some("Server disconnected. Reconnect to refresh history.".to_owned());
    }

    pub(crate) fn pending(&self, request: &HistoryRequest) -> bool {
        self.pending.contains(request)
    }
    pub(crate) fn error(&self, request: &HistoryRequest) -> Option<&str> {
        self.errors.get(request).map(String::as_str)
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
    pub(crate) fn clear_comparison(&mut self) {
        self.history.comparison_ids.clear();
        self.history.comparison_cache.clear();
    }

    pub(crate) fn use_comparison_baseline(&mut self, id: &str) {
        if let Some(index) = self
            .history
            .comparison_ids
            .iter()
            .position(|selected| selected == id)
        {
            self.history.comparison_ids.remove(index);
            self.history.comparison_ids.insert(0, id.to_owned());
        }
    }

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
            self.queue_history_request(HistoryRequest::LoadRun(id.to_owned()));
        }
        true
    }

    pub(crate) fn remove_comparison_run(&mut self, id: &str) {
        self.history
            .comparison_ids
            .retain(|selected| selected != id);
        self.history.comparison_cache.remove(id);
    }

    fn queue_history_request(&mut self, request: HistoryRequest) {
        if self.history.pending(&request) {
            return;
        }
        if !self.remote_command_available("history") {
            self.history.errors.insert(
                request,
                self.command_error
                    .clone()
                    .unwrap_or_else(|| "History is unavailable while disconnected.".to_owned()),
            );
            return;
        }
        self.history.errors.remove(&request);
        self.history.connection_error = None;
        self.history.pending.insert(request.clone());
        self.backend.command(BackendCommand::History(request));
    }

    pub(crate) fn history_request(&mut self, request: HistoryRequest) {
        if !self.is_remote() {
            return;
        }
        match &request {
            HistoryRequest::LoadRun(id) => {
                if self.history.selected_run.as_deref() != Some(id) {
                    self.history.selected_run = Some(id.clone());
                    self.history.loaded_run = None;
                }
            }
            HistoryRequest::LoadCycle(id) => {
                if self.history.selected_cycle.as_deref() != Some(id) {
                    self.history.selected_cycle = Some(id.clone());
                    self.history.loaded_cycle = None;
                    self.history.selected_run = None;
                    self.history.loaded_run = None;
                }
            }
            _ => {}
        }
        self.queue_history_request(request);
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
        for id in self.history.comparison_ids.clone() {
            self.queue_history_request(HistoryRequest::LoadRun(id));
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

    fn receive(history: &mut HistoryState, request: HistoryRequest, event: HistoryEvent) {
        history.pending.insert(request.clone());
        history.apply_result(request, Ok(event));
    }

    fn remote_session() -> DeviceSession {
        let mut session = DeviceSession::default();
        session.transport_mode = crate::session::TransportMode::Remote;
        session.remote_status = crate::backend::BackendConnectionStatus::Connected;
        session
    }

    #[test]
    fn correlated_requests_coalesce_fail_retry_and_keep_unrelated_pending() {
        let mut session = remote_session();
        let runs = HistoryRequest::RefreshRuns;
        let cycles = HistoryRequest::RefreshCycles;
        session.history_request(runs.clone());
        session.history_request(runs.clone());
        session.history_request(cycles.clone());
        assert_eq!(session.history.pending.len(), 2);
        session
            .history
            .apply_result(runs.clone(), Err("offline".to_owned()));
        assert!(!session.history.pending(&runs));
        assert!(session.history.pending(&cycles));
        assert_eq!(session.history.error(&runs), Some("offline"));
        session.history_request(runs.clone());
        assert!(session.history.pending(&runs));
        assert_eq!(session.history.error(&runs), None);
        session
            .history
            .apply_result(runs.clone(), Ok(HistoryEvent::Runs(vec![run("a")])));
        assert!(!session.history.pending(&runs));
        assert!(session.history.pending(&cycles));
        assert_eq!(session.history.runs[0].id, "a");
    }

    #[test]
    fn distinct_completed_exports_are_queued_without_overwriting() {
        let mut history = HistoryState::default();
        for request in [
            HistoryRequest::ExportRunCsv("a".to_owned()),
            HistoryRequest::ExportCycleCsv("b".to_owned()),
        ] {
            let filename = format!("{request:?}.csv");
            receive(
                &mut history,
                request,
                HistoryEvent::FileExported(DownloadedFile {
                    filename,
                    content_type: "text/csv".to_owned(),
                    bytes: Vec::new(),
                }),
            );
        }
        assert_eq!(history.pending_exports.len(), 2);
        assert_ne!(
            history.pending_exports[0].1.filename,
            history.pending_exports[1].1.filename
        );
    }

    #[test]
    fn refresh_keeps_loaded_detail_and_comparison_cache_through_failure_and_success() {
        let mut session = remote_session();
        let old = RunHistory {
            summary: run("a"),
            samples: Vec::new(),
        };
        session.history.selected_run = Some("a".to_owned());
        session.history.loaded_run = Some(old.clone());
        session.history.comparison_ids.push("a".to_owned());
        session
            .history
            .comparison_cache
            .insert("a".to_owned(), old.clone());
        session.refresh_history();
        let request = HistoryRequest::LoadRun("a".to_owned());
        assert_eq!(session.history.loaded_run, Some(old.clone()));
        assert_eq!(session.history.comparison_cache["a"], old);
        assert_eq!(session.history.pending.len(), 3); // lists plus one coalesced run load
        session
            .history
            .apply_result(request.clone(), Err("offline".to_owned()));
        assert_eq!(
            session
                .history
                .loaded_run
                .as_ref()
                .map(|h| h.summary.id.as_str()),
            Some("a")
        );
        assert!(session.history.comparison_cache.contains_key("a"));
        assert_eq!(session.history.error(&request), Some("offline"));
        session.history_request(request.clone());
        let mut replacement = run("a");
        replacement.capacity_mah = Some(42);
        session.history.apply_result(
            request,
            Ok(HistoryEvent::RunLoaded(RunHistory {
                summary: replacement,
                samples: Vec::new(),
            })),
        );
        assert_eq!(
            session
                .history
                .loaded_run
                .as_ref()
                .and_then(|h| h.summary.capacity_mah),
            Some(42)
        );
        assert_eq!(
            session.history.comparison_cache["a"].summary.capacity_mah,
            Some(42)
        );
    }

    #[test]
    fn selection_change_and_disconnect_preserve_old_cache_but_not_wrong_detail() {
        let mut session = remote_session();
        session.history.selected_run = Some("a".to_owned());
        session.history.loaded_run = Some(RunHistory {
            summary: run("a"),
            samples: Vec::new(),
        });
        session.history_request(HistoryRequest::LoadRun("b".to_owned()));
        assert!(session.history.loaded_run.is_none());
        let request = HistoryRequest::LoadRun("b".to_owned());
        assert!(session.history.pending(&request));
        session.history.apply_result(
            request.clone(),
            Ok(HistoryEvent::RunLoaded(RunHistory {
                summary: run("b"),
                samples: Vec::new(),
            })),
        );
        assert_eq!(
            session
                .history
                .loaded_run
                .as_ref()
                .map(|h| h.summary.id.as_str()),
            Some("b")
        );
        session.history.comparison_ids.push("b".to_owned());
        session.history.comparison_cache.insert(
            "b".to_owned(),
            session.history.loaded_run.clone().expect("loaded"),
        );
        session.history_request(HistoryRequest::LoadRun("b".to_owned()));
        session.history.disconnect();
        assert!(session.history.pending.is_empty());
        assert!(session.history.comparison_cache.contains_key("b"));
        assert_eq!(session.history.comparison_ids, ["b"]);
        assert!(session.history.error(&request).is_some());
        session.history.apply_result(
            request,
            Ok(HistoryEvent::RunLoaded(RunHistory {
                summary: run("stale"),
                samples: Vec::new(),
            })),
        );
        assert_eq!(
            session
                .history
                .loaded_run
                .as_ref()
                .map(|h| h.summary.id.as_str()),
            Some("b")
        );
    }

    #[test]
    fn cycle_refresh_keeps_detail_and_new_selection_clears_it() {
        let mut session = remote_session();
        let summary: CycleSummary = serde_json::from_value(
            serde_json::json!({"execution_id":"cycle-a","sample_count":0,"child_run_count":0}),
        )
        .expect("cycle");
        session.history.selected_cycle = Some("cycle-a".to_owned());
        session.history.loaded_cycle = Some(CycleHistory {
            summary,
            samples: Vec::new(),
            child_runs: Vec::new(),
        });
        session.history_request(HistoryRequest::LoadCycle("cycle-a".to_owned()));
        assert!(session.history.loaded_cycle.is_some());
        session.history_request(HistoryRequest::LoadCycle("cycle-b".to_owned()));
        assert!(session.history.loaded_cycle.is_none());
        assert!(
            session
                .history
                .pending(&HistoryRequest::LoadCycle("cycle-b".to_owned()))
        );
    }

    #[test]
    fn baseline_reorder_and_clear_do_not_request_telemetry() {
        let mut session = remote_session();
        session.history.comparison_ids = vec!["a".into(), "b".into(), "c".into()];
        session.use_comparison_baseline("b");
        assert_eq!(session.history.comparison_ids, ["b", "a", "c"]);
        assert!(session.history.pending.is_empty());
        session.remove_comparison_run("b");
        assert_eq!(session.history.comparison_ids[0], "a");
        session.clear_comparison();
        assert!(session.history.comparison_ids.is_empty());
        assert!(session.history.comparison_cache.is_empty());
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
        receive(
            &mut session.history,
            HistoryRequest::RefreshRuns,
            HistoryEvent::Runs(vec![run("old"), child.clone()]),
        );
        receive(
            &mut session.history,
            HistoryRequest::RefreshCycles,
            HistoryEvent::Cycles(vec![summary.clone()]),
        );
        receive(
            &mut session.history,
            HistoryRequest::LoadRun("old".to_owned()),
            HistoryEvent::RunLoaded(RunHistory {
                summary: run("old"),
                samples: vec![sample.clone()],
            }),
        );
        receive(
            &mut session.history,
            HistoryRequest::LoadCycle("old-cycle".to_owned()),
            HistoryEvent::CycleLoaded(CycleHistory {
                summary,
                samples: vec![cycle_sample.clone()],
                child_runs: vec![child],
            }),
        );
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
        receive(
            &mut session.history,
            HistoryRequest::LoadRun("stale".to_owned()),
            HistoryEvent::RunLoaded(RunHistory {
                summary: run("stale"),
                samples: Vec::new(),
            }),
        );
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
        // History-only refreshes, failures, exports, and baseline edits must not
        // replace the independently authoritative live snapshot.
        session.history.comparison_ids.push("child".to_owned());
        session.use_comparison_baseline("child");
        session.history.pending.insert(HistoryRequest::RefreshRuns);
        session.history.apply_result(
            HistoryRequest::RefreshRuns,
            Err("temporary failure".to_owned()),
        );
        session
            .history
            .pending
            .insert(HistoryRequest::ExportRunCsv("old".to_owned()));
        session.history.apply_result(
            HistoryRequest::ExportRunCsv("old".to_owned()),
            Err("temporary export failure".to_owned()),
        );
        session.remove_comparison_run("old");
        session.clear_comparison();
        assert_eq!(session.samples, snapshot.history);
        assert_eq!(session.cycle_samples, snapshot.cycle_history);
        assert_eq!(session.current_run, snapshot.current_run);
        assert_eq!(session.cycle, snapshot.cycle);
        assert_eq!(session.current_test_config, snapshot.test.config);
        assert_eq!(session.capabilities, snapshot.capabilities);
    }

    #[test]
    fn direct_history_requests_do_not_queue_or_clear_live_samples() {
        let mut session = DeviceSession::default();
        session.refresh_history();
        session.history_request(HistoryRequest::LoadRun("old".to_owned()));
        assert!(session.history.pending.is_empty());
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
            receive(
                &mut session.history,
                HistoryRequest::LoadRun(id.to_owned()),
                HistoryEvent::RunLoaded(RunHistory {
                    summary: run(id),
                    samples: Vec::new(),
                }),
            );
        }
        assert_eq!(session.history.comparison_cache.len(), 4);
        assert_eq!(session.history.comparison_ids, ["a", "b", "c", "d"]);
        session.remove_comparison_run("a");
        assert_eq!(session.history.comparison_ids[0], "b");
        assert!(!session.history.comparison_cache.contains_key("a"));
        assert!(session.add_comparison_run("e"));
    }
}
