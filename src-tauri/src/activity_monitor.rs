use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::{
    observation::ObservationEngine,
    scheduler::MonotonicClock,
    state::RuntimeState,
    types::{LifecycleState, PrivacyState, WorkScope},
};

/// Owns only a timer while sensing is disabled. No OS queries or inference during pause.
pub struct ActivityMonitor(Mutex<Option<tokio::task::JoinHandle<()>>>);

impl ActivityMonitor {
    pub fn start(
        runtime: &Arc<RuntimeState>,
        observation: &Arc<ObservationEngine<MonotonicClock>>,
        executor: &tokio::runtime::Handle,
    ) -> Self {
        let runtime = Arc::downgrade(runtime);
        let observation = Arc::downgrade(observation);
        let task = executor.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let (Some(runtime), Some(observation)) = (runtime.upgrade(), observation.upgrade())
                else {
                    break;
                };
                let snapshot = runtime.snapshot();
                if snapshot.lifecycle_state != LifecycleState::Running {
                    break;
                }
                if snapshot.privacy_state != PrivacyState::Active {
                    continue;
                }
                let scope = if snapshot.features.screen_enabled {
                    WorkScope::ScreenCapture
                } else if snapshot.features.system_audio_enabled {
                    WorkScope::SystemAudioCapture
                } else {
                    continue;
                };
                let Ok(ticket) = runtime.admit(scope) else {
                    continue;
                };
                #[cfg(windows)]
                {
                    let settings = runtime.settings();
                    let privacy = crate::screen::wgc::WgcPrivacyRules::for_current_process(
                        settings.excluded_applications,
                        settings.excluded_windows,
                    );
                    let signal = crate::screen::wgc::environment_signal(&privacy);
                    if !ticket.cancellation_token().is_cancelled() {
                        observation.update_environment(signal);
                    }
                }
                #[cfg(not(windows))]
                let _ = (ticket, observation);
            }
        });
        Self(Mutex::new(Some(task)))
    }

    pub fn stop(&self) {
        if let Some(task) = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }
}

impl Drop for ActivityMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}
