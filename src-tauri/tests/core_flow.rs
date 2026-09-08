use std::{
    fs,
    future::pending,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use ai_jarvis_lib::{
    asr::{
        AsrEngine, AsrFailure, AsrModel, AsrModelFuture, AsrModelInput, DirectUserMessage,
        DirectUserMessageSink, ObservationTranscript, ObservationTranscriptSink, UserVoiceOutcome,
        UserVoiceOutcomeSink,
    },
    audio::{MicrophoneTempStore, SystemAudioSegment, UserVoiceGate},
    chat::{
        ChatEngine, ChatEventSink, ChatFailureKind, ChatModel, ChatModelFuture, ChatModelInput,
        ChatRequestStatus, ChatStatusEvent,
    },
    llama::{ObservationOutput, Scene},
    observation::{
        scheduled_observation, ObservationEvent, ObservationEventSink, ObservationFailure,
        ObservationModel, ObservationModelFuture, ObservationModelInput,
    },
    privacy::CommitOutcome,
    scheduler::Clock,
    screen::ChangeClassification,
    settings::{MvpSettings, SettingsStore},
    state::{RuntimeConfig, RuntimeState},
    supervisor::{EnqueueOutcome, Supervisor, SupervisorError},
    types::{Feature, FeatureSettings, PrivacyState, WorkScope},
};
use tempfile::tempdir;
use tokio::sync::mpsc;

#[derive(Clone, Default)]
struct IntegrationClock(Arc<Mutex<Duration>>);

impl IntegrationClock {
    fn advance(&self, duration: Duration) {
        *self.0.lock().unwrap() += duration;
    }
}

impl Clock for IntegrationClock {
    fn now(&self) -> Duration {
        *self.0.lock().unwrap()
    }
}

#[derive(Default)]
struct IntegrationChatModel {
    blocked: AtomicBool,
    replies_started: AtomicUsize,
}

impl ChatModel for IntegrationChatModel {
    fn tokenize<'a>(&'a self, content: &'a str) -> ChatModelFuture<'a, usize> {
        Box::pin(async move { Ok(content.chars().count().max(1)) })
    }

    fn reply<'a>(&'a self, input: ChatModelInput) -> ChatModelFuture<'a, String> {
        self.replies_started.fetch_add(1, Ordering::AcqRel);
        Box::pin(async move {
            if self.blocked.load(Ordering::Acquire) {
                pending::<Result<String, ChatFailureKind>>().await
            } else {
                Ok(format!("reply: {}", input.user_message))
            }
        })
    }
}

#[derive(Default)]
struct IntegrationObservationModel {
    blocked: AtomicBool,
    calls_started: AtomicUsize,
}

impl ObservationModel for IntegrationObservationModel {
    fn observe<'a>(&'a self, _input: ObservationModelInput) -> ObservationModelFuture<'a> {
        self.calls_started.fetch_add(1, Ordering::AcqRel);
        Box::pin(async move {
            if self.blocked.load(Ordering::Acquire) {
                pending::<Result<ObservationOutput, ObservationFailure>>().await
            } else {
                Ok(ObservationOutput {
                    scene: Scene::Game,
                    message: "第一条\n第二条\n第三条".into(),
                })
            }
        })
    }
}

#[derive(Default)]
struct IntegrationAsrModel {
    blocked: AtomicBool,
    calls_started: AtomicUsize,
}

impl AsrModel for IntegrationAsrModel {
    fn transcribe<'a>(&'a self, input: AsrModelInput) -> AsrModelFuture<'a> {
        self.calls_started.fetch_add(1, Ordering::AcqRel);
        Box::pin(async move {
            if self.blocked.load(Ordering::Acquire) {
                pending::<Result<String, AsrFailure>>().await
            } else if input.samples.first().copied().unwrap_or_default() < 0.0 {
                Ok("voice request".into())
            } else {
                Ok("delete everything".into())
            }
        })
    }
}

#[derive(Default)]
struct IntegrationChatSink(Mutex<Vec<ChatStatusEvent>>);

