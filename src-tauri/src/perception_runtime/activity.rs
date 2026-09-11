//! Local scheduling policy. Qwen generates responses; all admission and pacing stay here.
use std::{collections::VecDeque, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{llama::Scene, screen::ChangeClassification};

const CACHE_CAPACITY: usize = 16;
const MODEL_FAILURE_BACKOFF_MAX_SECONDS: u64 = 40;
const DELIVERY_CONTEXT_TTL: Duration = Duration::from_secs(600);
const COURSE_DELIVERY_COOLDOWN: Duration = Duration::from_secs(90);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ActivityMode {
    #[default]
    Available,
    Transitioning,
    Focused,
    Media,
    Gaming,
    Quiet,
    Protected,
    Interacting,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivitySnapshot {
    pub epoch: u64,
    pub mode: ActivityMode,
    pub scene: Option<Scene>,
    pub input_idle_seconds: Option<u64>,
    pub regular_check_seconds: u64,
    pub model_calls: u64,
    pub suppressed_responses: u64,
    pub successful_checks: u64,
    pub silent_checks: u64,
    pub failed_checks: u64,
    pub model_busy: bool,
    pub last_completed_ago_seconds: Option<u64>,
    pub active_session: ActiveSession,
    pub course_confirmation_remaining_seconds: Option<u64>,
    pub course_completion_id: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ActiveSession {
    #[default]
    None,
    Course,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CourseConfirmationChoice {
    Continue,
    End,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EnvironmentSignal {
    // Ephemeral process identity; window titles never enter the policy cache.
    pub application_key: u64,
    pub input_idle_seconds: Option<u64>,
    pub protected: bool,
    pub own_application: bool,
}

struct ActivityHistory {
    key: u64,
    scene: Option<Scene>,
    stable_completions: u8,
    next_due: Duration,
}

pub struct ActivityTracker {
    snapshot: ActivitySnapshot,
    environment: EnvironmentSignal,
    environment_seen: bool,
    last_tick: Option<Duration>,
    changed_at: Duration,
    transition_started: Option<Duration>,
    application_transition: bool,
    last_motion: Duration,
    interaction_until: Duration,
    voice_active: bool,
    history: VecDeque<ActivityHistory>,
    failures: u8,
    retry_not_before: Duration,
    last_completed_at: Option<Duration>,
    delivered: VecDeque<(String, Duration, u64)>,
    last_visual_change: ChangeClassification,
    game_interval_seconds: u64,
    course_confirmation_deadline: Option<Duration>,
    course_confirmation_suppressed: bool,
    last_course_delivery_at: Option<Duration>,
}

impl Default for ActivityTracker {
    fn default() -> Self {
        Self {
            snapshot: ActivitySnapshot {
                regular_check_seconds: 10,
                ..Default::default()
            },
            environment: EnvironmentSignal::default(),
            environment_seen: false,
            last_tick: None,
            changed_at: Duration::ZERO,
            transition_started: None,
            application_transition: false,
            last_motion: Duration::ZERO,
            interaction_until: Duration::ZERO,
            voice_active: false,
            history: VecDeque::new(),
            failures: 0,
            retry_not_before: Duration::ZERO,
            last_completed_at: None,
            delivered: VecDeque::new(),
            last_visual_change: ChangeClassification::Stable,
            game_interval_seconds: 3,
            course_confirmation_deadline: None,
            course_confirmation_suppressed: false,
            last_course_delivery_at: None,
        }
    }
}

impl ActivityTracker {
    pub fn with_game_interval_seconds(game_interval_seconds: u64) -> Self {
        Self {
            game_interval_seconds,
            ..Self::default()
        }
    }

    pub fn snapshot(&self) -> ActivitySnapshot {
        self.snapshot.clone()
    }

    pub fn set_scene(&mut self, scene: Scene) {
        self.snapshot.scene = Some(scene);
    }

    pub fn resolve_scene(&mut self, scene: Scene, now: Duration) -> Scene {
        match (self.snapshot.active_session, &scene) {
            (ActiveSession::None, Scene::Course) => {
                self.snapshot.active_session = ActiveSession::Course;
                self.course_confirmation_deadline = None;
                self.course_confirmation_suppressed = false;
                self.last_course_delivery_at = None;
            }
            (ActiveSession::Course, Scene::Course) => {
                self.course_confirmation_deadline = None;
                self.course_confirmation_suppressed = false;
            }
            (ActiveSession::Course, _) if !self.course_confirmation_suppressed => {
                self.course_confirmation_deadline
                    .get_or_insert(now + Duration::from_secs(10));
            }
            _ => {}
        }
        self.refresh_course_confirmation(now);
        scene
    }

    pub fn choose_course_confirmation(
        &mut self,
        choice: CourseConfirmationChoice,
        now: Duration,
    ) -> bool {
        if self.snapshot.active_session != ActiveSession::Course
            || self.course_confirmation_deadline.is_none()
        {
            return false;
        }
        self.course_confirmation_deadline = None;
        match choice {
            CourseConfirmationChoice::Continue => {
                self.course_confirmation_suppressed = true;
            }
            CourseConfirmationChoice::End => self.end_course(),
        }
        self.refresh_course_confirmation(now);
        true
    }

    fn end_course(&mut self) {
        self.snapshot.active_session = ActiveSession::None;
        self.snapshot.course_completion_id = self.snapshot.course_completion_id.saturating_add(1);
        self.snapshot.course_confirmation_remaining_seconds = None;
        self.course_confirmation_deadline = None;
        self.course_confirmation_suppressed = false;
    }

    fn refresh_course_confirmation(&mut self, now: Duration) {
        if self
            .course_confirmation_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.end_course();
            return;
        }
        self.snapshot.course_confirmation_remaining_seconds = self
            .course_confirmation_deadline
            .map(|deadline| deadline.saturating_sub(now).as_secs().max(1));
    }

    pub fn recent_responses(&self, now: Duration) -> Vec<&str> {
        let epoch = self.snapshot.epoch;
        self.delivered
            .iter()
            .filter(|(_, at, response_epoch)| {
                *response_epoch == epoch
                    && now >= *at
                    && now.saturating_sub(*at) <= DELIVERY_CONTEXT_TTL
            })
            .map(|(text, _, _)| text.as_str())
            .collect()
    }

    pub fn environment(&mut self, signal: EnvironmentSignal, now: Duration) -> bool {
        let gap = self
            .last_tick
            .is_some_and(|last| now < last || now.saturating_sub(last) > Duration::from_secs(30));
        self.last_tick = Some(now);
        let application_switch = self.environment_seen
            && !self.environment.own_application
            && !signal.own_application
            && signal.application_key != self.environment.application_key;
        let boundary = gap
            || (self.environment_seen
                && (signal.protected != self.environment.protected
                    || signal.own_application != self.environment.own_application
                    || (!signal.own_application
                        && signal.application_key != self.environment.application_key)));
        if application_switch
            && self.snapshot.active_session == ActiveSession::Course
            && !self.course_confirmation_suppressed
        {
            self.course_confirmation_deadline
                .get_or_insert(now + Duration::from_secs(10));
        }
        if boundary {
            self.snapshot.epoch = self.snapshot.epoch.saturating_add(1);
            self.changed_at = now;
            self.last_motion = now;
            self.transition_started = Some(now);
            self.application_transition = true;
            self.snapshot.scene = self
                .history
                .iter()
                .find(|entry| entry.key == signal.application_key)
                .and_then(|entry| entry.scene.clone());
        }
        if !signal.own_application {
            self.environment.application_key = signal.application_key;
        }
        self.environment.protected = signal.protected;
        self.environment.own_application = signal.own_application;
        self.environment.input_idle_seconds = signal.input_idle_seconds;
        self.environment_seen = true;
        self.refresh(now);
        boundary
    }

    pub fn interact(&mut self, now: Duration, duration: Duration) {
        self.interaction_until = self.interaction_until.max(now + duration);
        self.snapshot.epoch = self.snapshot.epoch.saturating_add(1);
        self.refresh(now);
    }

    pub fn refresh(&mut self, now: Duration) {
        self.refresh_course_confirmation(now);
        self.snapshot.last_completed_ago_seconds = self
            .last_completed_at
            .map(|at| now.saturating_sub(at).as_secs());
        self.snapshot.input_idle_seconds = self.environment.input_idle_seconds;
        self.snapshot.mode = if self.environment.protected {
            ActivityMode::Protected
        } else if self.environment.own_application
            || self.voice_active
            || now < self.interaction_until
        {
            ActivityMode::Interacting
        } else if self.transition_started.is_some() {
            ActivityMode::Transitioning
        } else {
            match self.snapshot.scene {
                Some(Scene::Game) => ActivityMode::Gaming,
                Some(Scene::Course) => ActivityMode::Media,
                _ if self
                    .environment
                    .input_idle_seconds
                    .is_some_and(|idle| idle >= 900) =>
                {
                    ActivityMode::Quiet
                }
                Some(Scene::Other)
                    if now.saturating_sub(self.changed_at) >= Duration::from_secs(90) =>
                {
                    ActivityMode::Focused
                }
                _ => ActivityMode::Available,
            }
        };
    }

    pub fn set_voice_active(&mut self, active: bool, now: Duration) {
        self.voice_active = active;
        self.interact(now, Duration::from_secs(5));
    }

    pub fn blocked(&mut self, now: Duration) -> bool {
        self.refresh(now);
        matches!(
            self.snapshot.mode,
            ActivityMode::Protected | ActivityMode::Interacting
        )
    }

    pub fn visual_event(&mut self, change: ChangeClassification, now: Duration) -> bool {
        self.last_visual_change = change;
        if change == ChangeClassification::Transition {
            if self.transition_started.is_none() {
                self.transition_started = Some(now);
                self.application_transition = false;
                self.snapshot.epoch = self.snapshot.epoch.saturating_add(1);
            }
            self.last_motion = now;
        }
        let settled = self.transition_started.is_some_and(|start| {
            change == ChangeClassification::NewStableState
                || (change == ChangeClassification::Stable
                    && now.saturating_sub(self.last_motion) >= Duration::from_secs(3))
                || now.saturating_sub(start) >= Duration::from_secs(6)
        });
        if settled {
            self.transition_started = None;
        }
        let familiar_application = self.application_transition
            && self
                .history
                .iter()
                .any(|entry| entry.key == self.environment.application_key && now < entry.next_due);
        if settled {
            self.application_transition = false;
        }
        // A visual state change is evidence that the application may have changed
        // scenes even when this process has a cached regular deadline.  The cache
        // must only suppress routine polling; otherwise reopening a game (or
        // switching from a desktop menu to gameplay in the same process) can keep
        // the previous `Other` scene indefinitely.
        let significant_scene_change = change == ChangeClassification::NewStableState;
        if (settled || significant_scene_change)
            && (!familiar_application || significant_scene_change)
        {
            self.changed_at = now;
            if let Some(entry) = self
                .history
                .iter_mut()
                .find(|entry| entry.key == self.environment.application_key)
            {
                entry.stable_completions = 0;
            }
        }
        self.refresh(now);
        (settled || significant_scene_change) && (!familiar_application || significant_scene_change)
    }

    pub fn regular_delay(&self, now: Duration) -> Duration {
        self.history
            .iter()
            .find(|entry| entry.key == self.environment.application_key)
            .map_or(Duration::from_secs(10), |entry| {
                entry
                    .next_due
                    .saturating_sub(now)
                    .max(Duration::from_secs(5))
            })
    }

    pub fn active_check_limit(&self) -> Option<Duration> {
        self.environment
            .input_idle_seconds
            .filter(|idle| *idle < 60)
            .map(|_| Duration::from_secs(30))
    }

    pub fn is_transitioning(&self) -> bool {
        self.transition_started.is_some()
    }

    pub fn allow_early_event(&self) -> bool {
        self.failures == 0
    }

    pub fn retry_ready(&self, now: Duration) -> bool {
        now >= self.retry_not_before
    }

    pub fn started(&mut self) {
        self.snapshot.model_calls = self.snapshot.model_calls.saturating_add(1);
    }

    pub fn completed(&mut self, scene: Option<Scene>, silent: bool, now: Duration) -> Duration {
        if let Some(scene) = scene {
            self.last_completed_at = Some(now);
            self.snapshot.successful_checks = self.snapshot.successful_checks.saturating_add(1);
            if silent {
                self.snapshot.silent_checks = self.snapshot.silent_checks.saturating_add(1);
            }
            self.failures = 0;
            self.retry_not_before = Duration::ZERO;
            let key = self.environment.application_key;
            let index = self.history.iter().position(|entry| entry.key == key);
            let mut entry = index
                .and_then(|i| self.history.remove(i))
                .unwrap_or(ActivityHistory {
                    key,
                    scene: None,
                    stable_completions: 0,
                    next_due: Duration::ZERO,
                });
            entry.stable_completions = if entry.scene.is_some() {
                entry.stable_completions.saturating_add(1).min(5)
            } else {
                0
            };
            entry.scene = Some(scene.clone());
            self.snapshot.scene = Some(scene.clone());
            self.snapshot.regular_check_seconds = match scene {
                Scene::Game => self.game_interval_seconds,
                Scene::Course | Scene::Other => {
                    let cap = if self.last_visual_change == ChangeClassification::Stable {
                        120
                    } else {
                        match scene {
                            Scene::Course => 45,
                            Scene::Other => {
                                // Keep sampling ordinary desktop activity frequently enough
                                // to confirm a newly opened course, then back off gradually.
                                let progressive_cap = 15_u64
                                    .saturating_mul(1 + u64::from(entry.stable_completions))
                                    .min(90);
                                self.active_check_limit().map_or(progressive_cap, |limit| {
                                    limit.as_secs().min(progressive_cap)
                                })
                            }
                            Scene::Game => unreachable!("game uses its configured interval"),
                        }
                    };
                    (30 * (1 + u64::from(entry.stable_completions))).min(cap)
                }
            };
            entry.next_due = now + Duration::from_secs(self.snapshot.regular_check_seconds);
            self.history.push_back(entry);
            if self.history.len() > CACHE_CAPACITY {
                self.history.pop_front();
            }
        } else {
            self.snapshot.failed_checks = self.snapshot.failed_checks.saturating_add(1);
            self.failures = self.failures.saturating_add(1).min(2);
            self.snapshot.regular_check_seconds =
                (10_u64 << self.failures).min(MODEL_FAILURE_BACKOFF_MAX_SECONDS);
            self.retry_not_before = now + Duration::from_secs(self.snapshot.regular_check_seconds);
        }
        self.refresh(now);
        Duration::from_secs(self.snapshot.regular_check_seconds)
    }

    /// Check whether a generated response may be shown in the current state.
    /// This does not retain the message; callers that commit asynchronously
    /// should record it only after the commit succeeds.
    pub fn can_deliver(&mut self, epoch: u64, now: Duration) -> bool {
        let course_cooldown = self.snapshot.active_session == ActiveSession::Course
            && self.last_course_delivery_at.is_some_and(|last| {
                now < last || now.saturating_sub(last) < COURSE_DELIVERY_COOLDOWN
            });
        if epoch != self.snapshot.epoch
            || self.blocked(now)
            || self.is_transitioning()
            || course_cooldown
        {
            self.snapshot.suppressed_responses =
                self.snapshot.suppressed_responses.saturating_add(1);
            return false;
        }
        true
    }

    /// Check and retain a generated response for callers whose delivery is
    /// synchronous. Observation work uses [`Self::can_deliver`] followed by
    /// [`Self::record_delivery`] so a rejected runtime commit cannot pollute
    /// future de-duplication context.
    pub fn allow_delivery(&mut self, epoch: u64, message: &str, now: Duration) -> bool {
        if !self.can_deliver(epoch, now) {
            return false;
        }
        self.record_delivery(epoch, message, now);
        true
    }

    /// Add a response to the context supplied to the next observation.
    /// Call this only after the corresponding event has been committed to the
    /// sink; stale or rejected work must not influence future de-duplication.
    pub fn record_delivery(&mut self, epoch: u64, message: &str, now: Duration) {
        if epoch != self.snapshot.epoch || message.trim().is_empty() {
            return;
        }
        self.delivered
            .push_back((message.chars().take(2000).collect(), now, epoch));
        if self.delivered.len() > 5 {
            self.delivered.pop_front();
        }
        if self.snapshot.active_session == ActiveSession::Course {
            self.last_course_delivery_at = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    #[test]
    fn last_five_replies_survive_long_quiet_intervals_in_chronological_order() {
        let mut tracker = ActivityTracker::default();
        for index in 0..8 {
            let message = format!("reply {index}");
            let now = at(index * 100);
            assert!(tracker.allow_delivery(0, &message, now));
        }
        assert_eq!(
            tracker.recent_responses(at(800)),
            vec!["reply 3", "reply 4", "reply 5", "reply 6", "reply 7"]
        );
    }

    #[test]
    fn keyboard_activity_does_not_override_stable_screen_backoff() {
        let mut tracker = ActivityTracker::default();
        tracker.environment(
            EnvironmentSignal {
                application_key: 1,
                input_idle_seconds: Some(0),
                ..Default::default()
            },
            at(0),
        );
        for second in 0..8 {
            tracker.completed(Some(Scene::Other), true, at(second));
        }
        assert_eq!(tracker.snapshot().regular_check_seconds, 120);
        tracker.environment(
            EnvironmentSignal {
                application_key: 2,
                ..Default::default()
            },
            at(9),
        );
        tracker.environment(
            EnvironmentSignal {
                application_key: 1,
                input_idle_seconds: Some(120),
                ..Default::default()
            },
            at(10),
        );
        assert_eq!(tracker.snapshot().scene, Some(Scene::Other));
        tracker.completed(Some(Scene::Other), true, at(11));
        assert_eq!(tracker.snapshot().regular_check_seconds, 120);
    }

    #[test]
    fn diagnostic_counts_distinguish_silence_failure_and_success() {
        let mut tracker = ActivityTracker::default();
        tracker.started();
        tracker.completed(Some(Scene::Other), true, at(2));
        tracker.started();
        tracker.completed(None, true, at(4));
        tracker.refresh(at(7));
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.model_calls, 2);
        assert_eq!(snapshot.successful_checks, 1);
        assert_eq!(snapshot.silent_checks, 1);
        assert_eq!(snapshot.failed_checks, 1);
        assert_eq!(snapshot.last_completed_ago_seconds, Some(5));
        tracker.started();
        tracker.completed(Some(Scene::Other), false, at(8));
        assert_eq!(tracker.snapshot().successful_checks, 2);
        assert_eq!(tracker.snapshot().silent_checks, 1);
    }

    #[test]
    fn stable_work_backs_off_but_games_use_their_configured_interval() {
        let mut activity = ActivityTracker::default();
        for n in 0..10 {
            activity.completed(Some(Scene::Other), true, at(n));
        }
        assert_eq!(activity.snapshot().regular_check_seconds, 120);
        activity.visual_event(ChangeClassification::ContinuousMotion, at(10));
        for n in 10..20 {
            activity.completed(Some(Scene::Game), true, at(n));
        }
        assert_eq!(activity.snapshot().regular_check_seconds, 3);
    }

    #[test]
    fn first_game_completion_uses_the_configured_game_interval() {
        let mut activity = ActivityTracker::with_game_interval_seconds(3);

        assert_eq!(
            activity.completed(Some(Scene::Game), true, at(0)),
            Duration::from_secs(3)
        );
        assert_eq!(activity.snapshot().regular_check_seconds, 3);
    }

    #[test]
    fn inactivity_is_not_absence_and_privacy_wins() {
        let mut activity = ActivityTracker::default();
        activity.environment(
            EnvironmentSignal {
                input_idle_seconds: Some(3600),
                ..Default::default()
            },
            at(0),
        );
        assert_eq!(activity.snapshot().mode, ActivityMode::Quiet);
        assert!(!activity.blocked(at(0)));
        activity.environment(
            EnvironmentSignal {
                protected: true,
                input_idle_seconds: Some(3600),
                ..Default::default()
            },
            at(1),
        );
        assert!(activity.blocked(at(1)));
        assert_eq!(activity.snapshot().mode, ActivityMode::Protected);
    }

    #[test]
    fn transition_waits_for_settlement_and_has_a_deadline() {
        let mut activity = ActivityTracker::default();
        assert!(!activity.visual_event(ChangeClassification::Transition, at(0)));
        assert!(!activity.visual_event(ChangeClassification::Transition, at(3)));
        assert!(activity.visual_event(ChangeClassification::ContinuousMotion, at(6)));
        assert!(!activity.is_transitioning());
    }

    #[test]
    fn fresh_responses_are_delivered_and_stale_results_do_not_escape() {
        let mut activity = ActivityTracker::default();
        activity.environment(
            EnvironmentSignal {
                application_key: 1,
                ..Default::default()
            },
            at(0),
        );
        let epoch = activity.snapshot().epoch;
        assert!(activity.allow_delivery(epoch, "hello", at(0)));
        assert!(activity.allow_delivery(epoch, " hello ", at(60)));
        activity.environment(
            EnvironmentSignal {
                application_key: 2,
                ..Default::default()
            },
            at(61),
        );
        activity.environment(
            EnvironmentSignal {
                application_key: 1,
                ..Default::default()
            },
            at(62),
        );
        assert!(!activity.allow_delivery(epoch, "late", at(80)));
    }

    #[test]
    fn delivery_context_requires_explicit_recording_and_current_epoch() {
        let mut activity = ActivityTracker::default();
        let epoch = activity.snapshot().epoch;

        assert!(activity.can_deliver(epoch, at(0)));
        assert!(activity.recent_responses(at(0)).is_empty());

        activity.record_delivery(epoch, "committed", at(1));
        assert_eq!(activity.recent_responses(at(1)), vec!["committed"]);

        // Interaction advances the context generation. A late record from the
        // previous generation must not leak into the next observation.
        activity.interact(at(2), Duration::ZERO);
        assert!(activity.recent_responses(at(2)).is_empty());
        activity.record_delivery(epoch, "late", at(2));
        assert!(activity.recent_responses(at(2)).is_empty());
    }

    #[test]
    fn delivery_context_expires_at_ttl_and_rejects_future_timestamps() {
        let mut activity = ActivityTracker::default();
        let epoch = activity.snapshot().epoch;
        activity.record_delivery(epoch, "fresh", at(100));

        assert!(activity.recent_responses(at(99)).is_empty());
        assert_eq!(activity.recent_responses(at(700)), vec!["fresh"]);
        assert!(activity.recent_responses(at(701)).is_empty());
    }

    #[test]
    fn sleep_gap_invalidates_results_and_failures_back_off() {
        let mut activity = ActivityTracker::default();
        activity.environment(EnvironmentSignal::default(), at(0));
        assert!(activity.environment(EnvironmentSignal::default(), at(300)));
        assert_eq!(activity.snapshot().epoch, 1);
        for n in 0..20 {
            activity.completed(None, true, at(n));
        }
        assert_eq!(activity.snapshot().regular_check_seconds, 40);
        assert!(activity.history.len() <= CACHE_CAPACITY);
    }

    #[test]
    fn model_failure_backoff_is_capped_at_forty_seconds() {
        let mut activity = ActivityTracker::default();
        assert_eq!(activity.completed(None, true, at(0)), at(20));
        assert_eq!(activity.completed(None, true, at(20)), at(40));
        assert_eq!(activity.completed(None, true, at(60)), at(40));
        assert_eq!(activity.completed(None, true, at(100)), at(40));
    }

    #[test]
    fn application_switches_do_not_bypass_failure_retry_deadline() {
        let mut activity = ActivityTracker::default();
        activity.environment(EnvironmentSignal::default(), at(0));
        assert_eq!(activity.completed(None, true, at(1)), at(20));
        activity.environment(
            EnvironmentSignal {
                application_key: 2,
                ..Default::default()
            },
            at(2),
        );
        activity.visual_event(ChangeClassification::NewStableState, at(3));
        assert!(!activity.allow_early_event());
        assert!(!activity.retry_ready(at(20)));
        assert!(activity.retry_ready(at(21)));
        assert_eq!(activity.completed(None, true, at(21)), at(40));
        activity.environment(EnvironmentSignal::default(), at(22));
        assert!(!activity.retry_ready(at(60)));
        assert!(activity.retry_ready(at(61)));
        activity.completed(Some(Scene::Other), true, at(61));
        assert!(activity.allow_early_event());
        assert!(activity.retry_ready(at(61)));
    }

    #[test]
    fn revisiting_a_familiar_application_rechecks_a_new_stable_state() {
        let mut activity = ActivityTracker::default();
        activity.environment(
            EnvironmentSignal {
                application_key: 1,
                ..Default::default()
            },
            at(0),
        );
        for n in 0..8 {
            activity.completed(Some(Scene::Other), true, at(n));
        }
        activity.environment(
            EnvironmentSignal {
                application_key: 2,
                ..Default::default()
            },
            at(8),
        );
        activity.visual_event(ChangeClassification::NewStableState, at(9));
        activity.completed(Some(Scene::Other), true, at(9));
        activity.environment(
            EnvironmentSignal {
                application_key: 1,
                ..Default::default()
            },
            at(10),
        );
        assert!(activity.visual_event(ChangeClassification::NewStableState, at(11)));
        assert!(activity.regular_delay(at(11)) > at(60));
        assert!(activity.visual_event(ChangeClassification::NewStableState, at(12)));
        assert!(activity.visual_event(ChangeClassification::NewStableState, at(12)));
    }

    #[test]
    fn long_run_keeps_history_and_delivery_cache_bounded() {
        let mut activity = ActivityTracker::default();
        for n in 0..10_000 {
            let now = at(n * 20);
            activity.environment(
                EnvironmentSignal {
                    application_key: n,
                    ..Default::default()
                },
                now,
            );
            activity.visual_event(ChangeClassification::NewStableState, now);
            activity.completed(Some(Scene::Other), true, now);
            let message = format!("event {n}");
            let epoch = activity.snapshot().epoch;
            assert!(activity.allow_delivery(epoch, &message, now));
            assert!(activity.history.len() <= CACHE_CAPACITY);
            assert!(activity.delivered.len() <= CACHE_CAPACITY);
        }
    }

    #[test]
    fn scene_label_jitter_does_not_reset_stable_backoff_and_new_visual_events_wake() {
        let mut activity = ActivityTracker::default();
        for n in 0..8 {
            activity.visual_event(ChangeClassification::Stable, at(n));
            activity.completed(Some(Scene::Other), false, at(n));
        }
        assert_eq!(activity.snapshot().regular_check_seconds, 120);
        assert!(activity.visual_event(ChangeClassification::NewStableState, at(10)));
        assert_eq!(
            activity.completed(Some(Scene::Course), false, at(10)),
            at(45)
        );
    }

    #[test]
    fn scene_results_are_applied_without_confirmation_delay() {
        let mut activity = ActivityTracker::default();
        activity.set_scene(Scene::Other);
        assert_eq!(activity.resolve_scene(Scene::Game, at(0)), Scene::Game);
        activity.set_scene(Scene::Game);
        assert_eq!(activity.resolve_scene(Scene::Course, at(1)), Scene::Course);
        activity.set_scene(Scene::Course);
        assert_eq!(activity.resolve_scene(Scene::Other, at(2)), Scene::Other);
    }

    #[test]
    fn leaving_a_course_asks_before_ending_and_times_out_after_ten_seconds() {
        let mut activity = ActivityTracker::default();
        activity.resolve_scene(Scene::Course, at(1));

        activity.resolve_scene(Scene::Other, at(2));
        assert_eq!(activity.snapshot().active_session, ActiveSession::Course);
        assert_eq!(
            activity.snapshot().course_confirmation_remaining_seconds,
            Some(10)
        );

        activity.refresh(at(11));
        assert_eq!(activity.snapshot().active_session, ActiveSession::Course);
        activity.refresh(at(12));
        assert_eq!(activity.snapshot().active_session, ActiveSession::None);
        assert_eq!(activity.snapshot().course_completion_id, 1);
    }

    #[test]
    fn switching_external_applications_asks_immediately_during_a_course() {
        let mut activity = ActivityTracker::default();
        activity.environment(
            EnvironmentSignal {
                application_key: 1,
                ..Default::default()
            },
            at(0),
        );
        activity.resolve_scene(Scene::Course, at(1));

        activity.environment(
            EnvironmentSignal {
                application_key: 2,
                ..Default::default()
            },
            at(2),
        );

        assert_eq!(activity.snapshot().active_session, ActiveSession::Course);
        assert_eq!(
            activity.snapshot().course_confirmation_remaining_seconds,
            Some(10)
        );
    }

    #[test]
    fn user_choice_keeps_course_active_across_non_course_observations() {
        let mut activity = ActivityTracker::default();
        activity.resolve_scene(Scene::Course, at(1));
        activity.resolve_scene(Scene::Other, at(2));

        assert!(activity.choose_course_confirmation(CourseConfirmationChoice::Continue, at(3)));
        activity.resolve_scene(Scene::Other, at(30));
        assert_eq!(activity.snapshot().active_session, ActiveSession::Course);
        assert_eq!(
            activity.snapshot().course_confirmation_remaining_seconds,
            None
        );

        activity.resolve_scene(Scene::Course, at(31));
        activity.resolve_scene(Scene::Other, at(32));
        assert_eq!(
            activity.snapshot().course_confirmation_remaining_seconds,
            Some(10)
        );
    }

    #[test]
    fn user_can_end_course_from_confirmation() {
        let mut activity = ActivityTracker::default();
        activity.resolve_scene(Scene::Course, at(1));
        activity.resolve_scene(Scene::Other, at(2));

        assert!(activity.choose_course_confirmation(CourseConfirmationChoice::End, at(3)));
        assert_eq!(activity.snapshot().active_session, ActiveSession::None);
        assert_eq!(activity.snapshot().course_completion_id, 1);
    }

    #[test]
    fn course_responses_have_a_ninety_second_delivery_cooldown() {
        let mut activity = ActivityTracker::default();
        activity.resolve_scene(Scene::Course, at(1));
        activity.record_delivery(0, "这是一个重要考点。", at(2));

        assert!(!activity.can_deliver(0, at(91)));
        assert!(activity.can_deliver(0, at(92)));
    }

    #[test]
    fn polling_during_transition_does_not_consume_the_visual_wakeup() {
        let mut activity = ActivityTracker::default();
        activity.visual_event(ChangeClassification::Transition, at(0));
        activity.refresh(at(6));
        assert!(activity.is_transitioning());
        assert!(activity.visual_event(ChangeClassification::ContinuousMotion, at(6)));
    }

    #[test]
    fn new_stable_visual_state_rechecks_cached_application_scene() {
        let mut activity = ActivityTracker::default();
        let key = 42;
        activity.environment(
            EnvironmentSignal {
                application_key: key,
                ..Default::default()
            },
            at(0),
        );
        activity.set_scene(Scene::Other);
        activity.completed(Some(Scene::Other), true, at(0));
        // A cached application is within its regular backoff window.
        activity.environment(
            EnvironmentSignal {
                application_key: key,
                ..Default::default()
            },
            at(1),
        );
        assert!(activity.visual_event(ChangeClassification::NewStableState, at(2)));
    }
}
