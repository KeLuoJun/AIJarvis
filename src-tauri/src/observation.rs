use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::{
    error::{AppError, ErrorCode},
    llama::{
        HttpTransport, LlamaError, LlamaService, ModelOutput, ModelRequest, ObservationOutput,
        ProcessSpawner, Scene,
    },
    prompts::PromptId,
    scheduler::{Clock, ObservationCandidate, ObservationDispatch, ObservationScheduler},
    screen::{ChangeClassification, ScreenRuntimeConfig},
    state::RuntimeState,
    supervisor::SupervisorError,
    types::WorkScope,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationPayload {
    pub image_data_urls: Vec<String>,
    pub audio_observations: String,
}

pub type ScheduledObservation = ObservationCandidate<ObservationPayload>;

#[derive(Clone, Debug)]
pub struct ObservationModelInput {
    pub persona: String,
    pub runtime_state: Value,
    pub image_data_urls: Vec<String>,
    pub audio_observations: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationFailure {
    ModelUnavailable,
    Timeout,
    InvalidResponse,
    Internal,
}

pub type ObservationModelFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ObservationOutput, ObservationFailure>> + Send + 'a>>;

pub trait ObservationModel: Send + Sync {
    fn observe<'a>(&'a self, input: ObservationModelInput) -> ObservationModelFuture<'a>;
}

pub struct UnavailableObservationModel;

impl ObservationModel for UnavailableObservationModel {
    fn observe<'a>(&'a self, _input: ObservationModelInput) -> ObservationModelFuture<'a> {
        Box::pin(async { Err(ObservationFailure::ModelUnavailable) })
    }
}

pub struct LlamaObservationModel<S, T> {
    service: Arc<LlamaService<S, T>>,
    timeout: Duration,
}

impl<S, T> LlamaObservationModel<S, T> {
    pub fn new(service: Arc<LlamaService<S, T>>, timeout: Duration) -> Self {
        Self { service, timeout }
    }
}

