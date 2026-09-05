use std::sync::{Arc, Mutex};

use crate::{
    asr::ObservationTranscriptBuffer,
    error::{AppError, ErrorCode},
    observation::ObservationEngine,
    scheduler::MonotonicClock,
};

struct ActiveCapture {
    generation: u64,
    cancellation: tokio_util::sync::CancellationToken,
}

#[derive(Default)]
struct ControllerState {
    generation: u64,
    active: Option<ActiveCapture>,
}

impl ControllerState {
    fn begin(&mut self) -> Result<Option<(u64, tokio_util::sync::CancellationToken)>, AppError> {
        if self
            .active
            .as_ref()
            .is_some_and(|capture| !capture.cancellation.is_cancelled())
        {
            return Ok(None);
        }
        self.stop();
        self.generation = self.generation.checked_add(1).ok_or_else(|| {
            AppError::new(
                ErrorCode::Internal,
                "The screen capture generation was exhausted.",
            )
        })?;
        let cancellation = tokio_util::sync::CancellationToken::new();
        self.active = Some(ActiveCapture {
            generation: self.generation,
            cancellation: cancellation.clone(),
        });
        Ok(Some((self.generation, cancellation)))
    }

    fn stop(&mut self) {
        if let Some(capture) = self.active.take() {
            capture.cancellation.cancel();
        }
    }

    fn clear(&mut self, generation: u64) {
        if self
            .active
            .as_ref()
            .is_some_and(|capture| capture.generation == generation)
        {
            self.active.take();
        }
    }
}

struct ActiveCaptureGuard {
    state: Arc<Mutex<ControllerState>>,
    generation: u64,
}

impl Drop for ActiveCaptureGuard {
    fn drop(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear(self.generation);
    }
}

pub struct ScreenCaptureController {
    state: Arc<Mutex<ControllerState>>,
}

impl Default for ScreenCaptureController {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(ControllerState::default())),
        }
    }
}

impl ScreenCaptureController {
    pub fn stop(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stop();
    }

