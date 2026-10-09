//! GUI-independent recipe orchestration.

use web_time::{Duration, Instant};

use crate::controller::{PreparedCommand, REPORT_TIMEOUT_REASON, TestController};
use crate::core::{
    ApiCommand, Capabilities, CycleRecipe, CycleState, CycleStatus, CycleStep, DeviceState,
    SavedRecipeReference, TestConfiguration, TestState, TestStatus, ValidationError,
};

const RESTART_REASON: &str = "cycle interrupted by process restart";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CycleAction {
    Start(TestConfiguration),
    Stop,
}

#[derive(Clone, Debug)]
struct RestClock {
    started: Instant,
    duration: Duration,
}

#[derive(Clone, Debug)]
pub struct CycleEngine {
    status: CycleStatus,
    cycle_started: Option<Instant>,
    cycle_ended: Option<Instant>,
    pending_action: Option<CycleAction>,
    rest_clock: Option<RestClock>,
    start_committed: bool,
    receive_prefix: bool,
    settled_advance: Option<Instant>,
    epoch: u64,
}

impl Default for CycleEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl CycleEngine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            status: CycleStatus::default(),
            cycle_started: None,
            cycle_ended: None,
            pending_action: None,
            rest_clock: None,
            start_committed: false,
            receive_prefix: false,
            settled_advance: None,
            epoch: 0,
        }
    }

    /// Restores persisted state. Work which may have owned the device is never
    /// resumed automatically after a process restart.
    #[must_use]
    pub fn from_status(mut status: CycleStatus) -> Self {
        if is_nonterminal(status.state) {
            status.state = CycleState::Interrupted;
            status.result = Some(RESTART_REASON.to_owned());
            status.rest_remaining_seconds = None;
        }
        Self {
            status,
            cycle_started: None,
            cycle_ended: None,
            pending_action: None,
            rest_clock: None,
            start_committed: false,
            receive_prefix: false,
            settled_advance: None,
            epoch: 0,
        }
    }

    #[must_use]
    pub fn from_persisted_status(status: CycleStatus) -> Self {
        Self::from_status(status)
    }

    #[must_use]
    pub fn status(&self) -> &CycleStatus {
        &self.status
    }

    /// Replaces status using the same conservative recovery behavior as
    /// [`Self::from_status`].
    pub fn replace_status(&mut self, status: CycleStatus) {
        let epoch = self
            .epoch
            .checked_add(1)
            .unwrap_or_else(|| panic!("cycle scheduling identities exhausted"));
        *self = Self::from_status(status);
        self.epoch = epoch;
    }

    #[must_use]
    pub fn pending_action(&self) -> Option<&CycleAction> {
        self.pending_action.as_ref()
    }

    #[must_use]
    pub fn is_executing(&self) -> bool {
        is_nonterminal(self.status.state)
    }

    #[must_use]
    pub fn owns_orchestration(&self) -> bool {
        self.is_executing()
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn validate_prepared(
        &self,
        controller: &TestController,
        prepared: PreparedCommand,
        now: Instant,
    ) -> Result<(), String> {
        if matches!(prepared.frame(), Some(crate::device::OutboundFrame::Stop)) {
            return controller.validate_prepared_at(prepared, now);
        }
        if let Some(epoch) = prepared.cycle_epoch() {
            if epoch != self.epoch
                || self
                    .pending_action
                    .as_ref()
                    .is_none_or(|action| match action {
                        CycleAction::Start(config) => {
                            prepared.command() != ApiCommand::Start(*config)
                                || !self.owns_orchestration()
                                || controller.device().current_ma != Some(0)
                        }
                        CycleAction::Stop => prepared.command() != ApiCommand::Stop,
                    })
            {
                return Err("command belongs to a retired cycle action".to_owned());
            }
        } else {
            self.authorize_manual(prepared.command())?;
        }
        controller.validate_prepared_at(prepared, now)
    }

    /// One backend-independent reservation rule. Capabilities are a projection,
    /// never a bypass credential for a physical command.
    pub(crate) fn authorize_manual(&self, command: ApiCommand) -> Result<(), String> {
        if self.owns_orchestration()
            && matches!(
                command,
                ApiCommand::Start(_)
                    | ApiCommand::Resume
                    | ApiCommand::Adjust(_)
                    | ApiCommand::Calibration(_)
            )
        {
            return Err("the active cycle owns test orchestration".to_owned());
        }
        Ok(())
    }

    pub(crate) fn capabilities(&self, controller: &TestController, now: Instant) -> Capabilities {
        let mut capabilities = controller.capabilities_at(now);
        if self.owns_orchestration() {
            capabilities.start = false;
            capabilities.resume = false;
            capabilities.stop = true;
            capabilities.show_stop = true;
            capabilities.adjust = false;
            capabilities.calibrate_voltage = false;
            capabilities.calibrate_current = false;
            capabilities.confirm_calibration = false;
        }
        capabilities
    }

    pub(crate) fn begin_receive_prefix(&mut self) {
        self.receive_prefix = true;
    }

    pub(crate) fn finish_receive_prefix(&mut self) -> Option<CycleAction> {
        self.receive_prefix = false;
        if let Some(now) = self.settled_advance.take()
            && self.status.state == CycleState::Settling
        {
            return self.advance(now);
        }
        None
    }

    #[must_use]
    pub fn elapsed(&self, now: Instant) -> Duration {
        self.cycle_started.map_or(Duration::ZERO, |started| {
            self.cycle_ended
                .unwrap_or(now)
                .saturating_duration_since(started)
        })
    }

    /// Starts a validated recipe. A first device step produces a semantic
    /// action; a first rest step starts its monotonic timer without an action.
    ///
    /// # Errors
    /// Returns an error if the recipe is invalid or another cycle is active.
    pub fn start(
        &mut self,
        recipe: CycleRecipe,
        execution_id: String,
        name: Option<String>,
        saved_recipe: Option<SavedRecipeReference>,
        started_at_utc: Option<String>,
        now: Instant,
    ) -> Result<Option<CycleAction>, ValidationError> {
        recipe.validate()?;
        if self.is_executing() {
            return Err(ValidationError {
                field: "state".to_owned(),
                message: "a cycle is already active".to_owned(),
            });
        }

        self.status = CycleStatus {
            state: CycleState::Preparing,
            recipe: Some(recipe),
            execution_id: Some(execution_id),
            name,
            saved_recipe,
            repeat_index: 0,
            step_index: 0,
            started_at_utc,
            result: None,
            rest_remaining_seconds: None,
        };
        self.epoch = self
            .epoch
            .checked_add(1)
            .unwrap_or_else(|| panic!("cycle scheduling identities exhausted"));
        self.settled_advance = None;
        self.cycle_started = Some(now);
        self.cycle_ended = None;
        self.pending_action = None;
        self.rest_clock = None;
        self.start_committed = false;
        Ok(self.begin_current_step(now))
    }

    /// Applies a physical report. `fresh_report` must only be true for a newly
    /// received device report, not for cached state accompanying completion.
    pub fn on_physical_state(
        &mut self,
        now: Instant,
        device: &DeviceState,
        test: &TestStatus,
        fresh_report: bool,
    ) -> Option<CycleAction> {
        if fresh_report && device.activity_known {
            let rest_violation = self.status.state == CycleState::Resting
                && (device.active || device.current_ma != Some(0));
            let settling_violation = self.status.state == CycleState::Settling
                && (device.active
                    || self
                        .current_device_mode()
                        .is_some_and(|mode| device.mode != Some(mode)));
            if rest_violation || settling_violation {
                self.set_interrupted_at(
                    "physical activity/current/mode contradicts cycle phase expectations",
                    now,
                );
                return None;
            }
        }
        if test.state == TestState::RecoveredUncertain && self.is_executing() {
            self.set_interrupted_at("physical state is uncertain", now);
            return None;
        }

        if self.status.state == CycleState::Stopping {
            if test.state == TestState::Stopped {
                self.status.state = CycleState::Stopped;
                self.freeze_elapsed(now);
                self.status.rest_remaining_seconds = None;
                self.pending_action = None;
            }
            return None;
        }

        if self.status.state == CycleState::Interrupted {
            return None;
        }

        if matches!(
            self.status.state,
            CycleState::StartingStep | CycleState::RunningStep | CycleState::Settling
        ) && matches!(test.state, TestState::Running | TestState::Completed)
            && device.mode.is_some()
            && self
                .current_device_mode()
                .is_some_and(|mode| device.mode != Some(mode))
        {
            self.set_interrupted_at(
                "physical device mode contradicts the current cycle step",
                now,
            );
            return None;
        }

        let expects_active_operation = self.status.state == CycleState::RunningStep
            || (self.status.state == CycleState::StartingStep && self.start_committed);
        if expects_active_operation && test.state == TestState::Stopped {
            self.set_interrupted_at("physical test stopped unexpectedly", now);
            return None;
        }

        match self.status.state {
            CycleState::StartingStep if self.start_committed => match test.state {
                TestState::Running => self.status.state = CycleState::RunningStep,
                TestState::Completed => self.enter_settling(),
                _ => {}
            },
            CycleState::RunningStep if test.state == TestState::Completed => {
                self.enter_settling();
            }
            CycleState::Settling
                if fresh_report
                    && device.activity_known
                    && !device.active
                    && device.current_ma == Some(0) =>
            {
                if self.receive_prefix {
                    self.settled_advance = Some(now);
                    return None;
                }
                return self.advance(now);
            }
            _ => {}
        }
        None
    }

    /// Confirms that an action was accepted by the physical controller.
    pub fn on_action_committed(&mut self, action: &CycleAction, test: &TestStatus) {
        if self.pending_action.as_ref() != Some(action) {
            return;
        }
        self.pending_action = None;
        match action {
            CycleAction::Start(_) => {
                self.start_committed = true;
                if test.state == TestState::Running {
                    self.status.state = CycleState::RunningStep;
                }
            }
            CycleAction::Stop
                if test.state == TestState::Stopped
                    && self.status.state != CycleState::Interrupted =>
            {
                self.status.state = CycleState::Stopped;
                self.freeze_elapsed(Instant::now());
            }
            CycleAction::Stop => {}
        }
    }

    pub fn on_action_failed(&mut self, reason: impl Into<String>) {
        self.pending_action = None;
        self.start_committed = false;
        self.set_interrupted(reason);
    }

    /// Updates rest timing and advances all expired rest-only work. A returned
    /// device start must still pass through the physical controller's safety
    /// validation.
    pub fn tick(&mut self, now: Instant) -> Option<CycleAction> {
        loop {
            if self.status.state != CycleState::Resting {
                return None;
            }
            let Some(clock) = self.rest_clock.clone() else {
                self.set_interrupted_at("rest timer is unavailable", now);
                return None;
            };
            let elapsed = now.saturating_duration_since(clock.started);
            if elapsed < clock.duration {
                self.status.rest_remaining_seconds =
                    Some(ceil_seconds(clock.duration.saturating_sub(elapsed)));
                return None;
            }

            let next_anchor = clock.started + clock.duration;
            if let Some(action) = self.advance(next_anchor) {
                return Some(action);
            }
        }
    }

    /// Requests a user stop. Only inactive work stops synchronously. Deduplicate
    /// in-flight actions, not completed wire submissions: those do not prove Stop.
    pub fn stop(&mut self, test: &TestStatus) -> Option<CycleAction> {
        if !self.is_executing() && self.status.state != CycleState::Interrupted {
            return None;
        }

        let may_be_active = matches!(
            test.state,
            TestState::Starting
                | TestState::Running
                | TestState::Stopping
                | TestState::RecoveredUncertain
        );
        if self.status.state == CycleState::Interrupted {
            return may_be_active
                .then(|| self.issue(CycleAction::Stop))
                .flatten();
        }

        self.rest_clock = None;
        self.start_committed = false;
        self.status.rest_remaining_seconds = None;
        if !may_be_active
            && matches!(
                self.status.state,
                CycleState::Preparing | CycleState::Resting | CycleState::Settling
            )
        {
            self.pending_action = None;
            self.status.state = CycleState::Stopped;
            self.freeze_elapsed(Instant::now());
            return None;
        }
        if may_be_active {
            self.status.state = CycleState::Stopping;
            if self.pending_action == Some(CycleAction::Stop) {
                return None;
            }
            self.pending_action = None;
            return self.issue(CycleAction::Stop);
        }

        self.pending_action = None;
        self.status.state = CycleState::Stopped;
        self.freeze_elapsed(Instant::now());
        None
    }

    pub fn interrupt(&mut self, reason: impl Into<String>) {
        if self.is_executing() {
            self.set_interrupted(reason);
        }
    }

    pub fn interrupt_for_gap(&mut self, reason: impl Into<String>) {
        self.interrupt(reason);
    }

    /// Shared controller/orchestration boundary for every physical backend.
    /// Must run before timers, user commands, and newly recovered reports.
    pub fn expire_report_freshness(
        &mut self,
        controller: &mut TestController,
        now: Instant,
    ) -> bool {
        if !controller.expire_report_freshness(now) {
            return false;
        }
        if self.is_executing() {
            self.set_interrupted_at(REPORT_TIMEOUT_REASON, now);
        }
        true
    }

    fn begin_current_step(&mut self, started: Instant) -> Option<CycleAction> {
        let step = self.current_step()?.clone();
        match step {
            CycleStep::Device { config, .. } => {
                self.rest_clock = None;
                self.start_committed = false;
                self.status.rest_remaining_seconds = None;
                self.status.state = CycleState::StartingStep;
                self.issue(CycleAction::Start(config))
            }
            CycleStep::Rest { duration_seconds } => {
                let duration = Duration::from_secs(duration_seconds);
                self.start_committed = false;
                self.status.state = CycleState::Resting;
                self.status.rest_remaining_seconds = Some(duration_seconds);
                self.rest_clock = Some(RestClock { started, duration });
                None
            }
        }
    }

    fn current_step(&self) -> Option<&CycleStep> {
        self.status
            .recipe
            .as_ref()?
            .steps
            .get(self.status.step_index)
    }

    fn current_device_mode(&self) -> Option<crate::device::DeviceMode> {
        match self.current_step()? {
            CycleStep::Device { config, .. } => Some(config.mode()),
            CycleStep::Rest { .. } => None,
        }
    }

    fn advance(&mut self, now: Instant) -> Option<CycleAction> {
        self.epoch = self
            .epoch
            .checked_add(1)
            .unwrap_or_else(|| panic!("cycle scheduling identities exhausted"));
        let recipe = self.status.recipe.as_ref()?;
        if self.status.step_index + 1 < recipe.steps.len() {
            self.status.step_index += 1;
        } else if self.status.repeat_index + 1 < recipe.repeat_count {
            self.status.repeat_index += 1;
            self.status.step_index = 0;
        } else {
            self.status.state = CycleState::Completed;
            self.freeze_elapsed(now);
            self.status.rest_remaining_seconds = None;
            self.rest_clock = None;
            self.pending_action = None;
            return None;
        }
        self.begin_current_step(now)
    }

    fn enter_settling(&mut self) {
        self.status.state = CycleState::Settling;
        self.status.rest_remaining_seconds = None;
        self.rest_clock = None;
    }

    fn issue(&mut self, action: CycleAction) -> Option<CycleAction> {
        if self.pending_action.is_some() {
            return None;
        }
        self.pending_action = Some(action);
        Some(action)
    }

    fn set_interrupted(&mut self, reason: impl Into<String>) {
        self.set_interrupted_at(reason, Instant::now());
    }

    fn freeze_elapsed(&mut self, now: Instant) {
        if self.cycle_ended.is_none() {
            self.cycle_ended = Some(now);
        }
    }

    fn set_interrupted_at(&mut self, reason: impl Into<String>, now: Instant) {
        self.freeze_elapsed(now);
        self.epoch = self
            .epoch
            .checked_add(1)
            .unwrap_or_else(|| panic!("cycle scheduling identities exhausted"));
        self.settled_advance = None;
        self.status.state = CycleState::Interrupted;
        self.status.result = Some(reason.into());
        self.status.rest_remaining_seconds = None;
        self.pending_action = None;
        self.rest_clock = None;
        self.start_committed = false;
    }
}

