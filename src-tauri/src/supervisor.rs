//! Bounded runtime lanes and cancellation ownership.
//!
//! This module deliberately has no knowledge of application state.  It is the
//! small piece of runtime plumbing shared by capture, ASR, and model code.
//! Production producers must hold a `WorkTicket` before enqueueing and use
//! `PrivacyGate::try_commit` for the final cache/emit side effect.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{task::JoinHandle, time::Instant};
use tokio_util::sync::CancellationToken;

use crate::types::{Feature, WorkScope};

const MAX_LANE_CAPACITY: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisorLimits {
    pub frame_ring_capacity: usize,
    pub observation_queue_capacity: usize,
    pub direct_qwen_queue_capacity: usize,
    pub system_audio_segment_capacity: usize,
    pub user_voice_queue_capacity: usize,
    pub system_audio_queue_capacity: usize,
}

impl SupervisorLimits {
    pub const fn all_non_zero(self) -> bool {
        self.frame_ring_capacity > 0
            && self.observation_queue_capacity > 0
            && self.direct_qwen_queue_capacity > 0
            && self.system_audio_segment_capacity > 0
            && self.user_voice_queue_capacity > 0
            && self.system_audio_queue_capacity > 0
    }

    pub fn validate(self) -> Result<Self, SupervisorError> {
        if self.all_non_zero()
            && [
                self.frame_ring_capacity,
                self.observation_queue_capacity,
                self.direct_qwen_queue_capacity,
                self.system_audio_segment_capacity,
                self.user_voice_queue_capacity,
                self.system_audio_queue_capacity,
            ]
            .into_iter()
            .all(|capacity| capacity <= MAX_LANE_CAPACITY)
        {
            Ok(self)
        } else {
            Err(SupervisorError::InvalidLimits)
        }
    }

    pub const fn max_scoped_tasks(self) -> usize {
        self.frame_ring_capacity
            .saturating_add(self.observation_queue_capacity)
            .saturating_add(self.direct_qwen_queue_capacity)
            .saturating_add(self.system_audio_segment_capacity)
            .saturating_add(self.user_voice_queue_capacity)
            .saturating_add(self.system_audio_queue_capacity)
            .saturating_add(2)
    }
}

impl Default for SupervisorLimits {
    fn default() -> Self {
        Self {
            frame_ring_capacity: 8,
            observation_queue_capacity: 1,
            direct_qwen_queue_capacity: 4,
            system_audio_segment_capacity: 20,
            user_voice_queue_capacity: 1,
            system_audio_queue_capacity: 8,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorError {
    InvalidLimits,
    RuntimeUnavailable,
    QueueFull,
    InvalidScope,
    TaskIdExhausted,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnqueueOutcome {
    Accepted,
    ReplacedLatest,
    DroppedOldest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShutdownReport {
    pub completed: usize,
    pub aborted: usize,
}

pub type TaskId = u64;

struct TaskRecord {
    scope: WorkScope,
    cancellation: CancellationToken,
    on_cancel: Option<Box<dyn FnOnce() + Send>>,
    handle: JoinHandle<()>,
}

struct TaskCompletionGuard<Q, A, F, O> {
    state: std::sync::Weak<Mutex<SupervisorState<Q, A, F, O>>>,
    id: TaskId,
}

struct DirectReservationGuard<Q, A, F, O> {
    state: std::sync::Weak<Mutex<SupervisorState<Q, A, F, O>>>,
    id: TaskId,
}

impl<Q, A, F, O> Drop for DirectReservationGuard<Q, A, F, O> {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .direct_task_reservations
                .remove(&self.id);
        }
    }
}

impl<Q, A, F, O> Drop for TaskCompletionGuard<Q, A, F, O> {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .tasks
                .remove(&self.id);
        }
    }
}

struct SupervisorState<Q, A, F, O> {
    closed: bool,
    next_task_id: TaskId,
    frame_ring: VecDeque<F>,
    audio_ring: VecDeque<A>,
    direct_queue: VecDeque<Q>,
    direct_task_reservations: HashSet<TaskId>,
    observation_queue: VecDeque<ScopedValue<Q>>,
    user_voice_queue: VecDeque<A>,
    system_audio_queue: VecDeque<A>,
    // WorkScope is a finite enum (1 direct + 2 capture + 4 observation
    // combinations), so this cache has a hard maximum of seven entries.
    outputs: HashMap<WorkScope, O>,
    tasks: HashMap<TaskId, TaskRecord>,
}

struct ScopedValue<T> {
    scope: WorkScope,
    value: T,
}

/// Owns every bounded in-memory lane used by the runtime.
///
/// The payload is intentionally generic so AG-02 tests can use small fake
/// values while later capture/model modules can use their real DTOs.
pub struct Supervisor<Q = (), A = (), F = (), O = ()> {
    limits: SupervisorLimits,
    state: Arc<Mutex<SupervisorState<Q, A, F, O>>>,
}

impl<Q, A, F, O> Clone for Supervisor<Q, A, F, O> {
    fn clone(&self) -> Self {
        Self {
            limits: self.limits,
            state: Arc::clone(&self.state),
        }
    }
}

impl<Q, A, F, O> Supervisor<Q, A, F, O> {
    pub fn new(limits: SupervisorLimits) -> Result<Self, SupervisorError> {
        let limits = limits.validate()?;
        Ok(Self {
            limits,
            state: Arc::new(Mutex::new(SupervisorState {
                closed: false,
                next_task_id: 0,
                frame_ring: VecDeque::with_capacity(limits.frame_ring_capacity),
                audio_ring: VecDeque::with_capacity(limits.system_audio_segment_capacity),
                direct_queue: VecDeque::with_capacity(limits.direct_qwen_queue_capacity),
                direct_task_reservations: HashSet::new(),
                observation_queue: VecDeque::with_capacity(limits.observation_queue_capacity),
                user_voice_queue: VecDeque::with_capacity(limits.user_voice_queue_capacity),
                system_audio_queue: VecDeque::with_capacity(limits.system_audio_queue_capacity),
                outputs: HashMap::new(),
                tasks: HashMap::new(),
            })),
        })
    }

