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
    activity::{
        ActiveSession, ActivitySnapshot, ActivityTracker, CourseConfirmationChoice,
        EnvironmentSignal,
    },
    course_notes::CourseNotes,
    error::{AppError, ErrorCode},
    llama::{
        HttpTransport, LlamaError, LlamaService, ModelOutput, ModelRequest, ObservationOutput,
        ProcessSpawner, Scene,
    },
    perception::PerceptionContext,
    persona::{PersonaManager, PersonaSource},
    prompts::{sanitize_prompt_text, PromptId},
    scheduler::{Clock, ObservationCandidate, ObservationDispatch, ObservationScheduler},
    screen::{ChangeClassification, ScreenRuntimeConfig},
    state::RuntimeState,
    supervisor::SupervisorError,
    types::WorkScope,
};

const MAX_PENDING_IMAGE_AGE: Duration = Duration::from_secs(10);
const MAX_INTERVAL_IMAGES: usize = 3;
// Keep dynamic text well below the multimodal model's context budget. The
// scheduler's source buffers remain larger so short bursts can still be
// coalesced; only the model-facing view is trimmed here.
const MAX_AUDIO_PROMPT_CHARS: usize = 4_000;
const MAX_RECENT_RESPONSE_PROMPT_CHARS: usize = 2_000;
const MAX_FOREGROUND_APPLICATION_PROMPT_CHARS: usize = 256;
const MAX_FOREGROUND_WINDOW_TITLE_PROMPT_CHARS: usize = 512;

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
    pub current_scene: Scene,
    pub active_session: ActiveSession,
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
pub type SceneClassificationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Scene, ObservationFailure>> + Send + 'a>>;

pub trait ObservationModel: Send + Sync {
    fn observe<'a>(&'a self, input: ObservationModelInput) -> ObservationModelFuture<'a>;

    fn classify_scene<'a>(&'a self, input: ObservationModelInput) -> SceneClassificationFuture<'a>;
}

pub struct UnavailableObservationModel;