fn is_nonterminal(state: CycleState) -> bool {
    matches!(
        state,
        CycleState::Preparing
            | CycleState::StartingStep
            | CycleState::RunningStep
            | CycleState::Settling
            | CycleState::Resting
            | CycleState::Stopping
    )
}

fn ceil_seconds(duration: Duration) -> u64 {
    duration
        .as_secs()
        .saturating_add(u64::from(duration.subsec_nanos() != 0))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "cycle tests should fail fast")]
mod tests {
    use super::*;
    use crate::core::CycleStepCompletion;

    #[test]
    fn final_completion_waits_for_prefix_and_latches_intermediate_activity() {
        let now = Instant::now();
        let mut engine = CycleEngine::new();
        let action =
            start(&mut engine, recipe(vec![device_step(100)], 1), now).expect("child Start");
        engine.on_action_committed(&action, &status(TestState::Starting));
        let mut device = settled_device();
        device.active = true;
        engine.on_physical_state(now, &device, &status(TestState::Running), true);
        device.active = false;
        engine.on_physical_state(now, &device, &status(TestState::Completed), true);
        engine.begin_receive_prefix();
        engine.on_physical_state(now, &device, &status(TestState::Completed), true);
        assert_eq!(
            engine.status().state,
            CycleState::Settling,
            "zero-current report cannot commit terminal progress before prefix reconciliation"
        );
        device.active = true;
        engine.on_physical_state(now, &device, &status(TestState::Completed), true);
        device.active = false;
        engine.on_physical_state(now, &device, &status(TestState::Completed), true);
        assert_eq!(engine.finish_receive_prefix(), None);
        assert_eq!(engine.status().state, CycleState::Interrupted);
    }