    pub fn with_limits(limits: SupervisorLimits) -> Result<Self, SupervisorError> {
        Self::new(limits)
    }

    pub fn limits(&self) -> SupervisorLimits {
        self.limits
    }

    pub fn max_scoped_tasks(&self) -> usize {
        self.limits.max_scoped_tasks()
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    pub fn push_frame(&self, value: F) -> Result<EnqueueOutcome, SupervisorError> {
        let mut state = self.lock();
        ensure_open(&state)?;
        Ok(push_ring(
            &mut state.frame_ring,
            self.limits.frame_ring_capacity,
            value,
        ))
    }

    pub fn enqueue_frame(&self, value: F) -> Result<EnqueueOutcome, SupervisorError> {
        self.push_frame(value)
    }

    pub fn push_audio(&self, value: A) -> Result<EnqueueOutcome, SupervisorError> {
        let mut state = self.lock();
        ensure_open(&state)?;
        Ok(push_ring(
            &mut state.audio_ring,
            self.limits.system_audio_segment_capacity,
            value,
        ))
    }

    pub fn enqueue_audio(&self, value: A) -> Result<EnqueueOutcome, SupervisorError> {
        self.push_audio(value)
    }

    pub fn pop_frame(&self) -> Option<F> {
        self.lock().frame_ring.pop_front()
    }

    pub fn pop_audio(&self) -> Option<A> {
        self.lock().audio_ring.pop_front()
    }

    pub fn try_enqueue_direct(&self, value: Q) -> Result<EnqueueOutcome, SupervisorError> {
        let mut state = self.lock();
        ensure_open(&state)?;
        if direct_lane_len(&state) == self.limits.direct_qwen_queue_capacity {
            return Err(SupervisorError::QueueFull);
        }
        let result = try_push_reject(
            &mut state.direct_queue,
            self.limits.direct_qwen_queue_capacity,
            value,
        );
        let cancelled = if result.is_ok() {
            state.observation_queue.clear();
            take_tasks(&mut state, WorkScope::uses_proactive)
        } else {
            Vec::new()
        };
        drop(state);
        cancel_task_records(cancelled);
        result
    }

    pub fn enqueue_direct(&self, value: Q) -> Result<EnqueueOutcome, SupervisorError> {
        self.try_enqueue_direct(value)
    }

    /// Atomically reserve direct-lane capacity and register its task.
    /// The reservation lives until the task completes or is cancelled.
    pub fn try_spawn_direct<TaskFactory, Fut, OnCancel>(
        &self,
        task: TaskFactory,
        on_cancel: OnCancel,
    ) -> Result<TaskId, SupervisorError>
    where
        TaskFactory: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
        OnCancel: FnOnce() + Send + 'static,
        Q: Send + 'static,
        A: Send + 'static,
        F: Send + 'static,
        O: Send + 'static,
    {
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(SupervisorError::RuntimeUnavailable);
        }
        let mut state_guard = self.lock();
        ensure_open(&state_guard)?;
        if direct_lane_len(&state_guard) == self.limits.direct_qwen_queue_capacity
            || state_guard.tasks.len() >= self.limits.max_scoped_tasks()
        {
            return Err(SupervisorError::QueueFull);
        }
        let id = state_guard
            .next_task_id
            .checked_add(1)
            .ok_or(SupervisorError::TaskIdExhausted)?;
        state_guard.next_task_id = id;
        state_guard.observation_queue.clear();
        let cancelled = take_tasks(&mut state_guard, WorkScope::uses_proactive);
        state_guard.direct_task_reservations.insert(id);

        let cancellation = CancellationToken::new();
        let task_token = cancellation.clone();
        let state = Arc::downgrade(&self.state);
        let reservation_state = state.clone();
        let handle = tokio::spawn(async move {
            let _completion = TaskCompletionGuard { state, id };
            let _reservation = DirectReservationGuard {
                state: reservation_state,
                id,
            };
            task(task_token).await;
        });
        state_guard.tasks.insert(
            id,
            TaskRecord {
                scope: WorkScope::Direct,
                cancellation,
                on_cancel: Some(Box::new(on_cancel)),
                handle,
            },
        );
        drop(state_guard);
        cancel_task_records(cancelled);
        Ok(id)
    }

