use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::{
    activity::{ActivitySnapshot, ActivityTracker, EnvironmentSignal},
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

const MAX_PENDING_IMAGE_AGE: Duration = Duration::from_secs(10);
const MAX_INTERVAL_IMAGES: usize = 6;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationPayload {
    pub image_data_urls: Vec<String>,
    pub audio_observations: String,
    pub image_captured_at: Vec<Instant>,
    elapsed_since_previous: Option<Duration>,
    epoch: u64,
    observed_at: Duration,
    scene_probe: bool,
    pub(crate) foreground_application: String,
    pub(crate) foreground_window_title: String,
}

struct ObservationControl<C> {
    scheduler: ObservationScheduler<C, ObservationPayload>,
    activity: ActivityTracker,
    audio: VecDeque<(Value, Duration)>,
    audio_seen: VecDeque<(String, Duration)>,
    last_observation_started: Option<Duration>,
    cancellation: tokio_util::sync::CancellationToken,
    wake_running: bool,
    next_sequence: u64,
}

struct DispatchGuard<C: Clock + Send + 'static> {
    engine: std::sync::Weak<ObservationEngine<C>>,
    sequence: u64,
}

impl<C: Clock + Send + 'static> Drop for DispatchGuard<C> {
    fn drop(&mut self) {
        if let Some(engine) = self.engine.upgrade() {
            engine.cancel_dispatch(self.sequence);
        }
    }
}

impl<C: Clock> ObservationControl<C> {
    fn add_audio(&mut self, input: &str, now: Duration) {
        self.audio_seen
            .retain(|(_, at)| now.saturating_sub(*at) < Duration::from_secs(600));
        let Ok(Value::Array(values)) = serde_json::from_str(input) else {
            return;
        };
        for value in values.into_iter().take(16) {
            let Some(text) = value.get("text").and_then(Value::as_str) else {
                continue;
            };
            if text.trim().is_empty() || text.len() > 16_000 {
                continue;
            }
            // A teacher may genuinely repeat a sentence. Deduplicate source segments,
            // never equal words from different moments.
            let normalized = value.to_string();
            if normalized.is_empty() || self.audio_seen.iter().any(|(old, _)| old == &normalized) {
                continue;
            }
            self.audio_seen.push_back((normalized, now));
            self.audio.push_back((value, now));
            while self.audio.len() > 128
                || self
                    .audio
                    .iter()
                    .map(|(value, _)| {
                        value
                            .get("text")
                            .and_then(Value::as_str)
                            .map_or(0, str::len)
                    })
                    .sum::<usize>()
                    > 32_000
            {
                self.audio.pop_front();
                crate::dev_event!("observation.audio_overflow", "capacity" => 128);
            }
            if self.audio_seen.len() > 256 {
                self.audio_seen.pop_front();
            }
        }
    }
}

impl ObservationPayload {
    fn prepend_pending_images(&mut self, pending: &mut Self) {
        let mut urls = std::mem::take(&mut pending.image_data_urls);
        let mut captured_at = std::mem::take(&mut pending.image_captured_at);
        urls.append(&mut self.image_data_urls);
        captured_at.append(&mut self.image_captured_at);
        let overflow = urls.len().saturating_sub(MAX_INTERVAL_IMAGES);
        if overflow > 0 {
            urls.drain(..overflow);
            captured_at.drain(..overflow);
            crate::dev_event!("observation.images_trimmed", "dropped" => overflow,
                "retained" => MAX_INTERVAL_IMAGES);
        }
        self.image_data_urls = urls;
        self.image_captured_at = captured_at;
        self.scene_probe |= pending.scene_probe;
        if self.foreground_application.is_empty() {
            self.foreground_application = pending.foreground_application.clone();
        }
        if self.foreground_window_title.is_empty() {
            self.foreground_window_title = pending.foreground_window_title.clone();
        }
    }
}

pub type ScheduledObservation = ObservationCandidate<ObservationPayload>;

#[derive(Clone, Debug)]
pub struct ObservationModelInput {
    pub persona: String,
    pub recent_responses: Value,
    pub image_data_urls: Vec<String>,
    pub audio_observations: String,
    pub image_captured_at: Vec<Instant>,
    pub elapsed_since_previous: Option<Duration>,
    pub foreground_application: String,
    pub foreground_window_title: String,
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

