use std::{fs, time::Duration};

use ai_jarvis_lib::{
    privacy::CommitOutcome,
    settings::{MvpSettings, SettingsStore},
    state::{RuntimeConfig, RuntimeState},
    supervisor::{EnqueueOutcome, Supervisor, SupervisorError},
    types::{Feature, FeatureSettings, PrivacyState, WorkScope},
};
use tempfile::tempdir;
use tokio::sync::mpsc;

fn runtime_config() -> RuntimeConfig {
    RuntimeConfig::parse(include_str!("../../resources/config/defaults.toml"))
        .expect("bundled defaults must be a valid runtime contract")
}

fn active_runtime() -> (tempfile::TempDir, RuntimeState) {
    let directory = tempdir().expect("temporary configuration directory");
    let store = SettingsStore::new(directory.path());
    store
        .save_startup_state(PrivacyState::Active)
        .expect("active startup state");
    store
        .save_settings(MvpSettings {
            features: FeatureSettings {
                screen_enabled: true,
                system_audio_enabled: true,
                proactive_response_enabled: true,
            },
            excluded_applications: Vec::new(),
            excluded_windows: Vec::new(),
        })
        .expect("enabled settings");
    let runtime = RuntimeState::load(directory.path(), runtime_config()).expect("runtime");
    (directory, runtime)
}

#[test]
fn every_runtime_lane_has_a_deterministic_full_policy() {
    let limits = runtime_config().supervisor_limits;
    let supervisor = Supervisor::<usize, usize, usize, usize>::new(limits).expect("supervisor");

    for value in 0..limits.direct_qwen_queue_capacity {
        assert_eq!(
            supervisor.try_enqueue_direct(value),
            Ok(EnqueueOutcome::Accepted)
        );
    }
    assert_eq!(
        supervisor.try_enqueue_direct(usize::MAX),
        Err(SupervisorError::QueueFull)
    );

    for value in 0..=limits.observation_queue_capacity {
        let outcome = supervisor
            .try_enqueue_observation(
                WorkScope::Observation {
                    uses_screen: true,
                    uses_system_audio: false,
                },
                value,
            )
            .expect("observation enqueue");
        if value == limits.observation_queue_capacity {
            assert_eq!(outcome, EnqueueOutcome::ReplacedLatest);
        }
    }

    for value in 0..=limits.system_audio_queue_capacity {
        let outcome = supervisor
            .try_enqueue_system_audio(value)
            .expect("system audio enqueue");
        if value == limits.system_audio_queue_capacity {
            assert_eq!(outcome, EnqueueOutcome::DroppedOldest);
        }
    }

    assert_eq!(
        supervisor.try_enqueue_user_voice(1),
        Ok(EnqueueOutcome::Accepted)
    );
    assert_eq!(
        supervisor.try_enqueue_user_voice(2),
        Err(SupervisorError::QueueFull)
    );

    for value in 0..=limits.frame_ring_capacity {
        let outcome = supervisor.push_frame(value).expect("frame enqueue");
        if value == limits.frame_ring_capacity {
            assert_eq!(outcome, EnqueueOutcome::DroppedOldest);
        }
    }
    assert!(supervisor
        .queue_lengths()
        .within_limits(supervisor.limits()));
}

#[tokio::test]
async fn source_switches_invalidate_observations_without_harming_direct_work() {
    let (_directory, runtime) = active_runtime();
    let direct = runtime.admit(WorkScope::Direct).expect("direct ticket");
    let observation = runtime
        .admit(WorkScope::Observation {
            uses_screen: true,
            uses_system_audio: true,
        })
        .expect("observation ticket");
    runtime
        .supervisor()
        .try_enqueue_direct(())
        .expect("direct queue reservation");
    runtime
        .supervisor()
        .try_enqueue_observation(
            WorkScope::Observation {
                uses_screen: true,
                uses_system_audio: true,
            },
            (),
        )
        .expect("observation queue reservation");

    let transition = runtime
        .set_feature_enabled(Feature::ProactiveResponse, false)
        .await
        .expect("disable proactive response");
    assert_eq!(
        transition.clear_transient_output,
        Some(ai_jarvis_lib::types::ClearTransientOutputReason::ProactiveResponseDisabled)
    );
    assert_eq!(runtime.supervisor().queue_lengths().direct_queue, 1);
    assert_eq!(runtime.supervisor().queue_lengths().observation_queue, 0);

    let (sender, mut receiver) = mpsc::channel(2);
    assert_eq!(
        runtime.try_commit(&direct, &sender, "direct"),
        CommitOutcome::Committed
    );
    assert_eq!(
        runtime.try_commit(&observation, &sender, "observation"),
        CommitOutcome::Stale("observation")
    );
    assert_eq!(receiver.try_recv(), Ok("direct"));
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn one_hundred_pause_cycles_never_publish_a_late_result() {
    let (_directory, runtime) = active_runtime();
    let (sender, mut receiver) = mpsc::channel(1);

    for sequence in 0..100_u32 {
        let ticket = runtime.admit(WorkScope::Direct).expect("direct ticket");
        runtime
            .set_privacy_paused(true)
            .await
            .expect("pause runtime");
        assert_eq!(
            runtime.try_commit(&ticket, &sender, sequence),
            CommitOutcome::Stale(sequence)
        );
        assert!(receiver.try_recv().is_err());
        runtime
            .set_privacy_paused(false)
            .await
            .expect("resume runtime");
    }
}

#[test]
fn damaged_configuration_starts_paused_with_every_feature_disabled() {
    let directory = tempdir().expect("temporary configuration directory");
    fs::write(directory.path().join("startup-state.json"), b"not-json")
        .expect("damaged startup state");
    fs::write(directory.path().join("settings.json"), b"not-json").expect("damaged settings");

    let runtime = RuntimeState::load(directory.path(), runtime_config()).expect("runtime");
    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.privacy_state, PrivacyState::Paused);
    assert_eq!(snapshot.features, FeatureSettings::default());
    assert_eq!(snapshot.diagnostics.len(), 2);
}

#[tokio::test]
async fn worker_failure_and_repeated_shutdown_remain_bounded() {
    let (_directory, runtime) = active_runtime();
    runtime
        .supervisor()
        .spawn_scoped(WorkScope::Direct, |_| async {
            panic!("injected model failure");
        })
        .expect("fake model task");
    tokio::task::yield_now().await;

    tokio::time::timeout(Duration::from_secs(3), runtime.shutdown())
        .await
        .expect("shutdown deadline")
        .expect("shutdown result");
    tokio::time::timeout(Duration::from_millis(100), runtime.shutdown())
        .await
        .expect("idempotent shutdown deadline")
        .expect("idempotent shutdown result");
}