    pub fn try_enqueue_observation(
        &self,
        scope: WorkScope,
        value: Q,
    ) -> Result<EnqueueOutcome, SupervisorError> {
        let mut state = self.lock();
        ensure_open(&state)?;
        if !matches!(scope, WorkScope::Observation { .. }) {
            return Err(SupervisorError::InvalidScope);
        }
        Ok(push_replace_latest(
            &mut state.observation_queue,
            self.limits.observation_queue_capacity,
            ScopedValue { scope, value },
        ))
    }

    pub fn try_enqueue_observation_all(&self, value: Q) -> Result<EnqueueOutcome, SupervisorError> {
        self.try_enqueue_observation(
            WorkScope::Observation {
                uses_screen: true,
                uses_system_audio: true,
            },
            value,
        )
    }

    pub fn enqueue_observation(
        &self,
        scope: WorkScope,
        value: Q,
    ) -> Result<EnqueueOutcome, SupervisorError> {
        self.try_enqueue_observation(scope, value)
    }

    pub fn try_enqueue_user_voice(&self, value: A) -> Result<EnqueueOutcome, SupervisorError> {
        let mut state = self.lock();
        ensure_open(&state)?;
        try_push_reject(
            &mut state.user_voice_queue,
            self.limits.user_voice_queue_capacity,
            value,
        )
    }

    pub fn enqueue_user_voice(&self, value: A) -> Result<EnqueueOutcome, SupervisorError> {
        self.try_enqueue_user_voice(value)
    }

    pub fn try_enqueue_system_audio(&self, value: A) -> Result<EnqueueOutcome, SupervisorError> {
        let mut state = self.lock();
        ensure_open(&state)?;
        Ok(push_drop_oldest(
            &mut state.system_audio_queue,
            self.limits.system_audio_queue_capacity,
            value,
        ))
    }

    pub fn enqueue_system_audio(&self, value: A) -> Result<EnqueueOutcome, SupervisorError> {
        self.try_enqueue_system_audio(value)
    }

    pub fn pop_direct(&self) -> Option<Q> {
        self.lock().direct_queue.pop_front()
    }

    pub fn pop_observation(&self) -> Option<Q> {
        self.lock()
            .observation_queue
            .pop_front()
            .map(|entry| entry.value)
    }

    pub fn pop_user_voice(&self) -> Option<A> {
        self.lock().user_voice_queue.pop_front()
    }

    pub fn pop_system_audio(&self) -> Option<A> {
        self.lock().system_audio_queue.pop_front()
    }

    pub fn queue_lengths(&self) -> QueueLengths {
        let state = self.lock();
        QueueLengths {
            frame_ring: state.frame_ring.len(),
            audio_ring: state.audio_ring.len(),
            direct_queue: direct_lane_len(&state),
            observation_queue: state.observation_queue.len(),
            user_voice_queue: state.user_voice_queue.len(),
            system_audio_queue: state.system_audio_queue.len(),
        }
    }