    fn classify_scene<'a>(&'a self, input: ObservationModelInput) -> ObservationModelFuture<'a> {
        Box::pin(async move {
            self.observe(input).await.map(|output| ObservationOutput {
                scene: output.scene,
                message: String::new(),
            })
        })
    }
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
            let images = crate::observation_image::compose(
                &input.image_data_urls,
                &input.image_captured_at,
                Instant::now(),
            )
            .map_err(|_| ObservationFailure::InvalidResponse)?;
            let variables = BTreeMap::from([
                ("persona".into(), Value::String(input.persona)),
                (
                    "recent_responses".into(),
                    Value::String(input.recent_responses.to_string()),
                ),
                (
                    "audio_observations".into(),
                    Value::String(audio_content(&input.audio_observations)),
                ),
                (
                    "elapsed_since_previous".into(),
                    Value::String(input.elapsed_since_previous.map_or_else(
                        || "首次观察".into(),
                        |elapsed| format!("{} 秒", elapsed.as_secs()),
                    )),
                ),
                (
                    "foreground_application".into(),
                    Value::String(input.foreground_application.clone()),
                ),
                (
                    "foreground_window_title".into(),
                    Value::String(input.foreground_window_title.clone()),
                ),
            ]);
            match self
                .service
                .request(
                    ModelRequest {
                        prompt: PromptId::Observation,
                        variables,
                        image_data_urls: images,
                    },
                    self.timeout,
                )
                .await
                .map_err(map_llama_error)?
            {
                ModelOutput::Observation(output) => Ok(output),
                ModelOutput::Scene(_) => Err(ObservationFailure::InvalidResponse),
                ModelOutput::DirectChat(_) => Err(ObservationFailure::InvalidResponse),
            }
        })
    }

    fn classify_scene<'a>(&'a self, input: ObservationModelInput) -> ObservationModelFuture<'a> {
        Box::pin(async move {
            let images = crate::observation_image::compose(
                &input.image_data_urls,
                &input.image_captured_at,
                Instant::now(),
            )
            .map_err(|_| ObservationFailure::InvalidResponse)?;
            let variables = BTreeMap::from([
                (
                    "instruction".into(),
                    Value::String("Classify the attached screen.".into()),
                ),
                (
                    "foreground_application".into(),
                    Value::String(input.foreground_application),
                ),
                (
                    "foreground_window_title".into(),
                    Value::String(input.foreground_window_title),
                ),
            ]);
            match self
                .service
                .request(
                    ModelRequest {
                        prompt: PromptId::SceneCheck,
                        variables,
                        image_data_urls: images,
                    },
                    self.timeout,
                )
                .await
                .map_err(map_llama_error)?
            {
                ModelOutput::Scene(scene) => Ok(ObservationOutput {
                    scene,
                    message: String::new(),
                }),
                _ => Err(ObservationFailure::InvalidResponse),
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
    control: Mutex<ObservationControl<C>>,
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
            control: Mutex::new(ObservationControl {
                scheduler,
                activity: ActivityTracker::default(),
                audio: VecDeque::new(),
                audio_seen: VecDeque::new(),
                last_observation_started: None,
                cancellation: tokio_util::sync::CancellationToken::new(),
                wake_running: false,
                next_sequence: 0,
            }),
        }))
    }

    pub fn submit(
        self: &Arc<Self>,
        mut observation: ScheduledObservation,
    ) -> Result<bool, AppError> {
        self.validate_payload(&observation.payload)?;
        let _source_sequence = observation.latest_frame_sequence;
        crate::dev_event!("observation.input", "images" => observation.payload.image_data_urls.len(),
            "has_audio" => !observation.payload.audio_observations.is_empty(),
            "classification" => format!("{:?}", observation.classification),
            "source_sequence" => _source_sequence);
        // Admit before retaining source data, including during a disabled/paused runtime.
        let _admission = self.runtime.admit(WorkScope::Observation {
            uses_screen: !observation.payload.image_data_urls.is_empty(),
            uses_system_audio: !observation.payload.audio_observations.is_empty(),
        })?;
        {
            let mut control = self.lock_control();
            let now = control.scheduler.now();
            if control.activity.blocked(now)
                || self.runtime.supervisor().queue_lengths().direct_queue > 0
            {
                crate::dev_event!("observation.input_blocked", "mode" => control.activity.snapshot().mode,
                    "direct_queue" => self.runtime.supervisor().queue_lengths().direct_queue);
                return Ok(false);
            }
            control.add_audio(&observation.payload.audio_observations, now);
            observation.payload.audio_observations.clear();
            if observation.payload.image_data_urls.is_empty() && control.audio.is_empty() {
                return Ok(false);
            }
            let visual_settled = !observation.payload.image_data_urls.is_empty()
                && control
                    .activity
                    .visual_event(observation.classification, now);
            let visual_event = control.last_observation_started.is_some()
                && observation.classification == ChangeClassification::Transition
                && !visual_settled;
            if visual_event {
                observation.payload.scene_probe = true;
            }
            // Ordinary visual/audio changes coalesce until the regular deadline.
            // Only the first observation bypasses that deadline; environment boundaries
            // explicitly reset it instead of treating every motion as an emergency.
            observation.classification = if control.last_observation_started.is_none() {
                ChangeClassification::NewStableState
            } else {
                ChangeClassification::Stable
            };
            observation.payload.epoch = control.activity.snapshot().epoch;
            observation.payload.observed_at = now;
            if let Some(pending) = control.scheduler.pending_payload_mut() {
                if pending.epoch == observation.payload.epoch {
                    observation.payload.prepend_pending_images(pending);
                }
            }
            // Capture and audio-only producers have independent sequence spaces.
            control.next_sequence = control
                .next_sequence
                .checked_add(1)
                .filter(|sequence| *sequence < (1_u64 << 53))
                .ok_or_else(|| AppError::invalid_argument("Observation sequence exhausted."))?;
            observation.latest_frame_sequence = control.next_sequence;
            control.scheduler.offer(observation);
        }
        let started = self.pump()?;
        crate::dev_event!("observation.scheduled", "started" => started, "state" => self.activity_snapshot(),
            "pending" => self.pending_len());
        self.ensure_wake();
        Ok(started)
    }

    pub fn cancel(&self) {
        crate::dev_event!("observation.reset");
        let mut control = self.lock_control();
        control.cancellation.cancel();
        control.cancellation = tokio_util::sync::CancellationToken::new();
        control.wake_running = false;
        control.scheduler.cancel();
        control.activity = ActivityTracker::default();
        control.audio.clear();
        control.audio_seen.clear();
        control.last_observation_started = None;
    }

    pub fn pending_len(&self) -> usize {
        self.lock_control().scheduler.pending_len()
    }

    pub fn activity_snapshot(&self) -> ActivitySnapshot {
        let mut control = self.lock_control();
        let now = control.scheduler.now();
        control.activity.refresh(now);
        let mut snapshot = control.activity.snapshot();
        snapshot.model_busy = control.scheduler.is_busy();
        snapshot
    }

    pub fn update_environment(&self, signal: EnvironmentSignal) -> bool {
        let mut control = self.lock_control();
        let now = control.scheduler.now();
        let boundary = control.activity.environment(signal, now);
        if boundary {
            crate::dev_event!("activity.boundary", "epoch" => control.activity.snapshot().epoch,
                "mode" => control.activity.snapshot().mode, "protected" => signal.protected,
                "own_application" => signal.own_application);
            let snapshot = self.runtime.snapshot();
            control.next_sequence = control
                .next_sequence
                .saturating_add(1)
                .min((1_u64 << 53) - 1);
            self.sink.emit(ObservationEvent {
                runtime_revision: snapshot.revision,
                runtime_generation: snapshot.runtime_generation,
                latest_frame_sequence: control.next_sequence,
                scene: Scene::Other,
                need_response: false,
                next_check_seconds: 5,
                message: String::new(),
            });
        }
        if boundary || control.activity.blocked(now) {
            control.cancellation.cancel();
            control.cancellation = tokio_util::sync::CancellationToken::new();
            control.wake_running = false;
            control.scheduler.cancel();
            let delay = Duration::from_secs(5);
            control.scheduler.defer_regular(delay);
            control.audio.clear();
        }
        boundary
    }

    pub fn user_interaction(&self, seconds: u64) {
        crate::dev_event!("activity.interaction", "lease_seconds" => seconds);
        let mut control = self.lock_control();
        let now = control.scheduler.now();
        control
            .activity
            .interact(now, Duration::from_secs(seconds.min(120)));
        control.cancellation.cancel();
        control.cancellation = tokio_util::sync::CancellationToken::new();
        control.wake_running = false;
        control.scheduler.cancel();
        control.audio.clear();
    }

    pub fn set_voice_active(&self, active: bool) {
        self.user_interaction(5);
        let mut control = self.lock_control();
        let now = control.scheduler.now();
        control.activity.set_voice_active(active, now);
    }

    fn pump(self: &Arc<Self>) -> Result<bool, AppError> {
        let snapshot = self.runtime.snapshot();
        if snapshot.privacy_state != crate::types::PrivacyState::Active
            || snapshot.lifecycle_state != crate::types::LifecycleState::Running
            || !snapshot.features.proactive_response_enabled
        {
            self.cancel();
            return Ok(false);
        }
        let dispatch = {
            let mut control = self.lock_control();
            let now = control.scheduler.now();
            let scene_probe_ready = control
                .scheduler
                .pending_payload_mut()
                .is_some_and(|payload| payload.scene_probe);
            if control.activity.blocked(now)
                || (!scene_probe_ready && control.activity.is_transitioning())
                || !control.activity.retry_ready(now)
                || self.runtime.supervisor().queue_lengths().direct_queue > 0
            {
                return Ok(false);
            }
            if let Some(payload) = control.scheduler.pending_payload_mut() {
                if !payload.image_data_urls.is_empty()
                    && (now.saturating_sub(payload.observed_at) > MAX_PENDING_IMAGE_AGE
                        || payload
                            .image_captured_at
                            .last()
                            .is_some_and(|at| at.elapsed() > MAX_PENDING_IMAGE_AGE))
                {
                    payload.image_data_urls.clear();
                    payload.image_captured_at.clear();
                    payload.observed_at = now;
                    crate::dev_event!("observation.stale_images_dropped");
                }
            }
            let allow_event = control.activity.allow_early_event();
            let Some(mut dispatch) = control.scheduler.poll(allow_event) else {
                return Ok(false);
            };
            if !control.audio.is_empty() {
                let values: Vec<_> = control.audio.drain(..).map(|(value, _)| value).collect();
                dispatch.candidate.payload.audio_observations =
                    serde_json::to_string(&values).expect("JSON values");
            }
            if dispatch.candidate.payload.image_data_urls.is_empty()
                && dispatch.candidate.payload.audio_observations.is_empty()
            {
                control.scheduler.cancel();
                return Ok(false);
            }
            control.activity.started();
            dispatch.candidate.payload.elapsed_since_previous = control
                .last_observation_started
                .map(|previous| now.saturating_sub(previous));
            control.last_observation_started = Some(now);
            dispatch
        };
        self.spawn_dispatch(dispatch)?;
        Ok(true)
    }

    fn ensure_wake(self: &Arc<Self>) {
        let mut control = self.lock_control();
        if control.wake_running || control.scheduler.pending_len() == 0 {
            return;
        }
        control.wake_running = true;
        let cancellation = control.cancellation.clone();
        let engine = Arc::downgrade(self);
        // One bounded timer flushes the final audio segment even if no more frames arrive.
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
                let Some(engine) = engine.upgrade() else {
                    break;
                };
                if engine.pump().is_err() {
                    break;
                }
                let mut control = engine.lock_control();
                if cancellation.is_cancelled() {
                    break;
                }
                if control.scheduler.pending_len() == 0 {
                    control.wake_running = false;
                    break;
                }
            }
        });
    }

    fn spawn_dispatch(
        self: &Arc<Self>,
        dispatch: ObservationDispatch<ObservationPayload>,
    ) -> Result<(), AppError> {
        crate::dev_event!("observation.dispatch", "sequence" => dispatch.candidate.latest_frame_sequence,
            "epoch" => dispatch.candidate.payload.epoch, "reason" => format!("{:?}", dispatch.reason),
            "images" => dispatch.candidate.payload.image_data_urls.len(),
            "has_audio" => !dispatch.candidate.payload.audio_observations.is_empty());
        crate::dev_event!("observation.queue_age", "age_ms" => self.lock_control().scheduler.now().saturating_sub(dispatch.candidate.payload.observed_at).as_millis());
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
        let activity_cancellation = self.lock_control().cancellation.clone();
        let guard = DispatchGuard {
            engine: Arc::downgrade(self),
            sequence: dispatch.candidate.latest_frame_sequence,
        };
        let engine = Arc::clone(self);
        let spawn =
            self.runtime
                .supervisor()
                .spawn_scoped(scope, move |supervisor_cancel| async move {
                    let _guard = guard;
                    if activity_cancellation.is_cancelled()
                        || ticket_cancellation.is_cancelled()
                        || supervisor_cancel.is_cancelled()
                        || !engine
                            .lock_control()
                            .scheduler
                            .is_active(dispatch.candidate.latest_frame_sequence)
                    {
                        return;
                    }
                    let snapshot = engine.runtime.snapshot();
                    let runtime_revision = snapshot.revision;
                    let foreground_application = dispatch
                        .candidate
                        .payload
                        .foreground_application
                        .clone();
                    let recent_responses = {
                        let control = engine.lock_control();
                        serde_json::json!(control
                            .activity
                            .recent_responses(control.scheduler.now()))
                    };
                    let input = ObservationModelInput {
                        persona: engine.persona.clone(),
                        recent_responses,
                        image_data_urls: dispatch.candidate.payload.image_data_urls,
                        audio_observations: dispatch.candidate.payload.audio_observations,
                        image_captured_at: dispatch.candidate.payload.image_captured_at,
                        elapsed_since_previous: dispatch.candidate.payload.elapsed_since_previous,
                        foreground_application,
                        foreground_window_title: dispatch
                            .candidate
                            .payload
                            .foreground_window_title
                            .clone(),
                    };
                    crate::dev_event!("observation.context", "recent_replies" => input.recent_responses.as_array().map_or(0, Vec::len),
                        "elapsed_seconds" => input.elapsed_since_previous.map(|elapsed| elapsed.as_secs()),
                        "audio_segments" => serde_json::from_str::<Vec<Value>>(&input.audio_observations).map_or(0, |values| values.len()));
                    let result = tokio::select! {
                        biased;
                        _ = activity_cancellation.cancelled() => None,
                        _ = ticket_cancellation.cancelled() => None,
                        _ = supervisor_cancel.cancelled() => None,
                        output = if dispatch.candidate.payload.scene_probe {
                            engine.model.classify_scene(input)
                        } else {
                            engine.model.observe(input)
                        } => Some(output),
                    };
                    let Some(result) = result else {
                        engine.cancel_dispatch(dispatch.candidate.latest_frame_sequence);
                        return;
                    };
                    let result = result.and_then(|mut output| {
                        if output.scene == Scene::Game && !output.message.trim().is_empty() {
                            output.message = crate::danmaku::normalize_game_message(&output.message)
                                .ok_or(ObservationFailure::InvalidResponse)?;
                        }
                        Ok(output)
                    });
                    let mut control = engine.lock_control();
                    let now = control.scheduler.now();
                    if !control
                        .scheduler
                        .is_active(dispatch.candidate.latest_frame_sequence)
                        || ticket_cancellation.is_cancelled()
                        || supervisor_cancel.is_cancelled()
                        || activity_cancellation.is_cancelled()
                    {
                        return;
                    }
                    let current =
                        dispatch.candidate.payload.epoch == control.activity.snapshot().epoch;
                    crate::dev_event!("observation.model_result", "sequence" => dispatch.candidate.latest_frame_sequence,
                        "current" => current, "success" => result.is_ok(),
                        "failure" => result.as_ref().err().map(|error| format!("{error:?}")));
                    let interval = if let Ok(mut output) = result {
                        if current {
                            let predicted = output.scene.clone();
                            let authoritative = dispatch.candidate.payload.scene_probe
                                || control.activity.snapshot().scene.is_none();
                            let scene = if authoritative {
                                control.activity.resolve_scene(predicted.clone())
                            } else {
                                control
                                    .activity
                                    .snapshot()
                                    .scene
                                    .unwrap_or(predicted.clone())
                            };
                            crate::dev_event!("observation.scene", "predicted" => predicted, "resolved" => scene,
                                "authoritative" => authoritative, "scene_probe" => dispatch.candidate.payload.scene_probe);
                            if authoritative {
                                control.activity.set_scene(scene.clone());
                            }
                            output.scene = scene;
                            let deliver = !output.message.trim().is_empty() && control.activity.allow_delivery(
                                    dispatch.candidate.payload.epoch,
                                    &output.message,
                                    now,
                                );
                            let interval = control.activity.completed(
                                Some(output.scene.clone()),
                                !deliver,
                                now,
                            );
                            crate::dev_event!("observation.completed", "sequence" => dispatch.candidate.latest_frame_sequence,
                                "scene" => output.scene, "need_response" => deliver,
                                "next_check_seconds" => interval.as_secs(), "state" => control.activity.snapshot());
                            engine.commit_event(
                                &ticket,
                                ObservationEvent {
                                    runtime_revision,
                                    runtime_generation,
                                    latest_frame_sequence: dispatch.candidate.latest_frame_sequence,
                                    scene: output.scene,
                                    need_response: deliver,
                                    next_check_seconds: interval.as_secs() as u8,
                                    message: if deliver { output.message } else { String::new() },
                                },
                            );
                            interval
                        } else {
                            Duration::from_secs(5)
                        }
                    } else {
                        control.activity.completed(None, true, now)
                    };
                    control
                        .scheduler
                        .settle(dispatch.candidate.latest_frame_sequence, interval);
                });
        if let Err(error) = spawn {
            self.cancel();
            return Err(map_supervisor_error(error));
        }
        Ok(())
    }

    fn commit_event(&self, ticket: &crate::privacy::WorkTicket, event: ObservationEvent) {
        crate::dev_event!("observation.delivery_attempt", "sequence" => event.latest_frame_sequence,
            "need_response" => event.need_response);
        let (sender, mut receiver) = mpsc::channel(1);
        if matches!(
            self.runtime.try_commit(ticket, &sender, event),
            crate::privacy::CommitOutcome::Committed
        ) {
            crate::dev_event!("observation.delivery_committed");
            self.sink.emit(
                receiver
                    .try_recv()
                    .expect("committed observation is queued"),
            );
        }
    }

    fn cancel_dispatch(&self, active_sequence: u64) {
        let mut control = self.lock_control();
        if control.scheduler.is_active(active_sequence) {
            crate::dev_event!("observation.cancelled", "sequence" => active_sequence);
            control.scheduler.cancel_dispatch(active_sequence);
        }
    }

    pub fn clear_audio_pending(&self) {
        let mut control = self.lock_control();
        control.audio.clear();
        control.audio_seen.clear();
        control
            .scheduler
            .clear_pending_if(|payload| payload.image_data_urls.is_empty());
    }

    fn validate_payload(&self, payload: &ObservationPayload) -> Result<(), AppError> {
        let count = payload.image_data_urls.len();
        let valid_images = count >= self.config.request_images_min
            && count <= MAX_INTERVAL_IMAGES
            && payload.image_captured_at.len() == count
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

    fn lock_control(&self) -> MutexGuard<'_, ObservationControl<C>> {
        self.control
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
            image_captured_at: vec![Instant::now(); image_data_urls.len()],
            image_data_urls,
            audio_observations,
            elapsed_since_previous: None,
            epoch: 0,
            observed_at: Duration::ZERO,
            scene_probe: false,
            foreground_application: String::new(),
            foreground_window_title: String::new(),
        },
    }
}