    #[cfg(windows)]
    pub fn start(
        &self,
        runtime: Arc<crate::state::RuntimeState>,
        observation: Arc<ObservationEngine<MonotonicClock>>,
        audio_transcripts: Arc<ObservationTranscriptBuffer>,
        config: crate::screen::ScreenRuntimeConfig,
    ) -> Result<(), AppError> {
        use std::time::{Duration, Instant};

        use base64::{engine::general_purpose::STANDARD, Engine};

        use crate::{
            observation::scheduled_observation,
            screen::{
                wgc::{WgcPrivacyRules, WgcSession},
                CapturePolicy, ChangeClassification, FrameRing,
            },
            supervisor::SupervisorError,
            types::WorkScope,
        };

        let ticket = runtime.admit(WorkScope::ScreenCapture)?;
        let mut controller_state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((generation, controller_cancellation)) = controller_state.begin()? else {
            return Ok(());
        };
        let ticket_cancellation = ticket.cancellation_token();
        let worker_cancellation = controller_cancellation.clone();
        let worker_state = Arc::clone(&self.state);
        let settings = runtime.settings();
        let privacy = WgcPrivacyRules::for_current_process(
            settings.excluded_applications.clone(),
            settings.excluded_windows.clone(),
        );
        let spawn = runtime.supervisor().spawn_scoped(
            WorkScope::ScreenCapture,
            move |supervisor_cancellation| async move {
                let _active_guard = ActiveCaptureGuard {
                    state: worker_state,
                    generation,
                };
                let Ok(session) = WgcSession::start_primary_with_privacy(
                    Duration::from_secs(config.capture_interval_seconds),
                    privacy,
                ) else {
                    return;
                };
                let policy = CapturePolicy {
                    target_monitor_id: session.monitor_id().to_owned(),
                    own_process_id: std::process::id(),
                    excluded_applications: settings.excluded_applications,
                    excluded_windows: settings.excluded_windows,
                };
                let Some(mut ring) = FrameRing::new(config.frame_ring_capacity) else {
                    return;
                };
                let started = Instant::now();
                let mut sequence = 0_u64;
                loop {
                    tokio::select! {
                        _ = ticket_cancellation.cancelled() => break,
                        _ = supervisor_cancellation.cancelled() => break,
                        _ = worker_cancellation.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                    }
                    let next_sequence = match sequence.checked_add(1) {
                        Some(value) => value,
                        None => break,
                    };
                    let frame = match session.try_latest(
                        next_sequence,
                        started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                    ) {
                        Ok(Some(frame)) => frame,
                        Ok(None) => continue,
                        Err(_) => break,
                    };
                    sequence = next_sequence;
                    let mut classification = match ring.admit(frame, &policy) {
                        Ok(classification) => classification,
                        Err(_) => continue,
                    };
                    if ring.len() < config.request_images_min {
                        continue;
                    }
                    let images = ring
                        .recent(config.request_images_max)
                        .into_iter()
                        .map(|frame| {
                            format!(
                                "data:image/png;base64,{}",
                                STANDARD.encode(&frame.encoded_image)
                            )
                        })
                        .collect();
                    let audio_observations = audio_transcripts.drain_json();
                    if !audio_observations.is_empty() {
                        classification = ChangeClassification::NewStableState;
                    }
                    let _ = observation.submit(scheduled_observation(
                        sequence,
                        classification,
                        images,
                        audio_observations,
                    ));
                }
                drop(ticket);
                let _ = session.stop();
            },
        );
        match spawn {
            Ok(_) => Ok(()),
            Err(error) => {
                controller_cancellation.cancel();
                controller_state.clear(generation);
                Err(match error {
                    SupervisorError::QueueFull => AppError::new(
                        crate::error::ErrorCode::QueueFull,
                        "The screen capture task queue is full.",
                    ),
                    SupervisorError::Closed => AppError::shutting_down(),
                    _ => AppError::invalid_argument("Screen capture could not be scheduled."),
                })
            }
        }
    }

    #[cfg(not(windows))]
    pub fn start(
        &self,
        _runtime: Arc<crate::state::RuntimeState>,
        _observation: Arc<ObservationEngine<MonotonicClock>>,
        _audio_transcripts: Arc<ObservationTranscriptBuffer>,
        _config: crate::screen::ScreenRuntimeConfig,
    ) -> Result<(), AppError> {
        Err(AppError::invalid_argument(
            "Screen capture is available only on Windows.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_start_is_idempotent_until_stopped() {
        let mut state = ControllerState::default();
        let (generation, cancellation) = state.begin().expect("first start").expect("new capture");
        assert!(state.begin().expect("duplicate start").is_none());
        state.stop();
        assert!(cancellation.is_cancelled());
        let (next_generation, _) = state
            .begin()
            .expect("restart")
            .expect("replacement capture");
        assert!(next_generation > generation);
    }

    #[test]
    fn stale_worker_completion_cannot_clear_replacement() {
        let state = Arc::new(Mutex::new(ControllerState::default()));
        let first_generation = state
            .lock()
            .unwrap()
            .begin()
            .expect("first start")
            .expect("first capture")
            .0;
        state.lock().unwrap().stop();
        let second_generation = state
            .lock()
            .unwrap()
            .begin()
            .expect("second start")
            .expect("second capture")
            .0;

        drop(ActiveCaptureGuard {
            state: Arc::clone(&state),
            generation: first_generation,
        });
        assert_eq!(
            state
                .lock()
                .unwrap()
                .active
                .as_ref()
                .map(|capture| capture.generation),
            Some(second_generation)
        );

        drop(ActiveCaptureGuard {
            state: Arc::clone(&state),
            generation: second_generation,
        });
        assert!(state.lock().unwrap().active.is_none());
    }
}
