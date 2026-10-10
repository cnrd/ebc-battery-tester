//! Persistent history reads and centralized cycle summary lifecycle.

use super::{
    ActorRequest, ActorResponse, ApiError, AppState, AxumPath, CycleExecutionMetadata,
    CycleHistory, CycleState, CycleStatus, CycleSummary, DeviceActor, Instant, Json, Persistence,
    RunHistory, RunSummary, SNAPSHOT_SAMPLE_LIMIT, State, cycle_presentation_history, fs,
    presentation_history, request, valid_run_id,
};
use crate::core::{AuthoritativeSnapshot, CycleSample};
use crate::cycle::CycleEngine;

impl Persistence {
    pub(super) fn run_history(&self, id: &str) -> Result<Option<RunHistory>, String> {
        let Some(summary) = self.runs.iter().find(|run| run.id == id) else {
            return Ok(None);
        };
        let path = self.runs_dir.join(format!("{id}.csv"));
        if !path.is_file() {
            return Err(format!("run telemetry missing for {id}"));
        }
        let samples = Self::load_run_samples(&path)?;
        // Empty IDs belong to the legacy CSV schema. Never normalize/rewrite on a read.
        if samples
            .iter()
            .any(|sample| !sample.run_id.is_empty() && sample.run_id != id)
        {
            return Err(format!("run telemetry id mismatch for {id}"));
        }
        Ok(Some(RunHistory {
            summary: summary.clone(),
            samples: presentation_history(&samples, SNAPSHOT_SAMPLE_LIMIT),
        }))
    }

    fn cycle_ids(&self) -> Result<Vec<String>, String> {
        let mut ids = std::collections::BTreeSet::new();
        for entry in fs::read_dir(&self.cycles_dir).map_err(|error| error.to_string())? {
            let path = entry.map_err(|error| error.to_string())?.path();
            if !matches!(
                path.extension().and_then(|value| value.to_str()),
                Some("csv" | "json")
            ) {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|value| value.to_str())
                .filter(|id| valid_run_id(id))
                .ok_or_else(|| format!("invalid cycle history filename {}", path.display()))?;
            if !self.cycle_path(id).is_file() {
                return Err(format!("cycle telemetry missing for {id}"));
            }
            ids.insert(id.to_owned());
        }
        Ok(ids.into_iter().collect())
    }

    fn child_runs(&self, id: &str) -> Vec<RunSummary> {
        let mut runs: Vec<_> = self
            .runs
            .iter()
            .filter(|run| {
                run.cycle
                    .as_ref()
                    .is_some_and(|cycle| cycle.execution_id == id)
            })
            .cloned()
            .collect();
        runs.sort_by(|left, right| {
            let key = |run: &RunSummary| {
                run.cycle
                    .as_ref()
                    .map(|cycle| (cycle.repeat_index, cycle.step_index))
            };
            key(left)
                .cmp(&key(right))
                .then_with(|| left.id.cmp(&right.id))
        });
        runs
    }

    fn cycle_summary(&self, id: &str, samples: &[CycleSample]) -> Result<CycleSummary, String> {
        let metadata = self.load_cycle_metadata(id)?.unwrap_or_default();
        Ok(CycleSummary {
            execution_id: id.to_owned(),
            name: metadata.name,
            recipe: metadata.recipe,
            saved_recipe: metadata.saved_recipe,
            started_at_utc: metadata.started_at_utc,
            state: metadata.state,
            result: metadata.result,
            elapsed_milliseconds: metadata
                .elapsed_milliseconds
                .or_else(|| samples.last().map(|sample| sample.elapsed_milliseconds)),
            sample_count: samples.len(),
            child_run_count: self.child_runs(id).len(),
        })
    }

    /// Recover sidecars too, including an execution whose start outlived session.json.
    /// Telemetry boundary states cannot prove terminal completion of legacy data.
    pub(super) fn recover_cycle_history(
        &self,
        snapshot: &mut AuthoritativeSnapshot,
    ) -> Result<(), String> {
        for id in self.cycle_ids()? {
            let samples = self.load_cycle_samples(&id)?;
            let metadata = self.load_cycle_metadata(&id)?;
            let current = snapshot.cycle.execution_id.as_deref() == Some(id.as_str());
            let status = CycleStatus {
                state: metadata
                    .as_ref()
                    .and_then(|value| value.state)
                    .unwrap_or(if current {
                        snapshot.cycle.state
                    } else {
                        CycleState::Idle
                    }),
                ..CycleStatus::default()
            };
            let recovered = CycleEngine::from_persisted_status(status.clone());
            if recovered.status().state != status.state {
                let mut metadata = metadata.unwrap_or_else(|| CycleExecutionMetadata {
                    execution_id: id.clone(),
                    name: snapshot.cycle.name.clone(),
                    recipe: snapshot.cycle.recipe.clone(),
                    saved_recipe: snapshot.cycle.saved_recipe.clone(),
                    started_at_utc: snapshot.cycle.started_at_utc.clone(),
                    ..CycleExecutionMetadata::default()
                });
                metadata.state = Some(recovered.status().state);
                metadata.result = recovered.status().result.clone();
                metadata.elapsed_milliseconds = metadata
                    .elapsed_milliseconds
                    .max(samples.last().map(|sample| sample.elapsed_milliseconds));
                metadata.sample_count = Some(samples.len());
                self.write_cycle_metadata(&metadata)?;
            }
            if current
                && let Some(metadata) = self.load_cycle_metadata(&id)?
                && let Some(state) = metadata.state
            {
                snapshot.cycle.state = state;
                snapshot.cycle.result = metadata.result;
                snapshot.cycle.rest_remaining_seconds = None;
            }
        }
        self.save_metadata(snapshot)
    }
}