fn audio_content(input: &str) -> String {
    serde_json::from_str::<Vec<Value>>(input)
        .unwrap_or_default()
        .iter()
        .filter_map(|value| value.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn map_llama_error(error: LlamaError) -> ObservationFailure {
    match error {
        LlamaError::ArtifactMissing
        | LlamaError::Spawn
        | LlamaError::EndpointInUse
        | LlamaError::AlreadyStarted
        | LlamaError::NotReady
        | LlamaError::ProcessExited => ObservationFailure::ModelUnavailable,
        LlamaError::Timeout => ObservationFailure::Timeout,
        LlamaError::BadRequest | LlamaError::Protocol | LlamaError::Schema => {
            ObservationFailure::InvalidResponse
        }
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
            scene: Scene::Other,
            message: message.into(),
        }
    }

    fn game_output(message: &str) -> ObservationOutput {
        ObservationOutput {
            scene: Scene::Game,
            message: message.into(),
        }
    }

    fn images(count: usize, prefix: &str) -> Vec<String> {
        (0..count)
            .map(|index| format!("data:image/png;base64,{prefix}-{index}"))
            .collect()
    }

    async fn wait_idle(engine: &Arc<ObservationEngine<FakeClock>>) {
        timeout(Duration::from_secs(1), async {
            while engine.lock_control().scheduler.is_busy() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("model completion");
    }

    #[tokio::test]
    async fn stable_hour_reduces_model_calls_without_stopping_observation() {
        let (_dir, runtime, config) = active_runtime();
        let model = FakeModel::new((0..400).map(|_| Ok(output("这一步可以先检查运行结果。"))));
        let sink = Arc::new(FakeSink::default());
        let clock = FakeClock::default();
        let engine = engine(
            runtime,
            model.clone(),
            sink.clone(),
            config.screen,
            clock.clone(),
        );
        for second in 0..3600 {
            if second % 3 == 0 {
                engine
                    .submit(scheduled_observation(
                        second,
                        ChangeClassification::Stable,
                        images(3, "stable"),
                        String::new(),
                    ))
                    .unwrap();
                wait_idle(&engine).await;
            }
            clock.advance(Duration::from_secs(1));
        }
        let calls = model.inputs.lock().unwrap().len();
        assert!((30..=34).contains(&calls), "stable hour calls: {calls}");
        let events = sink.events();
        assert_eq!(
            events.len(),
            calls,
            "every successful generation is delivered"
        );
        assert!(events
            .iter()
            .all(|event| event.need_response && !event.message.is_empty()));
        {
            let inputs = model.inputs.lock().unwrap();
            assert!(inputs[0].recent_responses.as_array().unwrap().is_empty());
            assert_eq!(inputs[1].recent_responses[0], events[0].message);
        }
        assert!(engine.pending_len() <= 1);
        let before = calls;
        engine
            .submit(scheduled_observation(
                4000,
                ChangeClassification::NewStableState,
                images(3, "new-event"),
                String::new(),
            ))
            .unwrap();
        wait_idle(&engine).await;
        assert_eq!(
            model.inputs.lock().unwrap().len(),
            before,
            "ordinary change must respect the deadline"
        );
        engine.cancel();
    }

    #[tokio::test]
    async fn ordinary_motion_and_audio_coalesce_until_deadline() {
        let (_dir, runtime, config) = active_runtime();
        let model = FakeModel::new([Ok(output("first")), Ok(output("second"))]);
        let clock = FakeClock::default();
        let engine = engine(
            runtime,
            model.clone(),
            Arc::new(FakeSink::default()),
            config.screen,
            clock.clone(),
        );
        assert!(engine
            .submit(scheduled_observation(
                0,
                ChangeClassification::Stable,
                images(1, "first"),
                String::new()
            ))
            .unwrap());
        wait_idle(&engine).await;
        for second in 1..30 {
            clock.advance(Duration::from_secs(1));
            assert!(!engine.submit(scheduled_observation(second, ChangeClassification::NewStableState,
                images(1, &format!("sample-{second}")), serde_json::json!([{"segmentSequence":second,"text":format!("phrase {second}")}]).to_string())).unwrap());
        }
        assert_eq!(model.inputs.lock().unwrap().len(), 1);
        clock.advance(Duration::from_secs(1));
        assert!(engine.pump().unwrap());
        wait_idle(&engine).await;
        let inputs = model.inputs.lock().unwrap();
        let audio: Vec<Value> = serde_json::from_str(&inputs[1].audio_observations).unwrap();
        assert_eq!(audio.len(), 29);
        assert_eq!(audio[0]["text"], "phrase 1");
        assert_eq!(audio[28]["text"], "phrase 29");
        assert_eq!(inputs[1].image_data_urls.len(), 6);
        assert!(inputs[1].image_data_urls[0].contains("sample-24"));
        assert!(inputs[1].image_data_urls[5].contains("sample-29"));
        let content = audio_content(&inputs[1].audio_observations);
        assert!(content.starts_with("phrase 1\nphrase 2\n"));
        assert!(!content.contains("segmentSequence"));
        assert_eq!(
            inputs[1].elapsed_since_previous,
            Some(Duration::from_secs(30))
        );
    }

    #[tokio::test]
    async fn silence_updates_scene_and_success_without_delivery_or_failure() {
        let (_dir, runtime, config) = active_runtime();
        let model = FakeModel::new([Ok(ObservationOutput {
            scene: Scene::Other,
            message: String::new(),
        })]);
        let sink = Arc::new(FakeSink::default());
        let engine = engine(
            runtime,
            model,
            sink.clone(),
            config.screen,
            FakeClock::default(),
        );
        engine
            .submit(scheduled_observation(
                1,
                ChangeClassification::Stable,
                images(1, "lecture"),
                String::new(),
            ))
            .unwrap();
        sink.wait_for_event().await;
        let events = sink.events();
        assert_eq!(events[0].scene, Scene::Other);
        assert!(!events[0].need_response);
        assert!(events[0].message.is_empty());
        let snapshot = engine.activity_snapshot();
        assert_eq!(snapshot.successful_checks, 1);
        assert_eq!(snapshot.silent_checks, 1);
        assert_eq!(snapshot.failed_checks, 0);
        assert!(engine
            .lock_control()
            .activity
            .recent_responses(Duration::ZERO)
            .is_empty());
    }

    #[tokio::test]
    async fn audio_arriving_during_inference_belongs_to_next_interval() {
        let (_dir, runtime, config) = active_runtime();
        let model = FakeModel::new([Ok(output("first")), Ok(output("second"))]);
        let release = model.block();
        let clock = FakeClock::default();
        let engine = engine(
            runtime,
            model.clone(),
            Arc::new(FakeSink::default()),
            config.screen,
            clock.clone(),
        );
        let audio = |sequence| {
            scheduled_observation(
                sequence,
                ChangeClassification::Stable,
                Vec::new(),
                serde_json::json!([{"segmentSequence":sequence,"text":"repeated sentence"}])
                    .to_string(),
            )
        };
        assert!(engine.submit(audio(1)).unwrap());
        timeout(Duration::from_secs(1), model.started.notified())
            .await
            .unwrap();
        clock.advance(Duration::from_secs(2));
        assert!(!engine.submit(audio(2)).unwrap());
        release.notify_one();
        wait_idle(&engine).await;
        *model.release.lock().unwrap() = None;
        clock.advance(Duration::from_secs(30));
        assert!(engine.pump().unwrap());
        wait_idle(&engine).await;
        let inputs = model.inputs.lock().unwrap();
        for (index, input) in inputs.iter().enumerate() {
            let audio: Vec<Value> = serde_json::from_str(&input.audio_observations).unwrap();
            assert_eq!(audio.len(), 1);
            assert_eq!(audio[0]["segmentSequence"], index + 1);
        }
    }

    #[tokio::test]
    async fn audio_is_deduplicated_and_final_segment_can_flush_without_new_input() {
        let (_dir, runtime, config) = active_runtime();
        let model = FakeModel::new([
            Ok(output("这一步可以先检查运行结果。")),
            Ok(output("这一步可以先检查运行结果。")),
        ]);
        let clock = FakeClock::default();
        let engine = engine(
            runtime,
            model.clone(),
            Arc::new(FakeSink::default()),
            config.screen,
            clock.clone(),
        );
        let audio = |seq, text| {
            scheduled_observation(
                seq,
                ChangeClassification::Stable,
                Vec::new(),
                serde_json::json!([{"text": text, "segmentSequence":seq}]).to_string(),
            )
        };
        assert!(engine.submit(audio(1, "first phrase")).unwrap());
        wait_idle(&engine).await;
        clock.advance(Duration::from_secs(1));
        assert!(!engine.submit(audio(1, "first phrase")).unwrap());
        assert!(!engine.submit(audio(3, "new final phrase")).unwrap());
        clock.advance(Duration::from_secs(30));
        assert!(engine.pump().unwrap());
        wait_idle(&engine).await;
        let inputs = model.inputs.lock().unwrap();
        assert_eq!(inputs.len(), 2);
        assert!(inputs[1].image_data_urls.is_empty());
        assert!(inputs[1].audio_observations.contains("new final phrase"));
        assert!(!inputs[1].audio_observations.contains("first phrase"));
    }

    #[tokio::test]
    async fn user_takeover_and_environment_switch_cancel_stale_results() {
        for user_takeover in [true, false] {
            let (_dir, runtime, config) = active_runtime();
            let model = FakeModel::new([Ok(output("stale")), Ok(output("fresh"))]);
            let release = model.block();
            let sink = Arc::new(FakeSink::default());
            let clock = FakeClock::default();
            let engine = engine(
                runtime,
                model.clone(),
                sink.clone(),
                config.screen,
                clock.clone(),
            );
            engine.update_environment(EnvironmentSignal {
                application_key: 1,
                ..Default::default()
            });
            engine
                .submit(scheduled_observation(
                    1,
                    ChangeClassification::NewStableState,
                    images(3, "old"),
                    String::new(),
                ))
                .unwrap();
            timeout(Duration::from_secs(1), model.started.notified())
                .await
                .unwrap();
            if user_takeover {
                engine.user_interaction(15);
            } else {
                engine.update_environment(EnvironmentSignal {
                    application_key: 2,
                    ..Default::default()
                });
            }
            release.notify_waiters();
            tokio::task::yield_now().await;
            assert!(sink
                .events()
                .iter()
                .all(|event| !event.need_response && event.message.is_empty()));
            sink.events.lock().unwrap().clear();
            clock.advance(Duration::from_secs(20));
            *model.release.lock().unwrap() = None;
            assert!(engine
                .submit(scheduled_observation(
                    2,
                    ChangeClassification::NewStableState,
                    images(3, "new"),
                    String::new()
                ))
                .unwrap());
            sink.wait_for_event().await;
            assert_eq!(sink.events()[0].message, "fresh");
        }
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
    async fn submits_ordered_keyframe_observation_and_emits_a_response() {
        let (_directory, runtime, config) = active_runtime();
        let model = FakeModel::new([Ok(output("这一步可以先检查运行结果。"))]);
        let sink = Arc::new(FakeSink::default());
        let clock = FakeClock::default();
        let engine = engine(
            runtime,
            Arc::clone(&model),
            Arc::clone(&sink),
            config.screen,
            clock,
        );
        let expected_images = images(3, "ordered");

        assert!(engine
            .submit(scheduled_observation(
                (1_u64 << 63) + 5,
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
        assert_eq!(
            events[0].latest_frame_sequence, 1,
            "producer counters must not escape to JavaScript"
        );
        assert!(events[0].need_response);
        assert!(!events[0].message.is_empty());
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
        assert_eq!(
            serde_json::from_str::<Value>(&inputs[0].audio_observations).unwrap(),
            serde_json::from_str::<Value>(audio).unwrap()
        );
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
                ChangeClassification::NewStableState,
                images(3, "first"),
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
                    ChangeClassification::NewStableState,
                    images(3, "pending"),
                    String::new(),
                ))
                .expect("coalesced");
            assert_eq!(engine.pending_len(), 1);
        }
        release.notify_one();
        timeout(Duration::from_secs(1), async {
            while engine.lock_control().scheduler.is_busy() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first completion");
        assert_eq!(model.inputs.lock().unwrap().len(), 1);
        clock.advance(Duration::from_secs(30));
        assert!(engine
            .submit(scheduled_observation(
                101,
                ChangeClassification::NewStableState,
                images(3, "pending"),
                String::new()
            ))
            .expect("regular dispatch with fresh images"));
        timeout(Duration::from_secs(1), async {
            loop {
                if model.inputs.lock().unwrap().len() == 2 {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("latest observation started");
        let inputs = model.inputs.lock().unwrap();
        assert_eq!(inputs[1].image_data_urls.len(), 6);
        assert_eq!(&inputs[1].image_data_urls[3..], images(3, "pending"));
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
                    ChangeClassification::NewStableState,
                    images(3, "late"),
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
                images(7, "oversized"),
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
                images(3, "game-valid"),
                String::new(),
            ))
            .expect("valid game observation"));
        sink.wait_for_event().await;
        assert_eq!(sink.events()[0].message, "漂亮！\n差一点？\n稳住。");

        clock.advance(Duration::from_secs(30));
        assert!(engine
            .submit(scheduled_observation(
                2,
                ChangeClassification::NewStableState,
                images(3, "game-invalid"),
                String::new(),
            ))
            .expect("invalid game observation still runs"));
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
        .expect("invalid game observation completed");
        assert_eq!(sink.events().len(), 1);
    }
}