impl<S, T> ObservationModel for LlamaObservationModel<S, T>
where
    S: ProcessSpawner + 'static,
    T: HttpTransport + 'static,
{
    fn observe<'a>(&'a self, input: ObservationModelInput) -> ObservationModelFuture<'a> {
        Box::pin(async move {
            let variables = BTreeMap::from([
                ("persona".into(), Value::String(input.persona)),
                (
                    "runtime_state".into(),
                    Value::String(input.runtime_state.to_string()),
                ),
                (
                    "audio_observations".into(),
                    Value::String(input.audio_observations),
                ),
            ]);
            match self
                .service
                .request(
                    ModelRequest {
                        prompt: PromptId::Observation,
                        variables,
                        image_data_urls: input.image_data_urls,
                    },
                    self.timeout,
                )
                .await
                .map_err(map_llama_error)?
            {
                ModelOutput::Observation(output) => Ok(output),
                ModelOutput::DirectChat(_) => Err(ObservationFailure::InvalidResponse),
            }
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservationEvent {
    pub runtime_revision: u64,
    pub runtime_generation: u64,
    pub latest_frame_sequence: u64,
    pub scene: Scene,
    pub need_response: bool,
    pub next_check_seconds: u8,
    pub message: String,
}

pub trait ObservationEventSink: Send + Sync {
    fn emit(&self, event: ObservationEvent);
}

pub struct ObservationEngine<C> {
    runtime: Arc<RuntimeState>,
    model: Arc<dyn ObservationModel>,
    sink: Arc<dyn ObservationEventSink>,
    persona: String,
    config: ScreenRuntimeConfig,
    scheduler: Mutex<ObservationScheduler<C, ObservationPayload>>,
}

impl<C: Clock + Send + 'static> ObservationEngine<C> {
    pub fn new(
        runtime: Arc<RuntimeState>,
        model: Arc<dyn ObservationModel>,
        sink: Arc<dyn ObservationEventSink>,
        persona: String,
        config: ScreenRuntimeConfig,
        clock: C,
    ) -> Result<Arc<Self>, AppError> {
        if persona.trim().is_empty() {
            return Err(AppError::invalid_argument("The active persona is empty."));
        }
        Self::build(runtime, model, sink, persona, config, clock)
    }

    pub fn unavailable(
        runtime: Arc<RuntimeState>,
        sink: Arc<dyn ObservationEventSink>,
        config: ScreenRuntimeConfig,
        clock: C,
    ) -> Result<Arc<Self>, AppError> {
        Self::build(
            runtime,
            Arc::new(UnavailableObservationModel),
            sink,
            String::new(),
            config,
            clock,
        )
    }

    fn build(
        runtime: Arc<RuntimeState>,
        model: Arc<dyn ObservationModel>,
        sink: Arc<dyn ObservationEventSink>,
        persona: String,
        config: ScreenRuntimeConfig,
        clock: C,
    ) -> Result<Arc<Self>, AppError> {
        let scheduler = ObservationScheduler::new(clock, config)
            .ok_or_else(|| AppError::invalid_argument("Screen scheduling limits are invalid."))?;
        Ok(Arc::new(Self {
            runtime,
            model,
            sink,
            persona,
            config,
            scheduler: Mutex::new(scheduler),
        }))
    }

    pub fn submit(self: &Arc<Self>, observation: ScheduledObservation) -> Result<bool, AppError> {
        self.validate_payload(&observation.payload)?;
        let dispatch = self.lock_scheduler().observe(observation);
        let Some(dispatch) = dispatch else {
            return Ok(false);
        };
        self.spawn_dispatch(dispatch)?;
        Ok(true)
    }

    pub fn cancel(&self) {
        self.lock_scheduler().cancel();
    }

    pub fn pending_len(&self) -> usize {
        self.lock_scheduler().pending_len()
    }

    fn spawn_dispatch(
        self: &Arc<Self>,
        dispatch: ObservationDispatch<ObservationPayload>,
    ) -> Result<(), AppError> {
        let uses_system_audio = !dispatch
            .candidate
            .payload
            .audio_observations
            .trim()
            .is_empty();
        let scope = WorkScope::Observation {
            uses_screen: !dispatch.candidate.payload.image_data_urls.is_empty(),
            uses_system_audio,
        };
        let ticket = match self.runtime.admit(scope) {
            Ok(ticket) => ticket,
            Err(error) => {
                self.cancel();
                return Err(error);
            }
        };
        let runtime_generation = ticket.global_generation();
        let ticket_cancellation = ticket.cancellation_token();
        let engine = Arc::clone(self);
        let spawn =
            self.runtime
                .supervisor()
                .spawn_scoped(scope, move |supervisor_cancel| async move {
                    let snapshot = engine.runtime.snapshot();
                    let runtime_revision = snapshot.revision;
                    let runtime_state = match serde_json::to_value(snapshot) {
                        Ok(value) => value,
                        Err(_) => {
                            engine.finish(dispatch.candidate.latest_frame_sequence, None);
                            return;
                        }
                    };
                    let input = ObservationModelInput {
                        persona: engine.persona.clone(),
                        runtime_state,
                        image_data_urls: dispatch.candidate.payload.image_data_urls,
                        audio_observations: dispatch.candidate.payload.audio_observations,
                    };
                    let result = tokio::select! {
                        _ = ticket_cancellation.cancelled() => None,
                        _ = supervisor_cancel.cancelled() => None,
                        output = engine.model.observe(input) => Some(output),
                    };
                    let Some(result) = result else {
                        engine.cancel_dispatch(dispatch.candidate.latest_frame_sequence);
                        return;
                    };
                    let result = result.and_then(|mut output| {
                        let interval = u64::from(output.next_check_seconds);
                        if output.need_response == output.message.trim().is_empty()
                            || interval < engine.config.next_check_min_seconds
                            || interval > engine.config.next_check_max_seconds
                        {
                            Err(ObservationFailure::InvalidResponse)
                        } else {
                            if output.scene == Scene::Game && output.need_response {
                                output.message =
                                    crate::danmaku::normalize_game_message(&output.message)
                                        .ok_or(ObservationFailure::InvalidResponse)?;
                            }
                            Ok(output)
                        }
                    });
                    let next_check = result
                        .as_ref()
                        .ok()
                        .map(|output| u64::from(output.next_check_seconds));
                    if let Ok(output) = result {
                        engine.commit_event(
                            &ticket,
                            ObservationEvent {
                                runtime_revision,
                                runtime_generation,
                                latest_frame_sequence: dispatch.candidate.latest_frame_sequence,
                                scene: output.scene,
                                need_response: output.need_response,
                                next_check_seconds: output.next_check_seconds,
                                message: output.message,
                            },
                        );
                    }
                    engine.finish(dispatch.candidate.latest_frame_sequence, next_check);
                });
        if let Err(error) = spawn {
            self.cancel();
            return Err(map_supervisor_error(error));
        }
        Ok(())
    }

    fn commit_event(&self, ticket: &crate::privacy::WorkTicket, event: ObservationEvent) {
        let (sender, mut receiver) = mpsc::channel(1);
        if matches!(
            self.runtime.try_commit(ticket, &sender, event),
            crate::privacy::CommitOutcome::Committed
        ) {
            self.sink.emit(
                receiver
                    .try_recv()
                    .expect("committed observation is queued"),
            );
        }
    }

    fn finish(self: &Arc<Self>, active_sequence: u64, next_check_seconds: Option<u64>) {
        let next = self
            .lock_scheduler()
            .complete(active_sequence, next_check_seconds);
        if let Some(dispatch) = next {
            if self.spawn_dispatch(dispatch).is_err() {
                self.cancel();
            }
        }
    }

    fn cancel_dispatch(&self, active_sequence: u64) {
        self.lock_scheduler().cancel_dispatch(active_sequence);
    }

    pub fn clear_audio_pending(&self) {
        self.lock_scheduler()
            .clear_pending_if(|payload| !payload.audio_observations.trim().is_empty());
    }

    fn validate_payload(&self, payload: &ObservationPayload) -> Result<(), AppError> {
        let count = payload.image_data_urls.len();
        let valid_images = count >= self.config.request_images_min
            && count <= self.config.request_images_max
            && payload
                .image_data_urls
                .iter()
                .all(|image| image.starts_with("data:image/") && image.contains(";base64,"));
        let valid_audio_only = count == 0 && !payload.audio_observations.trim().is_empty();
        if !valid_images && !valid_audio_only {
            return Err(AppError::invalid_argument(
                "Observation input does not satisfy the image or audio-only contract.",
            ));
        }
        Ok(())
    }

    fn lock_scheduler(&self) -> MutexGuard<'_, ObservationScheduler<C, ObservationPayload>> {
        self.scheduler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub fn scheduled_observation(
    sequence: u64,
    classification: ChangeClassification,
    image_data_urls: Vec<String>,
    audio_observations: String,
) -> ScheduledObservation {
    ObservationCandidate {
        latest_frame_sequence: sequence,
        classification,
        payload: ObservationPayload {
            image_data_urls,
            audio_observations,
        },
    }
}

fn map_llama_error(error: LlamaError) -> ObservationFailure {
    match error {
        LlamaError::ArtifactMissing
        | LlamaError::Spawn
        | LlamaError::AlreadyStarted
        | LlamaError::NotReady
        | LlamaError::ProcessExited => ObservationFailure::ModelUnavailable,
        LlamaError::Timeout => ObservationFailure::Timeout,
        LlamaError::Protocol | LlamaError::Schema => ObservationFailure::InvalidResponse,
        LlamaError::Config | LlamaError::Http | LlamaError::Stopping => {
            ObservationFailure::Internal
        }
    }
}

fn map_supervisor_error(error: SupervisorError) -> AppError {
    match error {
        SupervisorError::QueueFull => AppError::queue_full(),
        SupervisorError::Closed => AppError::shutting_down(),
        SupervisorError::InvalidLimits
        | SupervisorError::RuntimeUnavailable
        | SupervisorError::InvalidScope
        | SupervisorError::TaskIdExhausted => AppError::new(
            ErrorCode::Internal,
            "Observation work could not be scheduled.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use tempfile::{tempdir, TempDir};
    use tokio::{sync::Notify, time::timeout};

    use crate::{
        settings::{MvpSettings, SettingsStore},
        state::RuntimeConfig,
        types::{Feature, FeatureSettings, PrivacyState},
    };

    use super::*;

    #[derive(Clone, Default)]
    struct FakeClock(Arc<Mutex<Duration>>);

    impl FakeClock {
        fn advance(&self, duration: Duration) {
            *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) += duration;
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

    struct FakeModel {
        inputs: Mutex<Vec<ObservationModelInput>>,
        outputs: Mutex<VecDeque<Result<ObservationOutput, ObservationFailure>>>,
        started: Notify,
        release: Mutex<Option<Arc<Notify>>>,
    }

    impl FakeModel {
        fn new(
            outputs: impl IntoIterator<Item = Result<ObservationOutput, ObservationFailure>>,
        ) -> Arc<Self> {
            Arc::new(Self {
                inputs: Mutex::new(Vec::new()),
                outputs: Mutex::new(outputs.into_iter().collect()),
                started: Notify::new(),
                release: Mutex::new(None),
            })
        }

        fn block(&self) -> Arc<Notify> {
            let release = Arc::new(Notify::new());
            *self
                .release
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&release));
            release
        }
    }

    impl ObservationModel for FakeModel {
        fn observe<'a>(&'a self, input: ObservationModelInput) -> ObservationModelFuture<'a> {
            self.inputs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(input);
            let output = self
                .outputs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .unwrap_or(Err(ObservationFailure::Internal));
            let release = self
                .release
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            self.started.notify_one();
            Box::pin(async move {
                if let Some(release) = release {
                    release.notified().await;
                }
                output
            })
        }
    }

    #[derive(Default)]
    struct FakeSink {
        events: Mutex<Vec<ObservationEvent>>,
        changed: Notify,
    }

    impl FakeSink {
        fn events(&self) -> Vec<ObservationEvent> {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        async fn wait_for_event(&self) {
            timeout(Duration::from_secs(1), async {
                loop {
                    let changed = self.changed.notified();
                    if !self.events().is_empty() {
                        return;
                    }
                    changed.await;
                }
            })
            .await
            .expect("observation event");
        }
    }

    impl ObservationEventSink for FakeSink {
        fn emit(&self, event: ObservationEvent) {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
            self.changed.notify_waiters();
        }
    }

    fn output(message: &str) -> ObservationOutput {
        ObservationOutput {
            scene: Scene::Coding,
            need_response: !message.is_empty(),
            next_check_seconds: 10,
            message: message.into(),
        }
    }

    fn game_output(message: &str) -> ObservationOutput {
        ObservationOutput {
            scene: Scene::Game,
            need_response: true,
            next_check_seconds: 5,
            message: message.into(),
        }
    }

    fn images(count: usize, prefix: &str) -> Vec<String> {
        (0..count)
            .map(|index| format!("data:image/png;base64,{prefix}-{index}"))
            .collect()
    }

    fn active_runtime() -> (TempDir, Arc<RuntimeState>, RuntimeConfig) {
        let directory = tempdir().expect("configuration directory");
        let store = SettingsStore::new(directory.path());
        store
            .save_startup_state(PrivacyState::Active)
            .expect("active state");
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
            .expect("enabled features");
        let config = RuntimeConfig::parse(include_str!("../../resources/config/defaults.toml"))
            .expect("runtime defaults");
        let runtime =
            Arc::new(RuntimeState::load(directory.path(), config).expect("active runtime"));
        (directory, runtime, config)
    }

    fn engine(
        runtime: Arc<RuntimeState>,
        model: Arc<FakeModel>,
        sink: Arc<FakeSink>,
        config: ScreenRuntimeConfig,
        clock: FakeClock,
    ) -> Arc<ObservationEngine<FakeClock>> {
        ObservationEngine::new(runtime, model, sink, "persona".into(), config, clock)
            .expect("observation engine")
    }

    #[tokio::test]
    async fn submits_ordered_five_image_observation_and_emits_valid_silence() {
        let (_directory, runtime, config) = active_runtime();
        let model = FakeModel::new([Ok(output(""))]);
        let sink = Arc::new(FakeSink::default());
        let clock = FakeClock::default();
        let engine = engine(
            runtime,
            Arc::clone(&model),
            Arc::clone(&sink),
            config.screen,
            clock,
        );
        let expected_images = images(5, "ordered");

        assert!(engine
            .submit(scheduled_observation(
                5,
                ChangeClassification::NewStableState,
                expected_images.clone(),
                String::new(),
            ))
            .expect("scheduled"));
        sink.wait_for_event().await;

        assert_eq!(model.inputs.lock().unwrap().len(), 1);
        assert_eq!(
            model.inputs.lock().unwrap()[0].image_data_urls,
            expected_images
        );
        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].latest_frame_sequence, 5);
        assert!(!events[0].need_response);
        assert!(events[0].message.is_empty());
    }

    #[tokio::test]
    async fn audio_only_observation_does_not_depend_on_screen_feature() {
        let (_directory, runtime, config) = active_runtime();
        runtime
            .set_feature_enabled(Feature::Screen, false)
            .await
            .expect("disable screen");
        let model = FakeModel::new([Ok(output("audio response"))]);
        let sink = Arc::new(FakeSink::default());
        let engine = engine(
            runtime,
            Arc::clone(&model),
            Arc::clone(&sink),
            config.screen,
            FakeClock::default(),
        );
        let audio = r#"[{"segmentSequence":7,"capturedAtMs":11,"text":"untrusted"}]"#;

        assert!(engine
            .submit(scheduled_observation(
                9,
                ChangeClassification::NewStableState,
                Vec::new(),
                audio.into(),
            ))
            .expect("audio-only observation"));
        sink.wait_for_event().await;

        let inputs = model.inputs.lock().unwrap();
        assert_eq!(inputs.len(), 1);
        assert!(inputs[0].image_data_urls.is_empty());
        assert_eq!(inputs[0].audio_observations, audio);
    }

    #[tokio::test]
    async fn busy_model_keeps_only_latest_observation_batch() {
        let (_directory, runtime, config) = active_runtime();
        let model = FakeModel::new([Ok(output("first")), Ok(output("latest"))]);
        let release = model.block();
        let sink = Arc::new(FakeSink::default());
        let clock = FakeClock::default();
        let engine = engine(
            runtime,
            Arc::clone(&model),
            sink,
            config.screen,
            clock.clone(),
        );

        engine
            .submit(scheduled_observation(
                1,
                ChangeClassification::Transition,
                images(5, "first"),
                String::new(),
            ))
            .expect("first");
        timeout(Duration::from_secs(1), model.started.notified())
            .await
            .expect("model started");
        clock.advance(Duration::from_secs(5));
        for sequence in 2..=100 {
            engine
                .submit(scheduled_observation(
                    sequence,
                    ChangeClassification::Transition,
                    images(5, "pending"),
                    String::new(),
                ))
                .expect("coalesced");
            assert_eq!(engine.pending_len(), 1);
        }
        release.notify_waiters();
        timeout(Duration::from_secs(1), async {
            loop {
                if model.inputs.lock().unwrap().len() == 2
                    && engine.runtime.supervisor().task_count() == 0
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("latest observation started");
        assert_eq!(
            model.inputs.lock().unwrap()[1].image_data_urls,
            images(5, "pending")
        );
        engine.cancel();
    }

    #[tokio::test]
    async fn pause_and_source_switches_suppress_in_flight_results() {
        for feature in [
            None,
            Some(Feature::Screen),
            Some(Feature::ProactiveResponse),
        ] {
            let (_directory, runtime, config) = active_runtime();
            let model = FakeModel::new([Ok(output("must not emit"))]);
            let _release = model.block();
            let sink = Arc::new(FakeSink::default());
            let engine = engine(
                Arc::clone(&runtime),
                Arc::clone(&model),
                Arc::clone(&sink),
                config.screen,
                FakeClock::default(),
            );
            engine
                .submit(scheduled_observation(
                    1,
                    ChangeClassification::Transition,
                    images(5, "late"),
                    String::new(),
                ))
                .expect("observation");
            timeout(Duration::from_secs(1), model.started.notified())
                .await
                .expect("model started");
            match feature {
                Some(feature) => {
                    runtime
                        .set_feature_enabled(feature, false)
                        .await
                        .expect("disable source");
                }
                None => {
                    runtime
                        .set_privacy_paused(true)
                        .await
                        .expect("pause runtime");
                }
            }
            for _ in 0..20 {
                if runtime.supervisor().task_count() == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(sink.events().is_empty());
            assert_eq!(runtime.supervisor().task_count(), 0);
        }
    }

    #[tokio::test]
    async fn rejects_invalid_image_counts_without_model_work() {
        let (_directory, runtime, config) = active_runtime();
        let model = FakeModel::new([Ok(output("unused"))]);
        let sink = Arc::new(FakeSink::default());
        let engine = engine(
            runtime,
            Arc::clone(&model),
            sink,
            config.screen,
            FakeClock::default(),
        );
        assert!(engine
            .submit(scheduled_observation(
                1,
                ChangeClassification::Transition,
                images(4, "short"),
                String::new()
            ))
            .is_err());
        assert!(model.inputs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn normalizes_valid_game_batches_and_rejects_invalid_batches() {
        let (_directory, runtime, config) = active_runtime();
        let model = FakeModel::new([
            Ok(game_output("漂亮！差一点？稳住。")),
            Ok(game_output("只有一条")),
        ]);
        let sink = Arc::new(FakeSink::default());
        let clock = FakeClock::default();
        let engine = engine(
            runtime,
            Arc::clone(&model),
            Arc::clone(&sink),
            config.screen,
            clock.clone(),
        );

        assert!(engine
            .submit(scheduled_observation(
                1,
                ChangeClassification::NewStableState,
                images(5, "game-valid"),
                String::new(),
            ))
            .expect("valid game observation"));
        sink.wait_for_event().await;
        assert_eq!(sink.events()[0].message, "漂亮！\n差一点？\n稳住。");

        clock.advance(Duration::from_secs(5));
        assert!(engine
            .submit(scheduled_observation(
                2,
                ChangeClassification::NewStableState,
                images(5, "game-invalid"),
                String::new(),
            ))
            .expect("invalid game observation still runs"));
        timeout(Duration::from_secs(1), async {
            loop {
                if model.inputs.lock().unwrap().len() == 2 {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("invalid game observation completed");
        assert_eq!(sink.events().len(), 1);
    }
}