    pub fn cache_output(&self, scope: WorkScope, value: O) -> Result<(), SupervisorError> {
        let mut state = self.lock();
        ensure_open(&state)?;
        state.outputs.insert(scope, value);
        Ok(())
    }

    pub fn cache_output_for_scope(
        &self,
        scope: WorkScope,
        value: O,
    ) -> Result<(), SupervisorError> {
        self.cache_output(scope, value)
    }

    pub fn take_cached_output(&self, scope: WorkScope) -> Option<O> {
        self.lock().outputs.remove(&scope)
    }

    pub fn cached_output(&self, scope: WorkScope) -> Option<O>
    where
        O: Clone,
    {
        self.lock().outputs.get(&scope).cloned()
    }

    pub fn output_count(&self) -> usize {
        self.lock().outputs.len()
    }

    /// Remove all transient input/output data and cancel every scoped task.
    pub fn clear_all(&self) {
        let mut state = self.lock();
        clear_queues(&mut state);
        let cancelled = take_tasks(&mut state, |_| true);
        drop(state);
        cancel_task_records(cancelled);
    }

    /// Apply the exact source switch semantics from design section 7.6.
    pub fn clear_for_feature(&self, feature: Feature) {
        let mut state = self.lock();
        match feature {
            Feature::Screen => {
                state.frame_ring.clear();
                state
                    .observation_queue
                    .retain(|entry| !entry.scope.uses_screen());
                state.outputs.retain(|scope, _| !scope.uses_screen());
            }
            Feature::SystemAudio => {
                state.audio_ring.clear();
                state.system_audio_queue.clear();
                state
                    .observation_queue
                    .retain(|entry| !entry.scope.uses_system_audio());
                state.outputs.retain(|scope, _| !scope.uses_system_audio());
            }
            Feature::ProactiveResponse => {
                state.observation_queue.clear();
                state.outputs.retain(|scope, _| !scope.uses_proactive());
            }
        }
        let cancelled = take_tasks(&mut state, |scope| match feature {
            Feature::Screen => scope.uses_screen(),
            Feature::SystemAudio => scope.uses_system_audio(),
            Feature::ProactiveResponse => scope.uses_proactive(),
        });
        drop(state);
        cancel_task_records(cancelled);
    }