impl ObservationModel for UnavailableObservationModel {
    fn observe<'a>(&'a self, _input: ObservationModelInput) -> ObservationModelFuture<'a> {
        Box::pin(async { Err(ObservationFailure::ModelUnavailable) })
    }

    fn classify_scene<'a>(
        &'a self,
        _input: ObservationModelInput,
    ) -> SceneClassificationFuture<'a> {
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
                    Value::String(sanitize_prompt_text(&input.recent_responses.to_string())),
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
                    "current_scene".into(),
                    serde_json::to_value(&input.current_scene)
                        .map_err(|_| ObservationFailure::Internal)?,
                ),
                (
                    "active_session".into(),
                    serde_json::to_value(input.active_session)
                        .map_err(|_| ObservationFailure::Internal)?,
                ),
                (
                    "foreground_application".into(),
                    Value::String(bounded_sanitized_text(
                        &input.foreground_application,
                        MAX_FOREGROUND_APPLICATION_PROMPT_CHARS,
                    )),
                ),
                (
                    "foreground_window_title".into(),
                    Value::String(bounded_sanitized_text(
                        &input.foreground_window_title,
                        MAX_FOREGROUND_WINDOW_TITLE_PROMPT_CHARS,
                    )),
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
                ModelOutput::CourseSummary(_) => Err(ObservationFailure::InvalidResponse),
            }
        })
    }

    fn classify_scene<'a>(&'a self, input: ObservationModelInput) -> SceneClassificationFuture<'a> {
        Box::pin(async move {
            // Scene classification uses the latest complete frame directly.
            let images = input.image_data_urls.last().cloned().into_iter().collect();
            let variables = BTreeMap::from([
                (
                    "instruction".into(),
                    Value::String("请对附加的最新屏幕画面进行场景分类。".into()),
                ),
                (
                    "foreground_application".into(),
                    Value::String(bounded_sanitized_text(
                        &input.foreground_application,
                        MAX_FOREGROUND_APPLICATION_PROMPT_CHARS,
                    )),
                ),
                (
                    "foreground_window_title".into(),
                    Value::String(bounded_sanitized_text(
                        &input.foreground_window_title,
                        MAX_FOREGROUND_WINDOW_TITLE_PROMPT_CHARS,
                    )),
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
                ModelOutput::Scene(scene) => Ok(scene),
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
    pub active_session: ActiveSession,
    pub need_response: bool,
    pub next_check_seconds: u8,
    pub message: String,
}

enum ModelTaskOutput {
    Observation(ObservationOutput),
    Scene(Scene),
}

pub trait ObservationEventSink: Send + Sync {
    fn emit(&self, event: ObservationEvent);
}

pub struct ObservationEngine<C> {
    runtime: Arc<RuntimeState>,
    model: Arc<dyn ObservationModel>,
    sink: Arc<dyn ObservationEventSink>,
    persona: PersonaSource,
    perception: Arc<PerceptionContext>,
    config: ScreenRuntimeConfig,
    control: Mutex<ObservationControl<C>>,
    course_notes: Mutex<Arc<CourseNotes>>,
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
        Self::build(
            runtime,
            model,
            sink,
            PersonaSource::Static(persona),
            PerceptionContext::new(),
            config,
            clock,
        )
    }

    pub fn new_shared(
        runtime: Arc<RuntimeState>,
        model: Arc<dyn ObservationModel>,
        sink: Arc<dyn ObservationEventSink>,
        personas: Arc<PersonaManager>,
        perception: Arc<PerceptionContext>,
        config: ScreenRuntimeConfig,
        clock: C,
    ) -> Result<Arc<Self>, AppError> {
        Self::build(
            runtime,
            model,
            sink,
            PersonaSource::Managed(personas),
            perception,
            config,
            clock,
        )
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
            PersonaSource::Static(String::new()),
            PerceptionContext::new(),
            config,
            clock,
        )
    }

    fn build(
        runtime: Arc<RuntimeState>,
        model: Arc<dyn ObservationModel>,
        sink: Arc<dyn ObservationEventSink>,
        persona: PersonaSource,
        perception: Arc<PerceptionContext>,
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
            perception,
            config,
            control: Mutex::new(ObservationControl {
                scheduler,
                activity: ActivityTracker::with_game_interval_seconds(config.game_interval_seconds),
                audio: VecDeque::new(),
                audio_seen: VecDeque::new(),
                last_observation_started: None,
                cancellation: tokio_util::sync::CancellationToken::new(),
                wake_running: false,
                next_sequence: 0,
            }),
            course_notes: Mutex::new(CourseNotes::disabled()),
        }))
    }

    pub fn set_course_notes(&self, course_notes: Arc<CourseNotes>) {
        *self
            .course_notes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = course_notes;
    }

    fn course_notes(&self) -> Arc<CourseNotes> {
        Arc::clone(
            &self
                .course_notes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    pub fn submit(
        self: &Arc<Self>,
        mut observation: ScheduledObservation,
    ) -> Result<bool, AppError> {
        self.validate_payload(&observation.payload)?;
        let runtime_snapshot = self.runtime.snapshot();
        if runtime_snapshot.privacy_state == crate::types::PrivacyState::Active {
            let screen_enabled = runtime_snapshot.features.screen_enabled;
            self.perception.update(
                if screen_enabled {
                    observation.payload.image_data_urls.clone()
                } else {
                    Vec::new()
                },
                if screen_enabled {
                    observation.payload.image_captured_at.clone()
                } else {
                    Vec::new()
                },
                if runtime_snapshot.features.system_audio_enabled {
                    &observation.payload.audio_observations
                } else {
                    ""
                },
                if screen_enabled {
                    observation.payload.foreground_application.clone()
                } else {
                    String::new()
                },
                if screen_enabled {
                    observation.payload.foreground_window_title.clone()
                } else {
                    String::new()
                },
            );
        }
        if runtime_snapshot.privacy_state == crate::types::PrivacyState::Active {
            let course = self.activity_snapshot();
            let notes = self.course_notes();
            notes.sync(
                course.active_session == crate::activity::ActiveSession::Course,
                course.course_completion_id,
            );
            notes.record(
                &observation.payload.image_data_urls,
                &observation.payload.audio_observations,
            );
        }
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
            let visual_event =
                observation.classification == ChangeClassification::Transition && !visual_settled;
            if visual_event || control.activity.is_transitioning() {
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
        control.activity =
            ActivityTracker::with_game_interval_seconds(self.config.game_interval_seconds);
        control.audio.clear();
        control.audio_seen.clear();
        control.last_observation_started = None;
    }

    pub fn clear_perception(&self) {
        self.perception.clear();
    }

    pub fn clear_perception_images(&self) {
        self.perception.clear_images();
    }

    pub fn clear_perception_audio(&self) {
        self.perception.clear_audio();
    }

    pub fn pending_len(&self) -> usize {
        self.lock_control().scheduler.pending_len()
    }

    pub fn activity_snapshot(&self) -> ActivitySnapshot {
        let snapshot = {
            let mut control = self.lock_control();
            let now = control.scheduler.now();
            control.activity.refresh(now);
            let mut snapshot = control.activity.snapshot();
            snapshot.model_busy = control.scheduler.is_busy();
            snapshot
        };
        self.course_notes().sync(
            snapshot.active_session == crate::activity::ActiveSession::Course,
            snapshot.course_completion_id,
        );
        snapshot
    }

    pub fn course_session_active(&self) -> bool {
        self.lock_control().activity.snapshot().active_session == ActiveSession::Course
    }

    pub fn respond_course_confirmation(&self, keep_course: bool) -> ActivitySnapshot {
        let snapshot = {
            let mut control = self.lock_control();
            let now = control.scheduler.now();
            let choice = if keep_course {
                CourseConfirmationChoice::Continue
            } else {
                CourseConfirmationChoice::End
            };
            control.activity.choose_course_confirmation(choice, now);
            control.activity.snapshot()
        };
        self.course_notes().sync(
            snapshot.active_session == crate::activity::ActiveSession::Course,
            snapshot.course_completion_id,
        );
        snapshot
    }

    pub fn update_environment(&self, signal: EnvironmentSignal) -> bool {
        let mut control = self.lock_control();
        let now = control.scheduler.now();
        let boundary = control.activity.environment(signal, now);
        if boundary {
            if signal.protected {
                self.perception.clear();
            }
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
                active_session: control.activity.snapshot().active_session,
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
                    let (recent_responses, current_scene, active_session) = {
                        let control = engine.lock_control();
                        (
                            bounded_recent_responses(
                                control
                                    .activity
                                    .recent_responses(control.scheduler.now()),
                            ),
                            control.activity.snapshot().scene.unwrap_or(Scene::Other),
                            control.activity.snapshot().active_session,
                        )
                    };
                    let requested_scene = current_scene.clone();
                    let input = ObservationModelInput {
                        persona: engine.persona.current_prompt(),
                        recent_responses,
                        current_scene,
                        active_session,
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
                        output = async {
                            if dispatch.candidate.payload.scene_probe {
                                let note_context = is_course_note_context(
                                    &input.foreground_application,
                                    &input.foreground_window_title,
                                );
                                return engine
                                    .model
                                    .classify_scene(input)
                                    .await
                                    .map(|scene| {
                                        if note_context && scene == Scene::Course {
                                            crate::dev_event!(
                                                "observation.course_note_excluded",
                                                "reason" => "course_note_window"
                                            );
                                            Scene::Other
                                        } else {
                                            scene
                                        }
                                    })
                                    .map(ModelTaskOutput::Scene);
                            }
                            engine
                                .model
                                .observe(input)
                                .await
                                .and_then(|mut output| {
                                    if requested_scene == Scene::Game
                                        && active_session != ActiveSession::Course
                                        && !output.message.trim().is_empty()
                                    {
                                        output.message = crate::danmaku::normalize_game_message(
                                            &output.message,
                                        )
                                        .ok_or(ObservationFailure::InvalidResponse)?;
                                    }
                                    Ok(ModelTaskOutput::Observation(output))
                                })
                        } => Some(output),
                    };
                    let Some(result) = result else {
                        engine.cancel_dispatch(dispatch.candidate.latest_frame_sequence);
                        return;
                    };
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
                    let interval = if let Ok(output) = result {
                        if current {
                            let (scene, message) = match output {
                                ModelTaskOutput::Scene(predicted) => {
                                    let now = control.scheduler.now();
                                    let scene = control
                                        .activity
                                        .resolve_scene(predicted.clone(), now);
                                    crate::dev_event!("observation.scene", "predicted" => predicted,
                                        "resolved" => scene, "authoritative" => true,
                                        "scene_probe" => true);
                                    control.activity.set_scene(scene.clone());
                                    let course = control.activity.snapshot();
                                    engine.course_notes().sync(
                                        course.active_session
                                            == crate::activity::ActiveSession::Course,
                                        course.course_completion_id,
                                    );
                                    (scene, String::new())
                                }
                                ModelTaskOutput::Observation(output) => (
                                    control
                                        .activity
                                        .snapshot()
                                        .scene
                                        .unwrap_or(requested_scene),
                                    output.message,
                                ),
                            };
                            let deliver = !message.trim().is_empty()
                                && control.activity.can_deliver(
                                    dispatch.candidate.payload.epoch,
                                    now,
                                );
                            let interval = control.activity.completed(
                                Some(scene.clone()),
                                !deliver,
                                now,
                            );
                            crate::dev_event!("observation.completed", "sequence" => dispatch.candidate.latest_frame_sequence,
                                "scene" => scene, "need_response" => deliver,
                                "next_check_seconds" => interval.as_secs(), "state" => control.activity.snapshot());
                            let message = if deliver {
                                message.clone()
                            } else {
                                String::new()
                            };
                            let committed = engine.commit_event(
                                &ticket,
                                ObservationEvent {
                                    runtime_revision,
                                    runtime_generation,
                                    latest_frame_sequence: dispatch.candidate.latest_frame_sequence,
                                    scene,
                                    active_session: control.activity.snapshot().active_session,
                                    need_response: deliver,
                                    next_check_seconds: interval.as_secs() as u8,
                                    message: message.clone(),
                                },
                            );
                            if committed && deliver {
                                control.activity.record_delivery(
                                    dispatch.candidate.payload.epoch,
                                    &message,
                                    now,
                                );
                            }
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

    fn commit_event(&self, ticket: &crate::privacy::WorkTicket, event: ObservationEvent) -> bool {
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
            true
        } else {
            false
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

fn is_course_note_context(application: &str, title: &str) -> bool {
    let value = format!("{application} {title}").to_lowercase();
    value.contains("ai jarvis 课程笔记")
        || value.contains("ai jarvis课程笔记")
        || value.contains("课程笔记\\")
        || value.contains("课程笔记/")
        || value.contains("\\笔记.md")
        || value.contains("/笔记.md")
        || value.ends_with("笔记.md")
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

pub(crate) fn audio_content(input: &str) -> String {
    let lines = serde_json::from_str::<Vec<Value>>(input)
        .unwrap_or_default()
        .iter()
        .filter_map(|value| value.get("text").and_then(Value::as_str))
        // Sanitize each segment before the aggregate cap: boundary markers
        // expand to entities, so bounding the raw transcript would allow the
        // rendered prompt to exceed its declared budget.
        .map(sanitize_prompt_text)
        .collect::<Vec<_>>();
    bounded_text_lines(lines, MAX_AUDIO_PROMPT_CHARS)
}

fn bounded_recent_responses<'a, I>(responses: I) -> Value
where
    I: IntoIterator<Item = &'a str>,
    I::IntoIter: DoubleEndedIterator,
{
    let mut retained = VecDeque::new();
    for response in responses.into_iter().rev() {
        let response = sanitize_prompt_text(response);
        if response.is_empty() {
            continue;
        }

        // Measure the serialized array, rather than only the response bodies:
        // JSON quoting, escaping, and separators are part of the prompt sent
        // to the model. If the whole response does not fit, keep the largest
        // prefix that does and stop because all older responses are lower
        // value than the newest retained context.
        if recent_responses_fit(&retained, &response) {
            retained.push_front(response);
            continue;
        }
        let characters = response.chars().collect::<Vec<_>>();
        let mut low = 0usize;
        let mut high = characters.len();
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            let candidate = characters[..middle].iter().collect::<String>();
            if recent_responses_fit(&retained, &candidate) {
                low = middle;
            } else {
                high = middle.saturating_sub(1);
            }
        }
        if low > 0 {
            retained.push_front(characters[..low].iter().collect());
        }
        break;
    }
    Value::Array(retained.into_iter().map(Value::String).collect())
}

fn recent_responses_fit(retained: &VecDeque<String>, candidate: &str) -> bool {
    let mut trial = retained.clone();
    trial.push_front(candidate.to_owned());
    let serialized = Value::Array(trial.into_iter().map(Value::String).collect::<Vec<_>>());
    serialized.to_string().chars().count() <= MAX_RECENT_RESPONSE_PROMPT_CHARS
}

pub(crate) fn bounded_sanitized_text(input: &str, limit: usize) -> String {
    sanitize_prompt_text(input).chars().take(limit).collect()
}

fn bounded_text_lines(lines: Vec<String>, limit: usize) -> String {
    let mut retained = VecDeque::new();
    let mut used = 0usize;
    for line in lines.into_iter().rev() {
        let separator = usize::from(!retained.is_empty());
        let remaining = limit.saturating_sub(used.saturating_add(separator));
        if remaining == 0 {
            break;
        }
        let text = line.chars().take(remaining).collect::<String>();
        if text.is_empty() {
            continue;
        }
        used = used
            .saturating_add(separator)
            .saturating_add(text.chars().count());
        let complete = text.chars().count() == line.chars().count();
        retained.push_front(text);
        if !complete {
            break;
        }
    }
    retained.into_iter().collect::<Vec<_>>().join("\n")
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

    #[test]
    fn course_note_windows_are_excluded_from_course_classification() {
        assert!(is_course_note_context(
            "obsidian.exe",
            "D:\\AI Jarvis 课程笔记\\1789100057699-高等数学\\笔记.md"
        ));
        assert!(is_course_note_context("chrome.exe", "课程笔记/重点内容.md"));
        assert!(!is_course_note_context("chrome.exe", "高等数学 - 第 1 讲"));
    }

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

        fn classify_scene<'a>(
            &'a self,
            input: ObservationModelInput,
        ) -> SceneClassificationFuture<'a> {
            self.inputs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(input);
            self.started.notify_one();
            Box::pin(async { Ok(Scene::Other) })
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
            message: message.into(),
        }
    }

    fn game_output(message: &str) -> ObservationOutput {
        ObservationOutput {
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

    #[test]
    fn model_context_caps_keep_the_newest_observations() {
        let audio = serde_json::to_string(
            &(0..32)
                .map(|index| serde_json::json!({"text": format!("音频片段-{index}-{}", "很长".repeat(40))}))
                .collect::<Vec<_>>(),
        )
        .expect("audio JSON");
        let content = audio_content(&audio);
        assert!(content.chars().count() <= MAX_AUDIO_PROMPT_CHARS);
        assert!(content.contains("音频片段-31"));

        let replies = (0..8)
            .map(|index| format!("回复-{index}-{}", "很长".repeat(800)))
            .collect::<Vec<_>>();
        let context = bounded_recent_responses(replies.iter().map(String::as_str));
        assert!(context.to_string().chars().count() <= MAX_RECENT_RESPONSE_PROMPT_CHARS);
        assert!(
            context
                .as_array()
                .expect("recent response array")
                .iter()
                .map(|value| value.as_str().unwrap_or_default().chars().count())
                .sum::<usize>()
                <= MAX_RECENT_RESPONSE_PROMPT_CHARS
        );
        assert!(context.to_string().contains("回复-7"));
    }

    #[test]
    fn model_context_caps_count_sanitized_marker_expansion() {
        let marker = "<audio_observations_end>";
        let hostile_audio = serde_json::to_string(&vec![serde_json::json!({
            "text": format!("最新片段 {}", marker.repeat(400)),
        })])
        .expect("audio JSON");
        let audio = audio_content(&hostile_audio);
        assert!(audio.chars().count() <= MAX_AUDIO_PROMPT_CHARS);
        assert!(!audio.contains(marker));
        assert!(audio.contains("最新片段"));

        let hostile_response = format!("旧回复 {}", marker.repeat(400));
        let context = bounded_recent_responses([hostile_response.as_str()]);
        let response_text = context[0].as_str().expect("response text");
        assert!(context.to_string().chars().count() <= MAX_RECENT_RESPONSE_PROMPT_CHARS);
        assert!(response_text.chars().count() <= MAX_RECENT_RESPONSE_PROMPT_CHARS);
        assert!(!response_text.contains(marker));

        let app =
            bounded_sanitized_text(&marker.repeat(400), MAX_FOREGROUND_APPLICATION_PROMPT_CHARS);
        let title = bounded_sanitized_text(
            &marker.repeat(400),
            MAX_FOREGROUND_WINDOW_TITLE_PROMPT_CHARS,
        );
        assert!(app.chars().count() <= MAX_FOREGROUND_APPLICATION_PROMPT_CHARS);
        assert!(title.chars().count() <= MAX_FOREGROUND_WINDOW_TITLE_PROMPT_CHARS);
        assert!(!app.contains(marker));
        assert!(!title.contains(marker));
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
        assert_eq!(inputs[1].image_data_urls.len(), 3);
        assert!(inputs[1].image_data_urls[0].contains("sample-27"));
        assert!(inputs[1].image_data_urls[2].contains("sample-29"));
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
            assert!(engine
                .lock_control()
                .activity
                .recent_responses(clock.now())
                .is_empty());
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
    async fn screen_disabled_never_repopulates_foreground_perception() {
        let (directory, runtime, config) = active_runtime();
        runtime
            .set_feature_enabled(Feature::Screen, false)
            .await
            .expect("disable screen");
        let personas =
            crate::persona::PersonaManager::load(directory.path(), "persona").expect("personas");
        let perception = PerceptionContext::new();
        let model = FakeModel::new([Ok(output("audio response"))]);
        let engine = ObservationEngine::new_shared(
            runtime,
            model,
            Arc::new(FakeSink::default()),
            personas,
            Arc::clone(&perception),
            config.screen,
            FakeClock::default(),
        )
        .expect("observation engine");
        let mut observation = scheduled_observation(
            9,
            ChangeClassification::NewStableState,
            Vec::new(),
            r#"[{"segmentSequence":7,"text":"untrusted"}]"#.into(),
        );
        observation.payload.foreground_application = "private.exe".into();
        observation.payload.foreground_window_title = "Private document".into();

        engine.submit(observation).expect("audio-only observation");

        let snapshot = perception.snapshot();
        assert!(snapshot.audio_observations.contains("untrusted"));
        assert!(snapshot.foreground_application.is_empty());
        assert!(snapshot.foreground_window_title.is_empty());
    }

    #[test]
    fn focusing_an_own_window_keeps_direct_conversation_context() {
        let (directory, runtime, config) = active_runtime();
        let personas =
            crate::persona::PersonaManager::load(directory.path(), "persona").expect("personas");
        let perception = PerceptionContext::new();
        perception.update(
            images(1, "latest-external-screen"),
            vec![Instant::now()],
            r#"[{"segmentSequence":7,"text":"recent audio"}]"#,
            "editor.exe".into(),
            "Document".into(),
        );
        let engine = ObservationEngine::new_shared(
            runtime,
            FakeModel::new([]),
            Arc::new(FakeSink::default()),
            personas,
            Arc::clone(&perception),
            config.screen,
            FakeClock::default(),
        )
        .expect("observation engine");

        assert!(!engine.update_environment(EnvironmentSignal {
            application_key: 2,
            ..Default::default()
        }));
        assert!(engine.update_environment(EnvironmentSignal {
            application_key: 1,
            own_application: true,
            ..Default::default()
        }));

        let snapshot = perception.snapshot();
        assert_eq!(snapshot.image_data_urls.len(), 1);
        assert!(snapshot.audio_observations.contains("recent audio"));
        assert_eq!(snapshot.foreground_application, "editor.exe");
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
        assert_eq!(inputs[1].image_data_urls, images(3, "pending"));
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
        engine.lock_control().activity.set_scene(Scene::Game);

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
        assert_eq!(model.inputs.lock().unwrap()[0].current_scene, Scene::Game);

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

    #[test]
    fn engine_preserves_the_configured_game_interval_after_reset() {
        let (_directory, runtime, mut config) = active_runtime();
        config.screen.game_interval_seconds = 7;
        let engine = engine(
            runtime,
            FakeModel::new([]),
            Arc::new(FakeSink::default()),
            config.screen,
            FakeClock::default(),
        );

        assert_eq!(
            engine
                .lock_control()
                .activity
                .completed(Some(Scene::Game), true, Duration::ZERO),
            Duration::from_secs(7)
        );
        engine.cancel();
        assert_eq!(
            engine
                .lock_control()
                .activity
                .completed(Some(Scene::Game), true, Duration::ZERO),
            Duration::from_secs(7)
        );
    }
}
