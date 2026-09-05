use std::time::{Duration, Instant};

use crate::screen::{ChangeClassification, ScreenRuntimeConfig};

pub trait Clock {
    fn now(&self) -> Duration;
}

pub struct MonotonicClock {
    started_at: Instant,
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
        }
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> Duration {
        self.started_at.elapsed()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationCandidate<T = ()> {
    pub latest_frame_sequence: u64,
    pub classification: ChangeClassification,
    pub payload: T,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationReason {
    Scheduled,
    SignificantEvent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationDispatch<T = ()> {
    pub candidate: ObservationCandidate<T>,
    pub reason: ObservationReason,
}

pub struct ObservationScheduler<C, T = ()> {
    clock: C,
    minimum_interval: Duration,
    next_check_min: Duration,
    next_check_max: Duration,
    next_check_default: Duration,
    next_due: Duration,
    last_started: Option<Duration>,
    busy: bool,
    active_sequence: Option<u64>,
    pending: Option<ObservationCandidate<T>>,
}

impl<C: Clock, T> ObservationScheduler<C, T> {
    pub fn new(clock: C, config: ScreenRuntimeConfig) -> Option<Self> {
        if !config.is_valid() {
            return None;
        }
        let now = clock.now();
        Some(Self {
            clock,
            minimum_interval: Duration::from_secs(config.observation_min_interval_seconds),
            next_check_min: Duration::from_secs(config.next_check_min_seconds),
            next_check_max: Duration::from_secs(config.next_check_max_seconds),
            next_check_default: Duration::from_secs(config.next_check_default_seconds),
            next_due: now + Duration::from_secs(config.next_check_default_seconds),
            last_started: None,
            busy: false,
            active_sequence: None,
            pending: None,
        })
    }

    pub fn observe(
        &mut self,
        candidate: ObservationCandidate<T>,
    ) -> Option<ObservationDispatch<T>> {
        let now = self.clock.now();
        let significant = candidate.classification.is_significant();
        let due = now >= self.next_due;
        if significant || due {
            self.pending = Some(candidate);
        }
        if self.busy || !self.minimum_elapsed(now) {
            return None;
        }
        let pending = self.pending.take()?;
        let reason = if pending.classification.is_significant() {
            ObservationReason::SignificantEvent
        } else {
            ObservationReason::Scheduled
        };
        Some(self.start(pending, reason, now))
    }

    pub fn complete(
        &mut self,
        active_sequence: u64,
        next_check_seconds: Option<u64>,
    ) -> Option<ObservationDispatch<T>> {
        if self.active_sequence != Some(active_sequence) {
            return None;
        }
        let now = self.clock.now();
        self.busy = false;
        self.active_sequence = None;
        let requested =
            Duration::from_secs(next_check_seconds.unwrap_or(self.next_check_default.as_secs()));
        let bounded = requested.clamp(self.next_check_min, self.next_check_max);
        self.next_due = now + bounded;
        if !self.minimum_elapsed(now) {
            return None;
        }
        let candidate = self.pending.take()?;
        let reason = if candidate.classification.is_significant() {
            ObservationReason::SignificantEvent
        } else {
            ObservationReason::Scheduled
        };
        Some(self.start(candidate, reason, now))
    }

    pub fn cancel(&mut self) {
        self.busy = false;
        self.active_sequence = None;
        self.pending = None;
        self.next_due = self.clock.now() + self.next_check_default;
    }

    pub fn cancel_dispatch(&mut self, active_sequence: u64) {
        if self.active_sequence == Some(active_sequence) {
            self.cancel();
        }
    }

    pub fn clear_pending_if(&mut self, predicate: impl FnOnce(&T) -> bool) {
        if self
            .pending
            .as_ref()
            .is_some_and(|candidate| predicate(&candidate.payload))
        {
            self.pending = None;
        }
    }

    pub const fn is_busy(&self) -> bool {
        self.busy
    }

    pub fn pending_len(&self) -> usize {
        usize::from(self.pending.is_some())
    }

    fn minimum_elapsed(&self, now: Duration) -> bool {
        self.last_started.map_or(true, |last| {
            now.saturating_sub(last) >= self.minimum_interval
        })
    }

    fn start(
        &mut self,
        candidate: ObservationCandidate<T>,
        reason: ObservationReason,
        now: Duration,
    ) -> ObservationDispatch<T> {
        self.busy = true;
        self.active_sequence = Some(candidate.latest_frame_sequence);
        self.last_started = Some(now);
        self.pending = None;
        ObservationDispatch { candidate, reason }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct FakeClock(Arc<Mutex<Duration>>);

    impl FakeClock {
        fn advance(&self, duration: Duration) {
            let mut now = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *now += duration;
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    fn config() -> ScreenRuntimeConfig {
        ScreenRuntimeConfig {
            capture_interval_seconds: 3,
            frame_ring_capacity: 8,
            request_images_min: 5,
            request_images_max: 8,
            observation_min_interval_seconds: 5,
            next_check_min_seconds: 5,
            next_check_max_seconds: 18,
            next_check_default_seconds: 10,
            game_interval_seconds: 3,
            media_interval_seconds: 6,
            default_interval_seconds: 10,
            pending_observation_capacity: 1,
        }
    }

    fn candidate(sequence: u64, classification: ChangeClassification) -> ObservationCandidate {
        ObservationCandidate {
            latest_frame_sequence: sequence,
            classification,
            payload: (),
        }
    }

    #[test]
    fn scheduled_checks_and_significant_events_respect_the_five_second_floor() {
        let clock = FakeClock::default();
        let mut scheduler = ObservationScheduler::new(clock.clone(), config()).expect("scheduler");
        assert_eq!(
            scheduler.observe(candidate(1, ChangeClassification::Stable)),
            None
        );
        clock.advance(Duration::from_secs(10));
        assert_eq!(
            scheduler
                .observe(candidate(2, ChangeClassification::Stable))
                .expect("due")
                .reason,
            ObservationReason::Scheduled
        );
        scheduler.complete(2, Some(18));
        clock.advance(Duration::from_secs(1));
        assert_eq!(
            scheduler.observe(candidate(3, ChangeClassification::Transition)),
            None
        );
        assert_eq!(scheduler.pending_len(), 1);
        clock.advance(Duration::from_secs(4));
        assert_eq!(
            scheduler
                .observe(candidate(4, ChangeClassification::Stable))
                .expect("coalesced event")
                .candidate
                .latest_frame_sequence,
            3
        );
    }

    #[test]
    fn busy_scheduler_keeps_only_the_latest_candidate() {
        let clock = FakeClock::default();
        let mut scheduler = ObservationScheduler::new(clock.clone(), config()).expect("scheduler");
        clock.advance(Duration::from_secs(10));
        scheduler
            .observe(candidate(1, ChangeClassification::Stable))
            .expect("initial");
        for sequence in 2..=100 {
            scheduler.observe(candidate(sequence, ChangeClassification::NewStableState));
            assert_eq!(scheduler.pending_len(), 1);
        }
        clock.advance(Duration::from_secs(5));
        let dispatch = scheduler.complete(1, Some(5)).expect("latest pending");
        assert_eq!(dispatch.candidate.latest_frame_sequence, 100);
        assert!(scheduler.is_busy());
    }

    #[test]
    fn continuous_motion_does_not_create_early_calls_or_an_unbounded_queue() {
        let clock = FakeClock::default();
        let mut scheduler = ObservationScheduler::new(clock.clone(), config()).expect("scheduler");
        let mut calls = 0;
        for sequence in 1..=10_000 {
            clock.advance(Duration::from_secs(1));
            if scheduler
                .observe(candidate(sequence, ChangeClassification::ContinuousMotion))
                .is_some()
            {
                calls += 1;
                scheduler.complete(sequence, Some(10));
            }
            assert!(scheduler.pending_len() <= 1);
        }
        assert_eq!(calls, 1_000);
    }

    #[test]
    fn model_interval_is_clamped_and_cancel_drops_pending_work() {
        let clock = FakeClock::default();
        let mut scheduler = ObservationScheduler::new(clock.clone(), config()).expect("scheduler");
        clock.advance(Duration::from_secs(10));
        scheduler
            .observe(candidate(1, ChangeClassification::Stable))
            .expect("initial");
        scheduler.observe(candidate(2, ChangeClassification::Transition));
        scheduler.cancel();
        assert!(!scheduler.is_busy());
        assert_eq!(scheduler.pending_len(), 0);

        clock.advance(Duration::from_secs(8));
        assert!(scheduler
            .observe(candidate(3, ChangeClassification::Stable))
            .is_none());
        clock.advance(Duration::from_secs(10));
        assert!(scheduler
            .observe(candidate(4, ChangeClassification::Stable))
            .is_some());
        scheduler.complete(4, Some(1));
        clock.advance(Duration::from_secs(5));
        assert!(scheduler
            .observe(candidate(5, ChangeClassification::Stable))
            .is_some());
    }

    #[test]
    fn stale_completion_cannot_mutate_a_new_dispatch() {
        let clock = FakeClock::default();
        let mut scheduler = ObservationScheduler::new(clock.clone(), config()).expect("scheduler");
        let first = scheduler
            .observe(candidate(1, ChangeClassification::Transition))
            .expect("first");
        scheduler.cancel();
        clock.advance(Duration::from_secs(5));
        let second = scheduler
            .observe(candidate(2, ChangeClassification::Transition))
            .expect("second");

        assert_eq!(
            scheduler.complete(first.candidate.latest_frame_sequence, Some(5)),
            None
        );
        assert!(scheduler.is_busy());
        assert_eq!(
            scheduler.complete(second.candidate.latest_frame_sequence, Some(5)),
            None
        );
        assert!(!scheduler.is_busy());
    }
}