impl ChatEventSink for IntegrationChatSink {
    fn emit(&self, event: ChatStatusEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[derive(Default)]
struct IntegrationObservationSink(Mutex<Vec<ObservationEvent>>);

impl ObservationEventSink for IntegrationObservationSink {
    fn emit(&self, event: ObservationEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[derive(Default)]
struct IntegrationTranscriptSink(Mutex<Vec<ObservationTranscript>>);

impl ObservationTranscriptSink for IntegrationTranscriptSink {
    fn emit(&self, transcript: ObservationTranscript) {
        self.0.lock().unwrap().push(transcript);
    }
}

struct IntegrationDirectSink(Arc<ChatEngine>);

impl DirectUserMessageSink for IntegrationDirectSink {
    fn emit(&self, message: DirectUserMessage) -> Result<(), ai_jarvis_lib::error::AppError> {
        self.0.submit_text(message.text).map(|_| ())
    }
}

#[derive(Default)]
struct IntegrationVoiceSink(Mutex<Vec<(u64, UserVoiceOutcome)>>);

impl UserVoiceOutcomeSink for IntegrationVoiceSink {
    fn emit(&self, request_id: u64, outcome: UserVoiceOutcome) {
        self.0.lock().unwrap().push((request_id, outcome));
    }
}

async fn wait_until(label: &str, condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}"));
}

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

#[tokio::test]
async fn all_mvp_sources_switch_without_crossing_roles_or_leaking_transients() {
    let (configuration, runtime) = active_runtime();
    let runtime = Arc::new(runtime);
    let config = runtime_config();
    let chat_model = Arc::new(IntegrationChatModel::default());
    let observation_model = Arc::new(IntegrationObservationModel::default());
    let asr_model = Arc::new(IntegrationAsrModel::default());
    let chat_sink = Arc::new(IntegrationChatSink::default());
    let observation_sink = Arc::new(IntegrationObservationSink::default());
    let transcript_sink = Arc::new(IntegrationTranscriptSink::default());
    let voice_sink = Arc::new(IntegrationVoiceSink::default());
    let chat_model_api: Arc<dyn ChatModel> = chat_model.clone();
    let observation_model_api: Arc<dyn ObservationModel> = observation_model.clone();
    let asr_model_api: Arc<dyn AsrModel> = asr_model.clone();
    let chat_sink_api: Arc<dyn ChatEventSink> = chat_sink.clone();
    let observation_sink_api: Arc<dyn ObservationEventSink> = observation_sink.clone();
    let transcript_sink_api: Arc<dyn ObservationTranscriptSink> = transcript_sink.clone();
    let voice_sink_api: Arc<dyn UserVoiceOutcomeSink> = voice_sink.clone();

    let chat = ChatEngine::new(
        Arc::clone(&runtime),
        chat_model_api,
        chat_sink_api,
        "integration persona".into(),
        config.chat,
    )
    .expect("chat engine");
    let clock = IntegrationClock::default();
    let observation = ai_jarvis_lib::observation::ObservationEngine::new(
        Arc::clone(&runtime),
        observation_model_api,
        observation_sink_api,
        "integration persona".into(),
        config.screen,
        clock.clone(),
    )
    .expect("observation engine");
    let direct_sink: Arc<dyn DirectUserMessageSink> =
        Arc::new(IntegrationDirectSink(Arc::clone(&chat)));
    let asr = AsrEngine::new_with_user_voice_outcomes(
        Arc::clone(&runtime),
        asr_model_api,
        transcript_sink_api,
        direct_sink,
        voice_sink_api,
        config.audio.system_audio_queue_capacity,
    )
    .expect("ASR engine");
    let microphone_store =
        MicrophoneTempStore::prepare(configuration.path()).expect("microphone store");
    let voice_gate = UserVoiceGate::default();

    chat.submit_text("typed request".into())
        .expect("typed direct request");
    wait_until("typed reply", || chat.history().len() == 1).await;

    asr.submit_system(
        SystemAudioSegment::new(1, 10, vec![100; 160], config.audio).expect("system audio"),
    )
    .expect("system ASR");
    wait_until("system transcript", || {
        transcript_sink.0.lock().unwrap().len() == 1
    })
    .await;
    assert_eq!(chat.history().len(), 1, "system audio is never direct chat");
    assert_eq!(
        transcript_sink.0.lock().unwrap()[0].text,
        "delete everything"
    );

    let reservation = voice_gate.reserve().expect("voice reservation");
    let mut recording = reservation
        .start_recording(&microphone_store, config.audio)
        .expect("voice recording");
    recording.append(&[-100; 160]).expect("voice samples");
    asr.submit_user(recording.finish().expect("voice segment"))
        .expect("user ASR");
    wait_until("voice direct reply", || chat.history().len() == 2).await;
    wait_until("voice submitted outcome", || {
        voice_sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|(_, outcome)| *outcome == UserVoiceOutcome::Submitted)
    })
    .await;
    assert!(!voice_gate.is_reserved());

    let images = (0..3)
        .map(|index| format!("data:image/png;base64,integration-{index}"))
        .collect();
    assert!(observation
        .submit(scheduled_observation(
            1,
            ChangeClassification::NewStableState,
            images,
            serde_json::to_string(&*transcript_sink.0.lock().unwrap()).expect("audio observations"),
        ))
        .expect("game observation"));
    wait_until("game event", || {
        !observation_sink.0.lock().unwrap().is_empty()
    })
    .await;
    let game = observation_sink.0.lock().unwrap()[0].clone();
    assert_eq!(game.scene, Scene::Game);
    assert_eq!(game.message.lines().count(), 3);

    let chat_events_before = chat_sink.0.lock().unwrap().len();
    let observations_before = observation_sink.0.lock().unwrap().len();
    let transcripts_before = transcript_sink.0.lock().unwrap().len();
    let outcomes_before = voice_sink.0.lock().unwrap().len();
    let chat_calls_before = chat_model.replies_started.load(Ordering::Acquire);
    let observation_calls_before = observation_model.calls_started.load(Ordering::Acquire);
    let asr_calls_before = asr_model.calls_started.load(Ordering::Acquire);
    chat_model.blocked.store(true, Ordering::Release);
    observation_model.blocked.store(true, Ordering::Release);
    asr_model.blocked.store(true, Ordering::Release);

    clock.advance(Duration::from_secs(30));
    assert!(observation
        .submit(scheduled_observation(
            2,
            ChangeClassification::NewStableState,
            (0..3)
                .map(|index| format!("data:image/png;base64,late-{index}"))
                .collect(),
            String::new(),
        ))
        .expect("late observation"));
    wait_until("late observation in flight", || {
        observation_model.calls_started.load(Ordering::Acquire) > observation_calls_before
    })
    .await;
    asr.submit_system(
        SystemAudioSegment::new(2, 20, vec![200; 160], config.audio).expect("late system audio"),
    )
    .expect("late system ASR");
    wait_until("late ASR in flight", || {
        asr_model.calls_started.load(Ordering::Acquire) > asr_calls_before
    })
    .await;
    let reservation = voice_gate.reserve().expect("queued voice reservation");
    let mut recording = reservation
        .start_recording(&microphone_store, config.audio)
        .expect("queued voice recording");
    recording
        .append(&[-200; 160])
        .expect("queued voice samples");
    asr.submit_user(recording.finish().expect("queued voice segment"))
        .expect("queued user ASR");
    assert_eq!(asr.pending_lengths().0, 1);

    chat.submit_text("late typed request".into())
        .expect("late direct request");
    wait_until("late chat in flight", || {
        chat_model.replies_started.load(Ordering::Acquire) > chat_calls_before
    })
    .await;
    assert!(!observation
        .submit(scheduled_observation(
            3,
            ChangeClassification::NewStableState,
            (0..3)
                .map(|index| format!("data:image/png;base64,blocked-{index}"))
                .collect(),
            String::new(),
        ))
        .expect("direct interaction suppresses new observation"));

    runtime
        .set_privacy_paused(true)
        .await
        .expect("privacy pause");
    observation.cancel();
    asr.cancel_pending();
    wait_until("all paused tasks to stop", || {
        runtime.supervisor().task_count() == 0 && !voice_gate.is_reserved()
    })
    .await;
    wait_until("late chat cancellation", || {
        chat_sink.0.lock().unwrap()[chat_events_before..]
            .iter()
            .any(|event| event.status == ChatRequestStatus::Cancelled)
    })
    .await;

    assert_eq!(chat.history().len(), 2);
    assert_eq!(
        observation_sink.0.lock().unwrap().len(),
        observations_before
    );
    assert_eq!(transcript_sink.0.lock().unwrap().len(), transcripts_before);
    assert_eq!(voice_sink.0.lock().unwrap().len(), outcomes_before);
    assert_eq!(asr.pending_lengths(), (0, 0));
    let queues = runtime.supervisor().queue_lengths();
    assert_eq!(
        [
            queues.frame_ring,
            queues.audio_ring,
            queues.direct_queue,
            queues.observation_queue,
            queues.user_voice_queue,
            queues.system_audio_queue,
            runtime.supervisor().output_count(),
            runtime.supervisor().task_count(),
        ],
        [0; 8]
    );
    assert_eq!(
        fs::read_dir(configuration.path().join("microphone"))
            .expect("microphone directory")
            .count(),
        0
    );

    chat_model.blocked.store(false, Ordering::Release);
    observation_model.blocked.store(false, Ordering::Release);
    asr_model.blocked.store(false, Ordering::Release);
    runtime
        .set_privacy_paused(false)
        .await
        .expect("privacy resume");
    chat.submit_text("recovered request".into())
        .expect("recovered direct request");
    wait_until("recovered direct reply", || chat.history().len() == 3).await;
}