    /// Cancel all tasks, wait up to `timeout`, then abort anything still running.
    /// Draining the registry makes repeated shutdown calls harmless.
    pub async fn shutdown(&self, timeout: Duration) -> ShutdownReport {
        let (tasks, deadline) = {
            let mut state = self.lock();
            if !state.closed {
                state.closed = true;
            }
            clear_queues(&mut state);
            let tasks = state
                .tasks
                .drain()
                .map(|(_, task)| task)
                .collect::<Vec<_>>();
            (tasks, Instant::now() + timeout)
        };
        let mut handles = cancel_task_records_without_abort(tasks);

        let mut completed = 0;
        let mut aborted = 0;
        for mut handle in handles.drain(..) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                handle.abort();
                let _ = handle.await;
                aborted += 1;
                continue;
            }
            if tokio::time::timeout(remaining, &mut handle).await.is_ok() {
                completed += 1;
            } else {
                handle.abort();
                let _ = handle.await;
                aborted += 1;
            }
        }
        ShutdownReport { completed, aborted }
    }

    pub fn task_count(&self) -> usize {
        self.lock().tasks.len()
    }

    pub fn task_cancellation(&self, id: TaskId) -> Option<CancellationToken> {
        self.lock()
            .tasks
            .get(&id)
            .map(|task| task.cancellation.clone())
    }

    /// Register a task with a token owned by the supervisor.
    pub fn spawn_scoped<TaskFactory, Fut>(
        &self,
        scope: WorkScope,
        task: TaskFactory,
    ) -> Result<TaskId, SupervisorError>
    where
        TaskFactory: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
        Q: Send + 'static,
        A: Send + 'static,
        F: Send + 'static,
        O: Send + 'static,
    {
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(SupervisorError::RuntimeUnavailable);
        }
        let mut state_guard = self.lock();
        ensure_open(&state_guard)?;
        let id = state_guard
            .next_task_id
            .checked_add(1)
            .ok_or(SupervisorError::TaskIdExhausted)?;
        if state_guard.tasks.len() >= self.limits.max_scoped_tasks() {
            return Err(SupervisorError::QueueFull);
        }
        state_guard.next_task_id = id;
        let cancellation = CancellationToken::new();
        let task_token = cancellation.clone();
        let state = Arc::downgrade(&self.state);
        let handle = tokio::spawn(async move {
            let _completion = TaskCompletionGuard { state, id };
            task(task_token).await;
        });
        state_guard.tasks.insert(
            id,
            TaskRecord {
                scope,
                cancellation,
                on_cancel: None,
                handle,
            },
        );
        Ok(id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SupervisorState<Q, A, F, O>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueLengths {
    pub frame_ring: usize,
    pub audio_ring: usize,
    pub direct_queue: usize,
    pub observation_queue: usize,
    pub user_voice_queue: usize,
    pub system_audio_queue: usize,
}

impl QueueLengths {
    pub const fn within_limits(self, limits: SupervisorLimits) -> bool {
        self.frame_ring <= limits.frame_ring_capacity
            && self.audio_ring <= limits.system_audio_segment_capacity
            && self.direct_queue <= limits.direct_qwen_queue_capacity
            && self.observation_queue <= limits.observation_queue_capacity
            && self.user_voice_queue <= limits.user_voice_queue_capacity
            && self.system_audio_queue <= limits.system_audio_queue_capacity
    }
}

fn ensure_open<Q, A, F, O>(state: &SupervisorState<Q, A, F, O>) -> Result<(), SupervisorError> {
    if state.closed {
        Err(SupervisorError::Closed)
    } else {
        Ok(())
    }
}

fn push_ring<T>(queue: &mut VecDeque<T>, capacity: usize, value: T) -> EnqueueOutcome {
    let outcome = if queue.len() == capacity {
        queue.pop_front();
        EnqueueOutcome::DroppedOldest
    } else {
        EnqueueOutcome::Accepted
    };
    queue.push_back(value);
    outcome
}

fn try_push_reject<T>(
    queue: &mut VecDeque<T>,
    capacity: usize,
    value: T,
) -> Result<EnqueueOutcome, SupervisorError> {
    if queue.len() == capacity {
        return Err(SupervisorError::QueueFull);
    }
    queue.push_back(value);
    Ok(EnqueueOutcome::Accepted)
}

fn push_replace_latest<T>(queue: &mut VecDeque<T>, capacity: usize, value: T) -> EnqueueOutcome {
    if queue.len() == capacity {
        queue.pop_back();
        queue.push_back(value);
        EnqueueOutcome::ReplacedLatest
    } else {
        queue.push_back(value);
        EnqueueOutcome::Accepted
    }
}

fn push_drop_oldest<T>(queue: &mut VecDeque<T>, capacity: usize, value: T) -> EnqueueOutcome {
    push_ring(queue, capacity, value)
}

fn clear_queues<Q, A, F, O>(state: &mut SupervisorState<Q, A, F, O>) {
    state.frame_ring.clear();
    state.audio_ring.clear();
    state.direct_queue.clear();
    state.direct_task_reservations.clear();
    state.observation_queue.clear();
    state.user_voice_queue.clear();
    state.system_audio_queue.clear();
    state.outputs.clear();
}

fn direct_lane_len<Q, A, F, O>(state: &SupervisorState<Q, A, F, O>) -> usize {
    state
        .direct_queue
        .len()
        .saturating_add(state.direct_task_reservations.len())
}

fn take_tasks<Q, A, F, O>(
    state: &mut SupervisorState<Q, A, F, O>,
    predicate: impl Fn(WorkScope) -> bool,
) -> Vec<TaskRecord> {
    let ids = state
        .tasks
        .iter()
        .filter_map(|(id, task)| predicate(task.scope).then_some(*id))
        .collect::<Vec<_>>();
    ids.into_iter()
        .filter_map(|id| state.tasks.remove(&id))
        .collect()
}

fn cancel_task_records(tasks: Vec<TaskRecord>) {
    for mut task in tasks {
        task.cancellation.cancel();
        if let Some(on_cancel) = task.on_cancel.take() {
            on_cancel();
        }
        task.handle.abort();
    }
}

fn cancel_task_records_without_abort(tasks: Vec<TaskRecord>) -> Vec<JoinHandle<()>> {
    tasks
        .into_iter()
        .map(|mut task| {
            task.cancellation.cancel();
            if let Some(on_cancel) = task.on_cancel.take() {
                on_cancel();
            }
            task.handle
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::time::sleep;

    use super::*;

    fn limits() -> SupervisorLimits {
        SupervisorLimits {
            frame_ring_capacity: 2,
            observation_queue_capacity: 2,
            direct_qwen_queue_capacity: 2,
            system_audio_segment_capacity: 2,
            user_voice_queue_capacity: 1,
            system_audio_queue_capacity: 2,
        }
    }

    #[test]
    fn limits_reject_zero_capacity() {
        let mut bad = limits();
        bad.frame_ring_capacity = 0;
        assert!(matches!(
            Supervisor::<u8, u8, u8, u8>::new(bad),
            Err(SupervisorError::InvalidLimits)
        ));

        let mut too_large = limits();
        too_large.system_audio_segment_capacity = MAX_LANE_CAPACITY + 1;
        assert!(matches!(
            Supervisor::<u8, u8, u8, u8>::new(too_large),
            Err(SupervisorError::InvalidLimits)
        ));
    }

    #[test]
    fn rings_are_bounded_and_keep_the_newest_values() {
        let supervisor = Supervisor::<(), (), i32, ()>::new(limits()).expect("valid limits");
        assert_eq!(supervisor.push_frame(1), Ok(EnqueueOutcome::Accepted));
        assert_eq!(supervisor.push_frame(2), Ok(EnqueueOutcome::Accepted));
        assert_eq!(supervisor.push_frame(3), Ok(EnqueueOutcome::DroppedOldest));
        assert_eq!(supervisor.pop_frame(), Some(2));
        assert_eq!(supervisor.pop_frame(), Some(3));
    }

    #[test]
    fn direct_and_voice_lanes_reject_when_full() {
        let supervisor = Supervisor::<i32, i32, (), ()>::new(limits()).expect("valid limits");
        assert!(supervisor.try_enqueue_direct(1).is_ok());
        assert!(supervisor.try_enqueue_direct(2).is_ok());
        assert_eq!(
            supervisor.try_enqueue_direct(3),
            Err(SupervisorError::QueueFull)
        );
        assert!(supervisor.try_enqueue_user_voice(1).is_ok());
        assert_eq!(
            supervisor.try_enqueue_user_voice(2),
            Err(SupervisorError::QueueFull)
        );
    }

    #[test]
    fn observation_replaces_latest_and_audio_drops_oldest() {
        let supervisor = Supervisor::<i32, i32, (), ()>::new(limits()).expect("valid limits");
        supervisor
            .try_enqueue_observation(
                WorkScope::Observation {
                    uses_screen: true,
                    uses_system_audio: true,
                },
                1,
            )
            .unwrap();
        supervisor
            .try_enqueue_observation(
                WorkScope::Observation {
                    uses_screen: true,
                    uses_system_audio: true,
                },
                2,
            )
            .unwrap();
        assert_eq!(
            supervisor.try_enqueue_observation(
                WorkScope::Observation {
                    uses_screen: true,
                    uses_system_audio: true,
                },
                3,
            ),
            Ok(EnqueueOutcome::ReplacedLatest)
        );
        assert_eq!(supervisor.pop_observation(), Some(1));
        assert_eq!(supervisor.pop_observation(), Some(3));

        supervisor.try_enqueue_system_audio(1).unwrap();
        supervisor.try_enqueue_system_audio(2).unwrap();
        assert_eq!(
            supervisor.try_enqueue_system_audio(3),
            Ok(EnqueueOutcome::DroppedOldest)
        );
        assert_eq!(supervisor.pop_system_audio(), Some(2));
        assert_eq!(supervisor.pop_system_audio(), Some(3));
    }

    #[test]
    fn accepted_direct_work_clears_waiting_observation() {
        let supervisor = Supervisor::<u8, (), (), ()>::new(limits()).expect("valid limits");
        supervisor
            .try_enqueue_observation(
                WorkScope::Observation {
                    uses_screen: true,
                    uses_system_audio: true,
                },
                1,
            )
            .unwrap();
        supervisor.try_enqueue_direct(2).unwrap();
        assert_eq!(supervisor.pop_observation(), None);
        assert_eq!(supervisor.pop_direct(), Some(2));
    }

    #[test]
    fn feature_cleanup_does_not_touch_direct_work_or_output() {
        let supervisor = Supervisor::<i32, (), (), i32>::new(limits()).expect("valid limits");
        supervisor.try_enqueue_direct(7).unwrap();
        supervisor
            .try_enqueue_observation(
                WorkScope::Observation {
                    uses_screen: true,
                    uses_system_audio: true,
                },
                8,
            )
            .unwrap();
        supervisor
            .cache_output(WorkScope::Direct, 9)
            .expect("cache direct output");
        supervisor
            .cache_output(
                WorkScope::Observation {
                    uses_screen: true,
                    uses_system_audio: false,
                },
                10,
            )
            .expect("cache observation output");
        supervisor.clear_for_feature(Feature::ProactiveResponse);
        assert_eq!(supervisor.pop_direct(), Some(7));
        assert_eq!(supervisor.cached_output(WorkScope::Direct), Some(9));
        assert_eq!(supervisor.output_count(), 1);
    }

    #[test]
    fn source_cleanup_keeps_observations_from_other_sources() {
        let supervisor = Supervisor::<u8, u8, u8, u8>::new(SupervisorLimits {
            observation_queue_capacity: 2,
            ..limits()
        })
        .expect("valid limits");
        supervisor
            .try_enqueue_observation(
                WorkScope::Observation {
                    uses_screen: true,
                    uses_system_audio: false,
                },
                1,
            )
            .unwrap();
        supervisor
            .try_enqueue_observation(
                WorkScope::Observation {
                    uses_screen: false,
                    uses_system_audio: true,
                },
                2,
            )
            .unwrap();
        supervisor.clear_for_feature(Feature::Screen);
        assert_eq!(supervisor.pop_observation(), Some(2));
    }

    #[test]
    fn source_cleanup_removes_only_dependent_outputs() {
        let supervisor = Supervisor::<u8, u8, u8, u8>::new(limits()).expect("valid limits");
        let screen = WorkScope::Observation {
            uses_screen: true,
            uses_system_audio: false,
        };
        let audio = WorkScope::Observation {
            uses_screen: false,
            uses_system_audio: true,
        };
        supervisor.cache_output(screen, 1).unwrap();
        supervisor.cache_output(audio, 2).unwrap();
        supervisor.cache_output(WorkScope::Direct, 3).unwrap();
        supervisor.clear_for_feature(Feature::Screen);
        assert_eq!(supervisor.cached_output(screen), None);
        assert_eq!(supervisor.cached_output(audio), Some(2));
        assert_eq!(supervisor.cached_output(WorkScope::Direct), Some(3));
        supervisor.clear_for_feature(Feature::SystemAudio);
        assert_eq!(supervisor.cached_output(audio), None);
        assert_eq!(supervisor.cached_output(WorkScope::Direct), Some(3));
    }

    #[tokio::test]
    async fn feature_cancellation_removes_tasks_without_registry_growth() {
        let supervisor = Supervisor::<(), (), (), ()>::new(limits()).expect("valid limits");
        supervisor
            .spawn_scoped(WorkScope::ScreenCapture, |token| async move {
                token.cancelled().await;
            })
            .expect("spawn task");
        supervisor.clear_for_feature(Feature::Screen);
        assert_eq!(supervisor.task_count(), 0);
        let report = supervisor.shutdown(Duration::from_millis(100)).await;
        assert_eq!(
            report,
            ShutdownReport {
                completed: 0,
                aborted: 0
            }
        );
    }

    #[tokio::test]
    async fn completed_tasks_leave_the_active_registry() {
        let supervisor = Supervisor::<(), (), (), ()>::new(limits()).expect("valid limits");
        supervisor
            .spawn_scoped(WorkScope::Direct, |_| async {})
            .expect("spawn task");
        for _ in 0..10 {
            tokio::task::yield_now().await;
            if supervisor.task_count() == 0 {
                return;
            }
        }
        panic!("completed task remained in registry");
    }

    #[tokio::test]
    async fn direct_task_reservation_is_atomic_bounded_and_released() {
        let supervisor = Supervisor::<(), (), (), ()>::new(SupervisorLimits {
            direct_qwen_queue_capacity: 1,
            ..limits()
        })
        .expect("valid limits");
        let release = Arc::new(tokio::sync::Notify::new());
        let task_release = Arc::clone(&release);
        supervisor
            .try_spawn_direct(
                move |_| async move {
                    task_release.notified().await;
                },
                || {},
            )
            .expect("first direct task");
        assert_eq!(supervisor.queue_lengths().direct_queue, 1);
        assert_eq!(
            supervisor.try_spawn_direct(|_| async {}, || {}),
            Err(SupervisorError::QueueFull)
        );

        release.notify_one();
        for _ in 0..10 {
            tokio::task::yield_now().await;
            if supervisor.queue_lengths().direct_queue == 0 {
                break;
            }
        }
        assert_eq!(supervisor.queue_lengths().direct_queue, 0);
        supervisor
            .try_spawn_direct(|_| async {}, || {})
            .expect("released direct capacity");
    }

    #[tokio::test]
    async fn direct_cancellation_callback_runs_once_before_first_poll() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let supervisor = Supervisor::<(), (), (), ()>::new(limits()).expect("valid limits");
        let cancellations = Arc::new(AtomicUsize::new(0));
        let callback_cancellations = Arc::clone(&cancellations);
        supervisor
            .try_spawn_direct(
                |_| async { panic!("cancelled task must not be polled") },
                move || {
                    callback_cancellations.fetch_add(1, Ordering::AcqRel);
                },
            )
            .expect("direct task");

        supervisor.clear_all();
        supervisor.clear_all();

        assert_eq!(cancellations.load(Ordering::Acquire), 1);
        assert_eq!(supervisor.task_count(), 0);
    }

    #[tokio::test]
    async fn panicking_tasks_leave_the_active_registry() {
        let supervisor = Supervisor::<(), (), (), ()>::new(limits()).expect("valid limits");
        supervisor
            .spawn_scoped(WorkScope::Direct, |_| async {
                panic!("fake task failure");
            })
            .expect("spawn task");
        for _ in 0..10 {
            tokio::task::yield_now().await;
            if supervisor.task_count() == 0 {
                return;
            }
        }
        panic!("panicking task remained in registry");
    }

    #[tokio::test]
    async fn scoped_task_registry_has_a_hard_bound() {
        let supervisor = Supervisor::<(), (), (), ()>::new(SupervisorLimits {
            frame_ring_capacity: 1,
            observation_queue_capacity: 1,
            direct_qwen_queue_capacity: 1,
            system_audio_segment_capacity: 1,
            user_voice_queue_capacity: 1,
            system_audio_queue_capacity: 1,
        })
        .expect("valid limits");
        let capacity = supervisor.max_scoped_tasks();
        for _ in 0..capacity {
            supervisor
                .spawn_scoped(WorkScope::Direct, |_| async {
                    sleep(Duration::from_secs(5)).await;
                })
                .expect("within task bound");
        }
        assert_eq!(
            supervisor.spawn_scoped(WorkScope::Direct, |_| async {}),
            Err(SupervisorError::QueueFull)
        );
        supervisor.shutdown(Duration::ZERO).await;
    }

    #[tokio::test]
    async fn scoped_tasks_are_cancelled_and_shutdown_is_bounded_and_idempotent() {
        let supervisor =
            Arc::new(Supervisor::<i32, (), (), i32>::new(limits()).expect("valid limits"));
        let task_supervisor = Arc::clone(&supervisor);
        supervisor
            .spawn_scoped(WorkScope::Direct, move |token| async move {
                token.cancelled().await;
                task_supervisor.cache_output(WorkScope::Direct, 1).ok();
            })
            .expect("spawn task");
        let report = supervisor.shutdown(Duration::from_millis(100)).await;
        assert_eq!(report.aborted, 0);
        assert_eq!(supervisor.task_count(), 0);
        assert_eq!(
            supervisor.try_enqueue_direct(1),
            Err(SupervisorError::Closed)
        );
        let second = supervisor.shutdown(Duration::ZERO).await;
        assert_eq!(
            second,
            ShutdownReport {
                completed: 0,
                aborted: 0
            }
        );
    }

    #[tokio::test]
    async fn shutdown_aborts_a_task_that_ignores_cancellation() {
        let supervisor = Supervisor::<(), (), (), ()>::new(limits()).expect("valid limits");
        supervisor
            .spawn_scoped(WorkScope::Direct, |_| async {
                sleep(Duration::from_secs(5)).await;
            })
            .expect("spawn task");
        let report = supervisor.shutdown(Duration::from_millis(10)).await;
        assert_eq!(report.aborted, 1);
    }
}
