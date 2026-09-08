//! Local activity policy inspired by N.E.K.O's rule/LLM separation. No I/O or wall clock.
use std::{collections::VecDeque, time::Duration};

use serde::Serialize;

use crate::{llama::Scene, screen::ChangeClassification};

const CACHE_CAPACITY: usize = 16;

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
    quiet_completions: u8,
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
    last_delivery: Option<Duration>,
    last_completed_at: Option<Duration>,
    delivered: VecDeque<(String, Duration)>,
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
            last_delivery: None,
            last_completed_at: None,
            delivered: VecDeque::new(),
        }
    }
}

impl ActivityTracker {
    pub fn snapshot(&self) -> ActivitySnapshot {
        self.snapshot.clone()
    }

    pub fn set_scene(&mut self, scene: Scene) {
        self.snapshot.scene = Some(scene);
    }

    pub fn recent_responses(&self, now: Duration) -> Vec<&str> {
        self.delivered
            .iter()
            .filter(|(_, at)| now.saturating_sub(*at) < Duration::from_secs(300))
            .map(|(text, _)| text.as_str())
            .collect()
    }

    pub fn environment(&mut self, signal: EnvironmentSignal, now: Duration) -> bool {
        let gap = self
            .last_tick
            .is_some_and(|last| now < last || now.saturating_sub(last) > Duration::from_secs(30));
        self.last_tick = Some(now);
        let boundary = gap
            || (self.environment_seen
                && (signal.protected != self.environment.protected
                    || signal.own_application != self.environment.own_application
                    || (!signal.own_application
                        && signal.application_key != self.environment.application_key)));
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
        self.snapshot.last_completed_ago_seconds = self
            .last_completed_at
            .map(|at| now.saturating_sub(at).as_secs());
        if self
            .transition_started
            .is_some_and(|start| now.saturating_sub(start) >= Duration::from_secs(15))
        {
            self.transition_started = None;
        }
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
                Some(Scene::Video | Scene::Course | Scene::Meeting) => ActivityMode::Media,
                _ if self
                    .environment
                    .input_idle_seconds
                    .is_some_and(|idle| idle >= 900) =>
                {
                    ActivityMode::Quiet
                }
                Some(Scene::Coding | Scene::Reading | Scene::Work)
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
                    && now.saturating_sub(self.last_motion) >= Duration::from_secs(6))
                || now.saturating_sub(start) >= Duration::from_secs(15)
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
        self.refresh(now);
        (settled || change == ChangeClassification::NewStableState) && !familiar_application
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
            .map(|_| Duration::from_secs(45))
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