impl DeviceActor {
    /// All transition paths synchronize here, including gaps and write failures.
    /// A persisted terminal duration is written once and never derived from a later clock.
    pub(super) fn persist_cycle_transition(&mut self) -> Result<(), String> {
        let status = self.cycle.status();
        let Some(id) = status.execution_id.as_deref() else {
            return Ok(());
        };
        let Some(mut metadata) = self.persistence.load_cycle_metadata(id)? else {
            return Ok(());
        };
        if metadata.state == Some(status.state)
            && metadata.result == status.result
            && self.snapshot.cycle.repeat_index == status.repeat_index
            && self.snapshot.cycle.step_index == status.step_index
        {
            return Ok(());
        }
        self.persistence.flush_cycle_samples()?;
        metadata.state = Some(status.state);
        metadata.result = status.result.clone();
        let last_elapsed = self
            .snapshot
            .cycle_history
            .last()
            .filter(|sample| sample.execution_id == id)
            .map_or(0, |sample| sample.elapsed_milliseconds);
        metadata.elapsed_milliseconds = Some(
            u64::try_from(self.cycle.elapsed(Instant::now()).as_millis())
                .unwrap_or(u64::MAX)
                .max(last_elapsed)
                .max(metadata.elapsed_milliseconds.unwrap_or(0)),
        );
        metadata.sample_count = Some(self.persistence.raw_cycle_sample_count);
        self.persistence.write_cycle_metadata(&metadata)
    }

    fn live_cycle_summary(&self, summary: &mut CycleSummary) {
        if self.cycle.is_executing()
            && self.cycle.status().execution_id.as_deref() == Some(summary.execution_id.as_str())
        {
            let status = self.cycle.status();
            summary.name = status.name.clone();
            summary.recipe = status.recipe.clone();
            summary.saved_recipe = status.saved_recipe.clone();
            summary.started_at_utc = status.started_at_utc.clone();
            summary.state = Some(status.state);
            summary.result = status.result.clone();
            summary.elapsed_milliseconds = Some(
                u64::try_from(self.cycle.elapsed(Instant::now()).as_millis()).unwrap_or(u64::MAX),
            );
        }
    }

    pub(super) fn cycle_summaries(&mut self) -> Result<Vec<CycleSummary>, String> {
        self.persistence.flush_cycle_samples()?;
        let mut summaries = Vec::new();
        for id in self.persistence.cycle_ids()? {
            let samples = self.persistence.load_cycle_samples(&id)?;
            let mut summary = self.persistence.cycle_summary(&id, &samples)?;
            self.live_cycle_summary(&mut summary);
            summaries.push(summary);
        }
        summaries.sort_by(|left, right| {
            right
                .started_at_utc
                .cmp(&left.started_at_utc)
                .then_with(|| right.execution_id.cmp(&left.execution_id))
        });
        Ok(summaries)
    }

    pub(super) fn cycle_history(&mut self, id: &str) -> Result<Option<CycleHistory>, String> {
        if !self.persistence.cycle_path(id).exists()
            && !self.persistence.cycle_metadata_path(id).exists()
        {
            return Ok(None);
        }
        if !self.persistence.cycle_path(id).is_file() {
            return Err(format!("cycle telemetry missing for {id}"));
        }
        self.persistence.flush_cycle_samples()?;
        let samples = self.persistence.load_cycle_samples(id)?;
        let mut summary = self.persistence.cycle_summary(id, &samples)?;
        self.live_cycle_summary(&mut summary);
        Ok(Some(CycleHistory {
            summary,
            samples: cycle_presentation_history(&samples, SNAPSHOT_SAMPLE_LIMIT),
            child_runs: self.persistence.child_runs(id),
        }))
    }
}

pub(super) async fn get_cycles(
    State(state): State<AppState>,
) -> Result<Json<Vec<CycleSummary>>, ApiError> {
    match request(&state, ActorRequest::Cycles).await? {
        ActorResponse::Cycles(Ok(summaries)) => Ok(Json(summaries)),
        ActorResponse::Cycles(Err(error)) => Err(ApiError::internal(error)),
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}

pub(super) async fn get_run_history(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<RunHistory>, ApiError> {
    if !valid_run_id(&id) {
        return Err(ApiError::bad_request("invalid run id"));
    }
    match request(&state, ActorRequest::RunHistory(id)).await? {
        ActorResponse::RunHistory(Ok(Some(history))) => Ok(Json(history)),
        ActorResponse::RunHistory(Ok(None)) => Err(ApiError::not_found("run not found")),
        ActorResponse::RunHistory(Err(error)) => Err(ApiError::internal(error)),
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}

pub(super) async fn get_cycle_history(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<CycleHistory>, ApiError> {
    if !valid_run_id(&id) {
        return Err(ApiError::bad_request("invalid cycle execution id"));
    }
    match request(&state, ActorRequest::CycleHistory(id)).await? {
        ActorResponse::CycleHistory(Ok(Some(history))) => Ok(Json(history)),
        ActorResponse::CycleHistory(Ok(None)) => Err(ApiError::not_found("cycle not found")),
        ActorResponse::CycleHistory(Err(error)) => Err(ApiError::internal(error)),
        _ => Err(ApiError::internal("unexpected actor response")),
    }
}
