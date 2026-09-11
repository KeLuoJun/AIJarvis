use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, Weak},
};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    error::AppError,
    types::{Feature, FeatureSettings, LifecycleState, PrivacyState, RuntimeSnapshot, WorkScope},
};

#[derive(Clone, Debug)]
pub struct PrivacyGate {
    inner: Arc<Mutex<GateState>>,
}

#[derive(Debug)]
struct GateState {
    revision: u64,
    privacy_state: PrivacyState,
    lifecycle_state: LifecycleState,
    features: FeatureSettings,
    generations: Generations,
    next_ticket_id: u64,
    active_work: HashMap<u64, ActiveWork>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Generations {
    global: u64,
    screen: u64,
    system_audio: u64,
    proactive_response: u64,
}

#[derive(Debug)]
struct ActiveWork {
    scope: WorkScope,
    cancellation: CancellationToken,
}

#[derive(Debug)]
pub struct WorkTicket {
    id: u64,
    scope: WorkScope,
    generations: TicketGenerations,
    cancellation: CancellationToken,
    gate: Weak<Mutex<GateState>>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum CommitOutcome<T> {
    Committed,
    Stale(T),
    QueueFull(T),
    QueueClosed(T),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TicketGenerations {
    global: u64,
    screen: Option<u64>,
    system_audio: Option<u64>,
    proactive_response: Option<u64>,
}

impl PrivacyGate {
    pub fn new(privacy_state: PrivacyState, features: FeatureSettings) -> Self {
        Self {
            inner: Arc::new(Mutex::new(GateState {
                revision: 0,
                privacy_state,
                lifecycle_state: LifecycleState::Running,
                features,
                generations: Generations::default(),
                next_ticket_id: 0,
                active_work: HashMap::new(),
            })),
        }
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        let state = self.lock();
        RuntimeSnapshot {
            revision: state.revision,
            runtime_generation: state.generations.global,
            privacy_state: state.privacy_state,
            lifecycle_state: state.lifecycle_state,
            features: state.features,
            diagnostics: Vec::new(),
        }
    }

    pub fn admit(&self, scope: WorkScope) -> Result<WorkTicket, AppError> {
        let mut state = self.lock();
        ensure_running(&state)?;
        ensure_scope_enabled(&state, scope)?;

        let id = increment(state.next_ticket_id)?;
        state.next_ticket_id = id;
        let cancellation = CancellationToken::new();
        let generations = TicketGenerations::capture(state.generations, scope);
        state.active_work.insert(
            id,
            ActiveWork {
                scope,
                cancellation: cancellation.clone(),
            },
        );

        Ok(WorkTicket {
            id,
            scope,
            generations,
            cancellation,
            gate: Arc::downgrade(&self.inner),
        })
    }

    #[cfg(test)]
    fn can_commit(&self, ticket: &WorkTicket) -> bool {
        let state = self.lock();
        self.ticket_is_current(&state, ticket)
    }

    /// Atomically validate a ticket and enqueue its final side effect.
    ///
    /// `try_send` is non-blocking, so pause cannot be indefinitely delayed by
    /// a result sink and no arbitrary callback can re-enter the privacy gate.
    pub fn try_commit<T>(
        &self,
        ticket: &WorkTicket,
        sender: &mpsc::Sender<T>,
        value: T,
    ) -> CommitOutcome<T> {
        let state = self.lock();
        if !self.ticket_is_current(&state, ticket) {
            return CommitOutcome::Stale(value);
        }

        match sender.try_send(value) {
            Ok(()) => CommitOutcome::Committed,
            Err(mpsc::error::TrySendError::Full(value)) => CommitOutcome::QueueFull(value),
            Err(mpsc::error::TrySendError::Closed(value)) => CommitOutcome::QueueClosed(value),
        }
    }

    pub fn pause(&self) -> Result<bool, AppError> {
        let mut state = self.lock();
        if state.privacy_state == PrivacyState::Paused {
            return Ok(false);
        }
        ensure_running_lifecycle(&state)?;

        let next_revision = increment(state.revision)?;
        let next_global = increment(state.generations.global)?;
        state.revision = next_revision;
        state.generations.global = next_global;
        state.privacy_state = PrivacyState::Paused;
        cancel_matching(&mut state, |_| true);
        Ok(true)
    }

    pub fn resume(&self) -> Result<bool, AppError> {
        let mut state = self.lock();
        if state.privacy_state == PrivacyState::Active {
            return Ok(false);
        }
        ensure_running_lifecycle(&state)?;

        state.revision = increment(state.revision)?;
        state.privacy_state = PrivacyState::Active;
        Ok(true)
    }

    pub fn set_feature(&self, feature: Feature, enabled: bool) -> Result<bool, AppError> {
        let mut state = self.lock();
        if state.features.is_enabled(feature) == enabled {
            return Ok(false);
        }
        ensure_running_lifecycle(&state)?;

        let next_revision = increment(state.revision)?;
        let current_generation = generation_for_feature(state.generations, feature);
        let next_generation = increment(current_generation)?;

        state.revision = next_revision;
        state.features = state.features.with_feature(feature, enabled);
        set_generation_for_feature(&mut state.generations, feature, next_generation);
        cancel_matching(&mut state, |scope| scope_uses_feature(scope, feature));
        Ok(true)
    }

    pub fn begin_shutdown(&self) -> Result<bool, AppError> {
        let mut state = self.lock();
        if state.lifecycle_state != LifecycleState::Running {
            return Ok(false);
        }

        let next_revision = increment(state.revision)?;
        let next_global = increment(state.generations.global)?;
        state.revision = next_revision;
        state.generations.global = next_global;
        state.lifecycle_state = LifecycleState::ShuttingDown;
        cancel_matching(&mut state, |_| true);
        Ok(true)
    }

    pub fn finish_shutdown(&self) -> Result<bool, AppError> {
        let mut state = self.lock();
        match state.lifecycle_state {
            LifecycleState::Stopped => Ok(false),
            LifecycleState::Running => Err(AppError::new(
                crate::error::ErrorCode::InvalidArgument,
                "Shutdown has not started.",
            )),
            LifecycleState::ShuttingDown => {
                state.revision = increment(state.revision)?;
                state.lifecycle_state = LifecycleState::Stopped;
                Ok(true)
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn ticket_is_current(&self, state: &GateState, ticket: &WorkTicket) -> bool {
        Weak::ptr_eq(&ticket.gate, &Arc::downgrade(&self.inner))
            && state.active_work.contains_key(&ticket.id)
            && !ticket.cancellation.is_cancelled()
            && state.lifecycle_state == LifecycleState::Running
            && state.privacy_state == PrivacyState::Active
            && scope_is_enabled(state.features, ticket.scope)
            && ticket.generations.matches(state.generations)
    }
}

impl Default for PrivacyGate {
    fn default() -> Self {
        Self::new(PrivacyState::Paused, FeatureSettings::default())
    }
}

impl WorkTicket {
    pub const fn scope(&self) -> WorkScope {
        self.scope
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub const fn global_generation(&self) -> u64 {
        self.generations.global
    }
}

impl Drop for WorkTicket {
    fn drop(&mut self) {
        let Some(gate) = self.gate.upgrade() else {
            return;
        };
        let mut state = gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active_work.remove(&self.id);
    }
}

impl TicketGenerations {
    fn capture(generations: Generations, scope: WorkScope) -> Self {
        Self {
            global: generations.global,
            screen: scope.uses_screen().then_some(generations.screen),
            system_audio: scope
                .uses_system_audio()
                .then_some(generations.system_audio),
            proactive_response: scope
                .uses_proactive()
                .then_some(generations.proactive_response),
        }
    }

    fn matches(self, generations: Generations) -> bool {
        self.global == generations.global
            && optional_generation_matches(self.screen, generations.screen)
            && optional_generation_matches(self.system_audio, generations.system_audio)
            && optional_generation_matches(self.proactive_response, generations.proactive_response)
    }
}

const fn optional_generation_matches(expected: Option<u64>, actual: u64) -> bool {
    match expected {
        Some(expected) => expected == actual,
        None => true,
    }
}

fn ensure_running(state: &GateState) -> Result<(), AppError> {
    ensure_running_lifecycle(state)?;
    if state.privacy_state == PrivacyState::Paused {
        return Err(AppError::privacy_paused());
    }
    Ok(())
}

fn ensure_running_lifecycle(state: &GateState) -> Result<(), AppError> {
    if state.lifecycle_state != LifecycleState::Running {
        return Err(AppError::shutting_down());
    }
    Ok(())
}

fn ensure_scope_enabled(state: &GateState, scope: WorkScope) -> Result<(), AppError> {
    for feature in [
        Feature::Screen,
        Feature::SystemAudio,
        Feature::ProactiveResponse,
    ] {
        if scope_uses_feature(scope, feature) && !state.features.is_enabled(feature) {
            return Err(AppError::feature_disabled(feature));
        }
    }
    Ok(())
}

fn scope_is_enabled(features: FeatureSettings, scope: WorkScope) -> bool {
    (!scope.uses_screen() || features.screen_enabled)
        && (!scope.uses_system_audio() || features.system_audio_enabled)
        && (!scope.uses_proactive() || features.proactive_response_enabled)
}

const fn scope_uses_feature(scope: WorkScope, feature: Feature) -> bool {
    match feature {
        Feature::Screen => scope.uses_screen(),
        Feature::SystemAudio => scope.uses_system_audio(),
        Feature::ProactiveResponse => scope.uses_proactive(),
    }
}

const fn generation_for_feature(generations: Generations, feature: Feature) -> u64 {
    match feature {
        Feature::Screen => generations.screen,
        Feature::SystemAudio => generations.system_audio,
        Feature::ProactiveResponse => generations.proactive_response,
    }
}

fn set_generation_for_feature(generations: &mut Generations, feature: Feature, value: u64) {
    match feature {
        Feature::Screen => generations.screen = value,
        Feature::SystemAudio => generations.system_audio = value,
        Feature::ProactiveResponse => generations.proactive_response = value,
    }
}

fn cancel_matching(state: &mut GateState, predicate: impl Fn(WorkScope) -> bool) {
    state.active_work.retain(|_, work| {
        if predicate(work.scope) {
            work.cancellation.cancel();
            false
        } else {
            true
        }
    });
}

fn increment(value: u64) -> Result<u64, AppError> {
    value
        .checked_add(1)
        .ok_or_else(AppError::generation_exhausted)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Barrier},
        thread,
    };

    use crate::error::ErrorCode;

    use super::*;

    fn enabled_features() -> FeatureSettings {
        FeatureSettings {
            screen_enabled: true,
            system_audio_enabled: true,
            proactive_response_enabled: true,
        }
    }

    fn active_gate() -> PrivacyGate {
        PrivacyGate::new(PrivacyState::Active, enabled_features())
    }

    #[test]
    fn pause_cancels_work_and_generation_rejects_a_late_result() {
        let gate = active_gate();
        let ticket = gate.admit(WorkScope::Direct).expect("admit direct work");
        let ignored_cancellation = ticket.cancellation_token();

        assert!(gate.can_commit(&ticket));
        assert!(gate.pause().expect("pause"));
        assert!(ignored_cancellation.is_cancelled());
        assert!(!gate.can_commit(&ticket));
        assert!(gate.resume().expect("resume"));
        assert!(!gate.can_commit(&ticket));
        assert!(gate.admit(WorkScope::Direct).is_ok());
    }

    #[test]
    fn final_commit_is_non_blocking_and_linearized_with_pause() {
        let gate = active_gate();
        let ticket = gate.admit(WorkScope::Direct).expect("direct ticket");
        let (sender, mut receiver) = mpsc::channel(1);

        assert!(gate.pause().expect("pause"));
        assert_eq!(
            gate.try_commit(&ticket, &sender, "late"),
            CommitOutcome::Stale("late")
        );
        assert_eq!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty));

        gate.resume().expect("resume");
        let current = gate.admit(WorkScope::Direct).expect("current ticket");
        assert_eq!(
            gate.try_commit(&current, &sender, "current"),
            CommitOutcome::Committed
        );
        assert_eq!(receiver.try_recv(), Ok("current"));
    }

    #[test]
    fn paused_gate_rejects_every_scope() {
        let gate = PrivacyGate::new(PrivacyState::Paused, enabled_features());
        for scope in [
            WorkScope::Direct,
            WorkScope::ScreenCapture,
            WorkScope::SystemAudioCapture,
            WorkScope::Observation {
                uses_screen: true,
                uses_system_audio: true,
            },
        ] {
            assert_eq!(
                gate.admit(scope)
                    .expect_err("paused admission must fail")
                    .code,
                ErrorCode::PrivacyPaused
            );
        }
    }

    #[test]
    fn feature_generations_cancel_only_dependent_work() {
        let gate = active_gate();
        let direct = gate.admit(WorkScope::Direct).expect("direct ticket");
        let screen = gate.admit(WorkScope::ScreenCapture).expect("screen ticket");
        let audio = gate
            .admit(WorkScope::SystemAudioCapture)
            .expect("audio ticket");
        let observation = gate
            .admit(WorkScope::Observation {
                uses_screen: true,
                uses_system_audio: true,
            })
            .expect("observation ticket");

        assert!(gate
            .set_feature(Feature::ProactiveResponse, false)
            .expect("disable proactive response"));
        assert!(gate.can_commit(&direct));
        assert!(gate.can_commit(&screen));
        assert!(gate.can_commit(&audio));
        assert!(!gate.can_commit(&observation));
        assert!(!direct.is_cancelled());
        assert!(observation.is_cancelled());

        assert!(gate
            .set_feature(Feature::Screen, false)
            .expect("disable screen"));
        assert!(!gate.can_commit(&screen));
        assert!(gate.can_commit(&audio));
        assert!(gate.can_commit(&direct));
    }

    #[test]
    fn repeated_transitions_are_idempotent() {
        let gate = active_gate();
        let start_revision = gate.snapshot().revision;

        assert!(!gate.resume().expect("already active"));
        assert!(!gate
            .set_feature(Feature::Screen, true)
            .expect("same feature value"));
        assert_eq!(gate.snapshot().revision, start_revision);

        assert!(gate.pause().expect("first pause"));
        let paused_revision = gate.snapshot().revision;
        assert!(!gate.pause().expect("second pause"));
        assert_eq!(gate.snapshot().revision, paused_revision);
    }

    #[test]
    fn admission_and_pause_are_linearized_for_one_hundred_rounds() {
        let gate = active_gate();

        for _ in 0..100 {
            let barrier = Arc::new(Barrier::new(2));
            let worker_gate = gate.clone();
            let worker_barrier = barrier.clone();
            let worker = thread::spawn(move || {
                worker_barrier.wait();
                worker_gate.admit(WorkScope::Direct)
            });

            barrier.wait();
            gate.pause().expect("pause round");
            if let Ok(ticket) = worker.join().expect("admission worker") {
                assert!(ticket.is_cancelled());
                assert!(!gate.can_commit(&ticket));
            }
            gate.resume().expect("resume round");
        }
    }

    #[test]
    fn cancelled_tickets_do_not_accumulate_in_the_active_registry() {
        let gate = active_gate();
        let mut retained_tickets = Vec::new();

        for _ in 0..100 {
            retained_tickets.push(gate.admit(WorkScope::Direct).expect("direct ticket"));
            gate.pause().expect("pause round");
            assert!(gate.lock().active_work.is_empty());
            gate.resume().expect("resume round");
        }

        assert!(retained_tickets.iter().all(WorkTicket::is_cancelled));
    }

    #[test]
    fn shutdown_is_idempotent_and_permanently_closes_admission() {
        let gate = active_gate();
        let ticket = gate.admit(WorkScope::Direct).expect("direct ticket");

        assert!(gate.begin_shutdown().expect("begin shutdown"));
        assert!(ticket.is_cancelled());
        assert!(!gate.can_commit(&ticket));
        assert!(!gate.begin_shutdown().expect("repeat begin shutdown"));
        assert_eq!(
            gate.admit(WorkScope::Direct)
                .expect_err("shutdown rejects work")
                .code,
            ErrorCode::ShuttingDown
        );

        assert!(gate.finish_shutdown().expect("finish shutdown"));
        assert!(!gate.finish_shutdown().expect("repeat finish shutdown"));
        assert_eq!(gate.snapshot().lifecycle_state, LifecycleState::Stopped);
    }

    #[test]
    fn generation_overflow_fails_without_a_partial_pause() {
        let gate = active_gate();
        {
            let mut state = gate.lock();
            state.generations.global = u64::MAX;
        }

        assert_eq!(
            gate.pause().expect_err("overflow must fail").code,
            ErrorCode::GenerationExhausted
        );
        assert_eq!(gate.snapshot().privacy_state, PrivacyState::Active);
    }

    #[test]
    fn source_generation_overflow_does_not_partially_change_a_feature() {
        let gate = active_gate();
        {
            let mut state = gate.lock();
            state.generations.screen = u64::MAX;
        }

        assert_eq!(
            gate.set_feature(Feature::Screen, false)
                .expect_err("overflow must fail")
                .code,
            ErrorCode::GenerationExhausted
        );
        assert!(gate.snapshot().features.screen_enabled);
    }

    #[test]
    fn shutdown_generation_overflow_keeps_the_runtime_running() {
        let gate = active_gate();
        let ticket = gate.admit(WorkScope::Direct).expect("direct ticket");
        {
            let mut state = gate.lock();
            state.generations.global = u64::MAX;
        }

        assert_eq!(
            gate.begin_shutdown().expect_err("overflow must fail").code,
            ErrorCode::GenerationExhausted
        );
        assert_eq!(gate.snapshot().lifecycle_state, LifecycleState::Running);
        assert!(!ticket.is_cancelled());
    }

    #[test]
    fn a_poisoned_lock_is_recovered_without_a_gate_panic() {
        let gate = active_gate();
        let poison_gate = gate.clone();
        let _ = thread::spawn(move || {
            let _state = poison_gate.inner.lock().expect("initial lock");
            panic!("poison privacy gate for recovery test");
        })
        .join();

        assert_eq!(gate.snapshot().privacy_state, PrivacyState::Active);
        assert!(gate.pause().expect("pause through recovered lock"));
    }
}