    pub fn completed(
        &mut self,
        scene: Option<Scene>,
        silent: bool,
        model_seconds: u64,
        now: Duration,
    ) -> Duration {
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
                    quiet_completions: 0,
                    next_due: Duration::ZERO,
                });
            entry.quiet_completions = if silent && entry.scene.as_ref() == Some(&scene) {
                entry.quiet_completions.saturating_add(1).min(4)
            } else {
                0
            };
            entry.scene = Some(scene.clone());
            self.snapshot.scene = Some(scene.clone());
            let cap = match scene {
                Scene::Game => 20,
                Scene::Video | Scene::Course | Scene::Meeting => 60,
                Scene::Coding | Scene::Reading | Scene::Work => self
                    .active_check_limit()
                    .map_or(180, |limit| limit.as_secs()),
                _ => 90,
            };
            self.snapshot.regular_check_seconds = model_seconds
                .clamp(5, 18)
                .saturating_mul(1 << entry.quiet_completions)
                .min(cap);
            entry.next_due = now + Duration::from_secs(self.snapshot.regular_check_seconds);
            self.history.push_back(entry);
            if self.history.len() > CACHE_CAPACITY {
                self.history.pop_front();
            }
        } else {
            self.snapshot.failed_checks = self.snapshot.failed_checks.saturating_add(1);
            self.failures = self.failures.saturating_add(1).min(4);
            self.snapshot.regular_check_seconds = (10_u64 << self.failures).min(180);
            self.retry_not_before = now + Duration::from_secs(self.snapshot.regular_check_seconds);
        }
        self.refresh(now);
        Duration::from_secs(self.snapshot.regular_check_seconds)
    }

    pub fn allow_delivery(&mut self, epoch: u64, message: &str, now: Duration) -> bool {
        let normalized: String = message
            .chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_lowercase)
            .take(2000)
            .collect();
        self.delivered
            .retain(|(_, at)| now.saturating_sub(*at) < Duration::from_secs(300));
        let cooldown = match self.snapshot.scene {
            Some(Scene::Game) => 20,
            Some(Scene::Video | Scene::Course | Scene::Meeting) => 60,
            Some(Scene::Coding | Scene::Reading | Scene::Work) => 120,
            _ => 45,
        };
        if epoch != self.snapshot.epoch
            || self.blocked(now)
            || self.is_transitioning()
            || self
                .last_delivery
                .is_some_and(|at| now.saturating_sub(at) < Duration::from_secs(cooldown))
            || self.delivered.iter().any(|(text, _)| text == &normalized)
        {
            self.snapshot.suppressed_responses =
                self.snapshot.suppressed_responses.saturating_add(1);
            return false;
        }
        self.last_delivery = Some(now);
        self.delivered.push_back((normalized, now));
        if self.delivered.len() > CACHE_CAPACITY {
            self.delivered.pop_front();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    #[test]
    fn active_work_is_checked_within_45_seconds_and_cached_scene_is_restored() {
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
            tracker.completed(Some(Scene::Coding), true, 18, at(second));
        }
        assert_eq!(tracker.snapshot().regular_check_seconds, 45);
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
        assert_eq!(tracker.snapshot().scene, Some(Scene::Coding));
        tracker.completed(Some(Scene::Coding), true, 18, at(11));
        assert_eq!(tracker.snapshot().regular_check_seconds, 180);
    }

    #[test]
    fn diagnostic_counts_distinguish_silence_failure_and_success() {
        let mut tracker = ActivityTracker::default();
        tracker.started();
        tracker.completed(Some(Scene::Work), true, 10, at(2));
        tracker.started();
        tracker.completed(None, true, 10, at(4));
        tracker.refresh(at(7));
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.model_calls, 2);
        assert_eq!(snapshot.successful_checks, 1);
        assert_eq!(snapshot.silent_checks, 1);
        assert_eq!(snapshot.failed_checks, 1);
        assert_eq!(snapshot.last_completed_ago_seconds, Some(5));
        tracker.started();
        tracker.completed(Some(Scene::Work), false, 10, at(8));
        assert_eq!(tracker.snapshot().successful_checks, 2);
        assert_eq!(tracker.snapshot().silent_checks, 1);
    }

    #[test]
    fn stable_work_backs_off_but_games_keep_a_short_watchdog() {
        let mut activity = ActivityTracker::default();
        for n in 0..10 {
            activity.completed(Some(Scene::Coding), true, 18, at(n));
        }
        assert_eq!(activity.snapshot().regular_check_seconds, 180);
        for n in 10..20 {
            activity.completed(Some(Scene::Game), true, 18, at(n));
        }
        assert_eq!(activity.snapshot().regular_check_seconds, 20);
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
        assert!(!activity.visual_event(ChangeClassification::Transition, at(10)));
        assert!(activity.visual_event(ChangeClassification::ContinuousMotion, at(15)));
        assert!(!activity.is_transitioning());
    }

    #[test]
    fn stale_and_duplicate_messages_do_not_escape_after_switching_back() {
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
        assert!(!activity.allow_delivery(epoch, " hello ", at(60)));
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
    fn sleep_gap_invalidates_results_and_failures_back_off() {
        let mut activity = ActivityTracker::default();
        activity.environment(EnvironmentSignal::default(), at(0));
        assert!(activity.environment(EnvironmentSignal::default(), at(300)));
        assert_eq!(activity.snapshot().epoch, 1);
        for n in 0..20 {
            activity.completed(None, true, 5, at(n));
        }
        assert_eq!(activity.snapshot().regular_check_seconds, 160);
        assert!(activity.history.len() <= CACHE_CAPACITY);
    }

    #[test]
    fn application_switches_do_not_bypass_failure_retry_deadline() {
        let mut activity = ActivityTracker::default();
        activity.environment(EnvironmentSignal::default(), at(0));
        assert_eq!(activity.completed(None, true, 5, at(1)), at(20));
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
        assert_eq!(activity.completed(None, true, 5, at(21)), at(40));
        activity.environment(EnvironmentSignal::default(), at(22));
        assert!(!activity.retry_ready(at(60)));
        assert!(activity.retry_ready(at(61)));
        activity.completed(Some(Scene::Work), true, 10, at(61));
        assert!(activity.allow_early_event());
        assert!(activity.retry_ready(at(61)));
    }

    #[test]
    fn revisiting_a_familiar_application_preserves_its_backoff() {
        let mut activity = ActivityTracker::default();
        activity.environment(
            EnvironmentSignal {
                application_key: 1,
                ..Default::default()
            },
            at(0),
        );
        for n in 0..8 {
            activity.completed(Some(Scene::Coding), true, 18, at(n));
        }
        activity.environment(
            EnvironmentSignal {
                application_key: 2,
                ..Default::default()
            },
            at(8),
        );
        activity.visual_event(ChangeClassification::NewStableState, at(9));
        activity.completed(Some(Scene::Chat), true, 10, at(9));
        activity.environment(
            EnvironmentSignal {
                application_key: 1,
                ..Default::default()
            },
            at(10),
        );
        assert!(!activity.visual_event(ChangeClassification::NewStableState, at(11)));
        assert!(activity.regular_delay(at(11)) > at(100));
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
            activity.completed(Some(Scene::Other), true, 10, now);
            activity.allow_delivery(activity.snapshot().epoch, &format!("event {n}"), now);
            assert!(activity.history.len() <= CACHE_CAPACITY);
            assert!(activity.delivered.len() <= CACHE_CAPACITY);
        }
    }
}