    #[test]
    fn terminal_cycle_duration_is_frozen_at_completion_or_interruption() {
        let now = Instant::now();
        let mut completed = CycleEngine::new();
        start(
            &mut completed,
            recipe(
                vec![CycleStep::Rest {
                    duration_seconds: 2,
                }],
                1,
            ),
            now,
        );
        assert_eq!(completed.tick(now + Duration::from_secs(2)), None);
        assert_eq!(completed.status().state, CycleState::Completed);
        assert_eq!(
            completed.elapsed(now + Duration::from_secs(100)),
            Duration::from_secs(2)
        );

        let mut interrupted = CycleEngine::new();
        start(
            &mut interrupted,
            recipe(
                vec![CycleStep::Rest {
                    duration_seconds: 20,
                }],
                1,
            ),
            now,
        );
        let mut active = settled_device();
        active.active = true;
        interrupted.on_physical_state(
            now + Duration::from_secs(3),
            &active,
            &status(TestState::RecoveredUncertain),
            true,
        );
        assert_eq!(interrupted.status().state, CycleState::Interrupted);
        assert_eq!(
            interrupted.elapsed(now + Duration::from_secs(100)),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn rest_and_settling_reject_activity_independently_of_test_presentation() {
        for phase in [CycleState::Resting, CycleState::Settling] {
            for active in [false, true] {
                let now = Instant::now();
                let mut engine = CycleEngine::new();
                start(
                    &mut engine,
                    recipe(vec![device_step(100), device_step(100)], 1),
                    now,
                );
                engine.status.state = phase;
                let mut device = settled_device();
                device.mode = Some(crate::device::DeviceMode::DischargeConstantCurrent);
                device.active = active;
                device.current_ma = Some(1);
                engine.on_physical_state(now, &device, &status(TestState::Completed), true);
                if active || phase == CycleState::Resting {
                    assert_eq!(engine.status().state, CycleState::Interrupted);
                    device.active = false;
                    device.current_ma = Some(0);
                    assert_eq!(
                        engine.on_physical_state(now, &device, &status(TestState::Completed), true),
                        None
                    );
                    assert_eq!(engine.status().state, CycleState::Interrupted);
                }
            }
        }
    }

    fn config(current_ma: u16) -> TestConfiguration {
        TestConfiguration::DischargeConstantCurrent {
            current_ma,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 0,
        }
    }

    fn device_step(current_ma: u16) -> CycleStep {
        CycleStep::Device {
            config: config(current_ma),
            completion: CycleStepCompletion::Hardware,
        }
    }

    fn recipe(steps: Vec<CycleStep>, repeat_count: u32) -> CycleRecipe {
        CycleRecipe {
            steps,
            repeat_count,
        }
    }

    fn status(state: TestState) -> TestStatus {
        TestStatus {
            state,
            ..TestStatus::default()
        }
    }

    fn settled_device() -> DeviceState {
        DeviceState {
            mode: Some(crate::device::DeviceMode::DischargeConstantCurrent),
            activity_known: true,
            active: false,
            current_ma: Some(0),
            ..DeviceState::default()
        }
    }

    #[test]
    fn report_timeout_interrupts_every_nonterminal_phase_once() {
        use crate::controller::{
            ControllerMode, DeviceReport, REPORT_FRESHNESS_TIMEOUT, ReportState,
        };
        let t0 = Instant::now();
        for phase in [
            CycleState::Preparing,
            CycleState::StartingStep,
            CycleState::RunningStep,
            CycleState::Settling,
            CycleState::Resting,
            CycleState::Stopping,
        ] {
            let mut controller = TestController::new(ControllerMode::Server);
            controller.begin_connection("connect");
            controller.connection_established_at(t0);
            let idle = DeviceReport {
                mode: crate::device::DeviceMode::DischargeConstantCurrent,
                state: ReportState::Idle,
                voltage_mv: 4000,
                current_ma: 0,
                capacity_mah: 0,
                model: "EBC-A20".to_owned(),
                firmware_version: None,
            };
            controller.report_at(idle.clone(), t0);
            let mut engine = CycleEngine::new();
            start(
                &mut engine,
                recipe(
                    vec![
                        CycleStep::Rest {
                            duration_seconds: REPORT_FRESHNESS_TIMEOUT.as_secs(),
                        },
                        device_step(100),
                    ],
                    1,
                ),
                t0,
            );
            engine.status.state = phase;
            assert!(!engine.expire_report_freshness(
                &mut controller,
                t0 + REPORT_FRESHNESS_TIMEOUT.saturating_sub(Duration::from_nanos(1))
            ));
            assert_eq!(engine.status.state, phase);
            // Rest expiry and report expiry on the same tick: interruption wins.
            assert!(engine.expire_report_freshness(&mut controller, t0 + REPORT_FRESHNESS_TIMEOUT));
            assert_eq!(engine.status.state, CycleState::Interrupted);
            assert_eq!(engine.status.result.as_deref(), Some(REPORT_TIMEOUT_REASON));
            assert_eq!(engine.pending_action(), None);
            assert_eq!(engine.tick(t0 + Duration::from_secs(65)), None);
            let interrupted = engine.status.clone();
            assert!(!engine.expire_report_freshness(&mut controller, t0 + Duration::from_secs(65)));
            controller.report_at(idle, t0 + Duration::from_secs(66));
            assert_eq!(
                engine.on_physical_state(
                    t0 + Duration::from_secs(66),
                    controller.device(),
                    controller.test(),
                    true
                ),
                None
            );
            assert_eq!(engine.tick(t0 + Duration::from_secs(100)), None);
            assert_eq!(engine.status, interrupted);
        }
    }

    fn start(engine: &mut CycleEngine, recipe: CycleRecipe, now: Instant) -> Option<CycleAction> {
        engine
            .start(
                recipe,
                "execution".to_owned(),
                None,
                None,
                Some("timestamp".to_owned()),
                now,
            )
            .expect("valid recipe")
    }

    #[test]
    fn mixed_steps_repeat_and_require_a_later_fresh_settle_report() {
        let now = Instant::now();
        let mut engine = CycleEngine::new();
        let action = start(
            &mut engine,
            recipe(
                vec![
                    device_step(100),
                    CycleStep::Rest {
                        duration_seconds: 2,
                    },
                ],
                2,
            ),
            now,
        );
        assert_eq!(action, Some(CycleAction::Start(config(100))));
        assert!(
            engine
                .start(
                    recipe(vec![device_step(100)], 1),
                    "other".to_owned(),
                    None,
                    None,
                    Some("timestamp".to_owned()),
                    now,
                )
                .is_err()
        );

        engine.on_action_committed(
            action.as_ref().expect("start action"),
            &status(TestState::Starting),
        );
        engine.on_physical_state(
            now,
            &DeviceState::default(),
            &status(TestState::Running),
            true,
        );
        assert_eq!(engine.status().state, CycleState::RunningStep);

        assert_eq!(
            engine.on_physical_state(now, &settled_device(), &status(TestState::Completed), true),
            None
        );
        assert_eq!(engine.status().state, CycleState::Settling);
        assert_eq!(
            engine.on_physical_state(now, &settled_device(), &status(TestState::Completed), false),
            None
        );
        assert_eq!(engine.status().state, CycleState::Settling);
        engine.on_physical_state(now, &settled_device(), &status(TestState::Completed), true);
        assert_eq!(engine.status().state, CycleState::Resting);

        let second = engine.tick(now + Duration::from_secs(2));
        assert_eq!(second, Some(CycleAction::Start(config(100))));
        assert_eq!(engine.status().repeat_index, 1);
        assert_eq!(engine.status().step_index, 0);
        engine.on_physical_state(
            now + Duration::from_secs(2),
            &settled_device(),
            &status(TestState::Completed),
            true,
        );
        assert_eq!(engine.status().state, CycleState::StartingStep);
    }

    #[test]
    fn final_device_must_settle_before_completion() {
        let now = Instant::now();
        let mut engine = CycleEngine::new();
        let action = start(&mut engine, recipe(vec![device_step(100)], 1), now);
        engine.on_action_committed(action.as_ref().expect("start"), &status(TestState::Running));
        engine.on_physical_state(now, &settled_device(), &status(TestState::Completed), true);
        assert_eq!(engine.status().state, CycleState::Settling);
        engine.on_physical_state(now, &settled_device(), &status(TestState::Completed), true);
        assert_eq!(engine.status().state, CycleState::Completed);
    }

    #[test]
    fn cycle_elapsed_uses_one_monotonic_clock_across_steps_and_repeats() {
        let now = Instant::now();
        let mut engine = CycleEngine::new();
        let action = start(
            &mut engine,
            recipe(
                vec![
                    device_step(100),
                    CycleStep::Rest {
                        duration_seconds: 2,
                    },
                ],
                2,
            ),
            now,
        );
        engine.on_action_committed(
            action.as_ref().expect("start action"),
            &status(TestState::Running),
        );

        assert_eq!(engine.elapsed(now), Duration::ZERO);
        assert_eq!(
            engine
                .elapsed(now + Duration::from_millis(1500))
                .as_millis(),
            1500
        );
        engine.on_physical_state(
            now + Duration::from_secs(2),
            &settled_device(),
            &status(TestState::Completed),
            true,
        );
        engine.on_physical_state(
            now + Duration::from_secs(3),
            &settled_device(),
            &status(TestState::Completed),
            true,
        );
        assert_eq!(engine.status().state, CycleState::Resting);
        assert_eq!(engine.elapsed(now + Duration::from_secs(4)).as_secs(), 4);

        engine.status.state = CycleState::Stopped;
        start(
            &mut engine,
            recipe(vec![device_step(200)], 1),
            now + Duration::from_secs(10),
        );
        assert_eq!(
            engine.elapsed(now + Duration::from_secs(10)),
            Duration::ZERO
        );
    }

    #[test]
    fn settling_rejects_a_report_for_another_device_mode() {
        let now = Instant::now();
        let mut engine = CycleEngine::new();
        let action = start(&mut engine, recipe(vec![device_step(100)], 1), now);
        engine.on_action_committed(
            action.as_ref().expect("start"),
            &status(TestState::Starting),
        );
        let mut device = settled_device();
        device.mode = Some(crate::device::DeviceMode::DischargeConstantCurrent);
        engine.on_physical_state(now, &device, &status(TestState::Running), true);
        engine.on_physical_state(now, &device, &status(TestState::Completed), true);
        assert_eq!(engine.status().state, CycleState::Settling);

        device.mode = Some(crate::device::DeviceMode::ChargeConstantVoltage);
        engine.on_physical_state(now, &device, &status(TestState::Completed), true);

        assert_eq!(engine.status().state, CycleState::Interrupted);
        assert!(
            engine
                .status()
                .result
                .as_deref()
                .is_some_and(|reason| reason.contains("mode contradicts"))
        );
    }

    #[test]
    fn rest_only_recipe_uses_monotonic_time() {
        let now = Instant::now();
        let mut engine = CycleEngine::new();
        assert_eq!(
            start(
                &mut engine,
                recipe(
                    vec![
                        CycleStep::Rest {
                            duration_seconds: 1
                        },
                        CycleStep::Rest {
                            duration_seconds: 2
                        },
                    ],
                    2,
                ),
                now,
            ),
            None
        );
        assert_eq!(engine.tick(now), None);
        assert_eq!(engine.status().rest_remaining_seconds, Some(1));
        assert_eq!(engine.tick(now + Duration::from_millis(500)), None);
        assert_eq!(engine.status().rest_remaining_seconds, Some(1));
        assert_eq!(engine.tick(now + Duration::from_secs(1)), None);
        assert_eq!(engine.status().step_index, 1);
        assert_eq!(engine.tick(now + Duration::from_secs(3)), None);
        assert_eq!(engine.status().repeat_index, 1);
        assert_eq!(engine.status().step_index, 0);
        assert_eq!(engine.tick(now + Duration::from_secs(6)), None);
        assert_eq!(engine.status().state, CycleState::Completed);
    }

    #[test]
    fn stopped_physical_state_is_expected_during_rest() {
        let now = Instant::now();
        let mut engine = CycleEngine::new();
        assert_eq!(
            start(
                &mut engine,
                recipe(
                    vec![
                        CycleStep::Rest {
                            duration_seconds: 2,
                        },
                        device_step(100),
                    ],
                    1,
                ),
                now,
            ),
            None
        );

        assert_eq!(
            engine.on_physical_state(
                now + Duration::from_secs(1),
                &settled_device(),
                &status(TestState::Stopped),
                true,
            ),
            None
        );
        assert_eq!(engine.status().state, CycleState::Resting);
        assert_eq!(
            engine.tick(now + Duration::from_secs(2)),
            Some(CycleAction::Start(config(100)))
        );
        assert_eq!(engine.status().state, CycleState::StartingStep);
    }

    #[test]
    fn stopped_physical_state_is_ignored_before_step_start_commits() {
        let now = Instant::now();
        let mut engine = CycleEngine::new();
        assert_eq!(
            start(&mut engine, recipe(vec![device_step(100)], 1), now),
            Some(CycleAction::Start(config(100)))
        );

        engine.on_physical_state(now, &settled_device(), &status(TestState::Stopped), true);

        assert_eq!(engine.status().state, CycleState::StartingStep);
        assert_eq!(
            engine.pending_action(),
            Some(&CycleAction::Start(config(100)))
        );
    }

    #[test]
    fn stopped_physical_state_does_not_interrupt_settling() {
        let now = Instant::now();
        let mut engine = CycleEngine::new();
        let action = start(&mut engine, recipe(vec![device_step(100)], 1), now);
        engine.on_action_committed(action.as_ref().expect("start"), &status(TestState::Running));
        let mut stale_current = settled_device();
        stale_current.current_ma = Some(1000);
        engine.on_physical_state(now, &stale_current, &status(TestState::Completed), true);
        assert_eq!(engine.status().state, CycleState::Settling);

        engine.on_physical_state(now, &stale_current, &status(TestState::Stopped), true);
        assert_eq!(engine.status().state, CycleState::Settling);
        engine.on_physical_state(now, &settled_device(), &status(TestState::Stopped), true);
        assert_eq!(engine.status().state, CycleState::Completed);
    }

    #[test]
    fn stop_abort_and_unexpected_stop_have_distinct_results() {
        let now = Instant::now();
        let mut engine = CycleEngine::new();
        let start_action = start(&mut engine, recipe(vec![device_step(100)], 1), now);
        assert_eq!(
            engine.stop(&status(TestState::Starting)),
            Some(CycleAction::Stop)
        );
        assert_eq!(engine.stop(&status(TestState::Starting)), None);
        engine.on_action_committed(&CycleAction::Stop, &status(TestState::Stopped));
        assert_eq!(engine.status().state, CycleState::Stopped);

        let mut unexpected = CycleEngine::new();
        let action = start(&mut unexpected, recipe(vec![device_step(100)], 1), now);
        unexpected
            .on_action_committed(action.as_ref().expect("start"), &status(TestState::Running));
        unexpected.on_physical_state(
            now,
            &DeviceState::default(),
            &status(TestState::Stopped),
            true,
        );
        assert_eq!(unexpected.status().state, CycleState::Interrupted);
        assert!(
            unexpected
                .status()
                .result
                .as_deref()
                .expect("reason")
                .contains("unexpectedly")
        );
        assert_eq!(start_action, Some(CycleAction::Start(config(100))));
    }

    #[test]
    fn interrupted_safety_stop_deduplicates_and_retries_after_failure() {
        let now = Instant::now();
        let mut engine = CycleEngine::new();
        start(&mut engine, recipe(vec![device_step(100)], 1), now);
        assert_eq!(
            engine.on_physical_state(
                now,
                &DeviceState::default(),
                &status(TestState::RecoveredUncertain),
                true,
            ),
            None
        );
        assert_eq!(
            engine.stop(&status(TestState::RecoveredUncertain)),
            Some(CycleAction::Stop)
        );
        assert_eq!(engine.stop(&status(TestState::RecoveredUncertain)), None);
        engine.on_action_failed("stop write failed");
        assert_eq!(engine.status().state, CycleState::Interrupted);
        assert_eq!(
            engine.stop(&status(TestState::RecoveredUncertain)),
            Some(CycleAction::Stop)
        );
        engine.on_action_committed(&CycleAction::Stop, &status(TestState::Stopped));
        assert_eq!(engine.status().state, CycleState::Interrupted);
        assert_eq!(engine.stop(&status(TestState::Stopped)), None);
        assert_eq!(
            engine.stop(&status(TestState::RecoveredUncertain)),
            Some(CycleAction::Stop)
        );
    }

    #[test]
    fn action_failure_and_gap_interrupt_without_duplicate_start() {
        let now = Instant::now();
        let mut engine = CycleEngine::new();
        start(&mut engine, recipe(vec![device_step(100)], 1), now);
        assert_eq!(engine.tick(now), None);
        assert_eq!(
            engine.pending_action(),
            Some(&CycleAction::Start(config(100)))
        );
        engine.on_action_failed("write failed");
        assert_eq!(engine.status().state, CycleState::Interrupted);
        assert_eq!(engine.status().result.as_deref(), Some("write failed"));

        let mut gap = CycleEngine::new();
        start(
            &mut gap,
            recipe(
                vec![CycleStep::Rest {
                    duration_seconds: 5,
                }],
                1,
            ),
            now,
        );
        gap.interrupt_for_gap("report gap");
        assert_eq!(gap.status().state, CycleState::Interrupted);
    }

    #[cfg(any(feature = "gui", feature = "server"))]
    #[test]
    fn persisted_active_status_round_trips_then_recovers_interrupted() {
        let now = Instant::now();
        let mut engine = CycleEngine::new();
        let action = engine
            .start(
                recipe(vec![device_step(100)], 1),
                "execution".to_owned(),
                Some("Formation cycle".to_owned()),
                Some(SavedRecipeReference {
                    id: "recipe-1".to_owned(),
                    name: "Formation".to_owned(),
                    revision: 7,
                }),
                Some("timestamp".to_owned()),
                now,
            )
            .expect("valid recipe");
        assert_eq!(engine.status().state, CycleState::StartingStep);
        assert_eq!(engine.status().name.as_deref(), Some("Formation cycle"));
        engine.on_action_committed(
            action.as_ref().expect("start action"),
            &status(TestState::Running),
        );
        assert_eq!(engine.status().state, CycleState::RunningStep);
        assert_eq!(engine.status().name.as_deref(), Some("Formation cycle"));
        let json = serde_json::to_string(engine.status()).expect("serialize cycle status");
        let persisted = serde_json::from_str(&json).expect("deserialize cycle status");
        let recovered = CycleEngine::from_status(persisted);
        assert_eq!(recovered.status().state, CycleState::Interrupted);
        assert_eq!(
            recovered.status().execution_id.as_deref(),
            Some("execution")
        );
        assert_eq!(recovered.status().name.as_deref(), Some("Formation cycle"));
        assert_eq!(
            recovered.status().saved_recipe,
            Some(SavedRecipeReference {
                id: "recipe-1".to_owned(),
                name: "Formation".to_owned(),
                revision: 7,
            })
        );
        assert_eq!(recovered.status().step_index, 0);
        assert_eq!(recovered.status().result.as_deref(), Some(RESTART_REASON));
        assert_eq!(recovered.pending_action(), None);
    }
}
