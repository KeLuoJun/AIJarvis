use std::{
    fs,
    path::Path,
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use serde::Deserialize;
use tokio::sync::{mpsc, Mutex as AsyncMutex};

use crate::{
    audio::AudioRuntimeConfig,
    chat::ChatHistoryLimits,
    error::{AppError, ErrorCode},
    privacy::{CommitOutcome, PrivacyGate, WorkTicket},
    screen::ScreenRuntimeConfig,
    settings::{MvpSettings, SettingsStore},
    supervisor::{ShutdownReport, Supervisor, SupervisorError, SupervisorLimits},
    types::{
        ClearTransientOutputReason, ConfigDiagnostic, ConfigDiagnosticKind, ConfigSource, Feature,
        LifecycleState, PrivacyState, RuntimeSnapshot, WorkScope,
    },
};

const DEFAULTS_SCHEMA_VERSION: u32 = 1;
const MAX_DEFAULTS_BYTES: usize = 64 * 1024;
const MAX_QUEUE_CAPACITY: usize = 4096;
const MAX_SHUTDOWN_TIMEOUT_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeConfig {
    pub supervisor_limits: SupervisorLimits,
    pub shutdown_timeout: Duration,
    pub chat: ChatRuntimeConfig,
    pub screen: ScreenRuntimeConfig,
    pub audio: AudioRuntimeConfig,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChatRuntimeConfig {
    pub history_limits: ChatHistoryLimits,
    pub max_message_chars: usize,
    pub tokenize_timeout: Duration,
    pub model_timeout: Duration,
}

impl RuntimeConfig {
    pub fn load(path: &Path) -> Result<Self, AppError> {
        let contents = fs::read(path).map_err(|_| runtime_config_error())?;
        if contents.len() > MAX_DEFAULTS_BYTES {
            return Err(runtime_config_error());
        }
        let contents = std::str::from_utf8(&contents).map_err(|_| runtime_config_error())?;
        Self::parse(contents)
    }

    pub fn parse(contents: &str) -> Result<Self, AppError> {
        let document =
            toml::from_str::<DefaultsDocument>(contents).map_err(|_| runtime_config_error())?;
        let limits = SupervisorLimits {
            frame_ring_capacity: document.screen.frame_ring_capacity,
            observation_queue_capacity: document.screen.pending_observation_capacity,
            direct_qwen_queue_capacity: document.qwen.direct_queue_capacity,
            system_audio_segment_capacity: document.audio.system_audio_segment_capacity,
            user_voice_queue_capacity: document.audio.user_voice_queue_capacity,
            system_audio_queue_capacity: document.audio.system_audio_queue_capacity,
        };
        let screen = ScreenRuntimeConfig {
            capture_interval_seconds: document.screen.capture_interval_seconds,
            frame_ring_capacity: document.screen.frame_ring_capacity,
            request_images_min: document.screen.request_images_min,
            request_images_max: document.screen.request_images_max,
            observation_min_interval_seconds: document.screen.observation_min_interval_seconds,
            next_check_min_seconds: document.screen.next_check_min_seconds,
            next_check_max_seconds: document.screen.next_check_max_seconds,
            next_check_default_seconds: document.screen.next_check_default_seconds,
            game_interval_seconds: document.screen.game_interval_seconds,
            media_interval_seconds: document.screen.media_interval_seconds,
            default_interval_seconds: document.screen.default_interval_seconds,
            pending_observation_capacity: document.screen.pending_observation_capacity,
        };
        let audio = AudioRuntimeConfig {
            sample_rate_hz: document.audio.sample_rate_hz,
            channels: document.audio.channels,
            microphone_max_seconds: document.audio.microphone_max_seconds,
            microphone_max_bytes: document.audio.microphone_max_bytes,
            system_audio_ring_seconds: document.audio.system_audio_ring_seconds,
            system_audio_segment_capacity: document.audio.system_audio_segment_capacity,
            user_voice_queue_capacity: document.audio.user_voice_queue_capacity,
            system_audio_queue_capacity: document.audio.system_audio_queue_capacity,
        };

        if document.schema_version != DEFAULTS_SCHEMA_VERSION
            || document.qwen.max_concurrency != 1
            || !screen.is_valid()
            || !audio.is_valid()
            || !limits.all_non_zero()
            || capacities(limits).any(|capacity| capacity > MAX_QUEUE_CAPACITY)
            || document.runtime.shutdown_timeout_ms == 0
            || document.runtime.shutdown_timeout_ms > MAX_SHUTDOWN_TIMEOUT_MS
            || !(0.0..=1.0).contains(&document.chat.evict_at_context_ratio)
            || !(0.0..document.chat.evict_at_context_ratio)
                .contains(&document.chat.evict_to_context_ratio)
            || document.chat.available_context_tokens == 0
            || document.chat.max_message_chars == 0
            || document.chat.tokenize_timeout_ms == 0
            || document.chat.model_timeout_ms == 0
        {
            return Err(runtime_config_error());
        }

        Ok(Self {
            supervisor_limits: limits,
            shutdown_timeout: Duration::from_millis(document.runtime.shutdown_timeout_ms),
            chat: ChatRuntimeConfig {
                history_limits: ChatHistoryLimits {
                    available_tokens: document.chat.available_context_tokens,
                    evict_at_per_mille: ratio_to_per_mille(document.chat.evict_at_context_ratio),
                    evict_to_per_mille: ratio_to_per_mille(document.chat.evict_to_context_ratio),
                },
                max_message_chars: document.chat.max_message_chars,
                tokenize_timeout: Duration::from_millis(document.chat.tokenize_timeout_ms),
                model_timeout: Duration::from_millis(document.chat.model_timeout_ms),
            },
            screen,
            audio,
        })
    }
}

#[derive(Debug, Deserialize)]
struct DefaultsDocument {
    schema_version: u32,
    screen: ScreenDefaults,
    qwen: QwenDefaults,
    audio: AudioDefaults,
    runtime: RuntimeDefaults,
    chat: ChatDefaults,
}

#[derive(Debug, Deserialize)]
struct ScreenDefaults {
    capture_interval_seconds: u64,
    frame_ring_capacity: usize,
    request_images_min: usize,
    request_images_max: usize,
    observation_min_interval_seconds: u64,
    next_check_min_seconds: u64,
    next_check_max_seconds: u64,
    next_check_default_seconds: u64,
    game_interval_seconds: u64,
    media_interval_seconds: u64,
    default_interval_seconds: u64,
    pending_observation_capacity: usize,
}

#[derive(Debug, Deserialize)]
struct QwenDefaults {
    max_concurrency: usize,
    direct_queue_capacity: usize,
}

#[derive(Debug, Deserialize)]
struct AudioDefaults {
    sample_rate_hz: u32,
    channels: u16,
    microphone_max_seconds: u64,
    microphone_max_bytes: usize,
    system_audio_ring_seconds: u64,
    system_audio_segment_capacity: usize,
    user_voice_queue_capacity: usize,
    system_audio_queue_capacity: usize,
}

#[derive(Debug, Deserialize)]
struct RuntimeDefaults {
    shutdown_timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
struct ChatDefaults {
    available_context_tokens: usize,
    evict_at_context_ratio: f64,
    evict_to_context_ratio: f64,
    max_message_chars: usize,
    tokenize_timeout_ms: u64,
    model_timeout_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeTransition {
    pub snapshot: RuntimeSnapshot,
    pub clear_transient_output: Option<ClearTransientOutputReason>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeTransitionFailure {
    pub error: AppError,
    pub transition: RuntimeTransition,
}

pub type RuntimeTransitionResult = Result<RuntimeTransition, RuntimeTransitionFailure>;

struct RuntimeMetadata {
    revision: u64,
    settings: MvpSettings,
    diagnostics: Vec<ConfigDiagnostic>,
    startup_state_dirty: bool,
    settings_dirty: bool,
}

pub struct RuntimeState {
    gate: PrivacyGate,
    supervisor: Supervisor,
    store: SettingsStore,
    config: RuntimeConfig,
    metadata: Mutex<RuntimeMetadata>,
    transition_lock: AsyncMutex<()>,
}

impl RuntimeState {
    pub fn load(config_dir: impl AsRef<Path>, config: RuntimeConfig) -> Result<Self, AppError> {
        let store = SettingsStore::new(config_dir.as_ref());
        let loaded = store.load();
        let supervisor = Supervisor::new(config.supervisor_limits).map_err(map_supervisor_error)?;

        Ok(Self {
            gate: PrivacyGate::new(loaded.privacy_state, loaded.settings.features),
            supervisor,
            store,
            config,
            metadata: Mutex::new(RuntimeMetadata {
                revision: 0,
                settings: loaded.settings,
                diagnostics: loaded.diagnostics,
                startup_state_dirty: false,
                settings_dirty: false,
            }),
            transition_lock: AsyncMutex::new(()),
        })
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        let mut snapshot = self.gate.snapshot();
        let metadata = self.lock_metadata();
        snapshot.revision = metadata.revision;
        snapshot.diagnostics.clone_from(&metadata.diagnostics);
        snapshot
    }

    pub fn settings(&self) -> MvpSettings {
        self.lock_metadata().settings.clone()
    }

    pub fn supervisor(&self) -> &Supervisor {
        &self.supervisor
    }

    pub fn admit(&self, scope: WorkScope) -> Result<WorkTicket, AppError> {
        self.gate.admit(scope)
    }

    pub fn try_commit<T>(
        &self,
        ticket: &WorkTicket,
        sender: &mpsc::Sender<T>,
        value: T,
    ) -> CommitOutcome<T> {
        self.gate.try_commit(ticket, sender, value)
    }

    pub async fn set_privacy_paused(&self, paused: bool) -> RuntimeTransitionResult {
        let _transition = self.transition_lock.lock().await;
        let before = self.snapshot();
        let desired = if paused {
            PrivacyState::Paused
        } else {
            PrivacyState::Active
        };
        if before.privacy_state == desired && !self.persistence_is_dirty(ConfigSource::StartupState)
        {
            return Ok(RuntimeTransition {
                snapshot: before,
                clear_transient_output: None,
            });
        }
        if before.lifecycle_state != LifecycleState::Running {
            return Err(self.failure(AppError::shutting_down(), None));
        }
        let next_revision = self
            .next_revision()
            .map_err(|error| self.failure(error, None))?;

        if before.privacy_state == desired {
            let persistence = self.store.save_startup_state(desired);
            self.finish_persistence(
                next_revision,
                ConfigSource::StartupState,
                persistence.as_ref().err(),
                None,
            );
            let transition = RuntimeTransition {
                snapshot: self.snapshot(),
                clear_transient_output: None,
            };
            return persistence
                .map(|()| transition.clone())
                .map_err(|error| RuntimeTransitionFailure { error, transition });
        }

        if paused {
            if let Err(error) = self.gate.pause() {
                return Err(self.failure(error, None));
            }
            self.supervisor.clear_all();
            let persistence = self.store.save_startup_state(PrivacyState::Paused);
            self.finish_persistence(
                next_revision,
                ConfigSource::StartupState,
                persistence.as_ref().err(),
                None,
            );
            let transition = RuntimeTransition {
                snapshot: self.snapshot(),
                clear_transient_output: Some(ClearTransientOutputReason::PrivacyPaused),
            };
            return persistence
                .map(|()| transition.clone())
                .map_err(|error| RuntimeTransitionFailure { error, transition });
        }

        if let Err(error) = self.store.save_startup_state(PrivacyState::Active) {
            self.finish_persistence(
                next_revision,
                ConfigSource::StartupState,
                Some(&error),
                None,
            );
            return Err(self.failure(error, None));
        }
        if let Err(error) = self.gate.resume() {
            let rollback = self.store.save_startup_state(PrivacyState::Paused).err();
            if let Some(rollback_error) = rollback.as_ref() {
                self.finish_persistence(
                    next_revision,
                    ConfigSource::StartupState,
                    Some(rollback_error),
                    None,
                );
            }
            return Err(self.failure(error, None));
        }
        self.finish_persistence(next_revision, ConfigSource::StartupState, None, None);
        Ok(RuntimeTransition {
            snapshot: self.snapshot(),
            clear_transient_output: None,
        })
    }

    pub async fn set_feature_enabled(
        &self,
        feature: Feature,
        enabled: bool,
    ) -> RuntimeTransitionResult {
        let _transition = self.transition_lock.lock().await;
        let before = self.snapshot();
        if before.features.is_enabled(feature) == enabled
            && !self.persistence_is_dirty(ConfigSource::Settings)
        {
            return Ok(RuntimeTransition {
                snapshot: before,
                clear_transient_output: None,
            });
        }
        if before.lifecycle_state != LifecycleState::Running {
            return Err(self.failure(AppError::shutting_down(), None));
        }
        let next_revision = self
            .next_revision()
            .map_err(|error| self.failure(error, None))?;
        let previous_settings = self.settings();
        let requested_settings = MvpSettings {
            features: previous_settings.features.with_feature(feature, enabled),
            excluded_applications: previous_settings.excluded_applications.clone(),
            excluded_windows: previous_settings.excluded_windows.clone(),
        };

        if before.features.is_enabled(feature) == enabled {
            let persistence = self.store.save_settings(requested_settings);
            let current_settings = persistence.as_ref().ok().cloned();
            self.finish_persistence(
                next_revision,
                ConfigSource::Settings,
                persistence.as_ref().err(),
                current_settings,
            );
            let transition = RuntimeTransition {
                snapshot: self.snapshot(),
                clear_transient_output: None,
            };
            return persistence
                .map(|_| transition.clone())
                .map_err(|error| RuntimeTransitionFailure { error, transition });
        }

        if enabled {
            let persisted = match self.store.save_settings(requested_settings) {
                Ok(settings) => settings,
                Err(error) => {
                    self.finish_persistence(
                        next_revision,
                        ConfigSource::Settings,
                        Some(&error),
                        None,
                    );
                    return Err(self.failure(error, None));
                }
            };
            if let Err(error) = self.gate.set_feature(feature, true) {
                if let Err(rollback_error) = self.store.save_settings(previous_settings) {
                    self.finish_persistence(
                        next_revision,
                        ConfigSource::Settings,
                        Some(&rollback_error),
                        None,
                    );
                }
                return Err(self.failure(error, None));
            }
            self.finish_persistence(next_revision, ConfigSource::Settings, None, Some(persisted));
            return Ok(RuntimeTransition {
                snapshot: self.snapshot(),
                clear_transient_output: None,
            });
        }

        if let Err(error) = self.gate.set_feature(feature, false) {
            return Err(self.failure(error, None));
        }
        self.supervisor.clear_for_feature(feature);
        let persistence = self.store.save_settings(requested_settings.clone());
        let current_settings = persistence.as_ref().cloned().unwrap_or(requested_settings);
        self.finish_persistence(
            next_revision,
            ConfigSource::Settings,
            persistence.as_ref().err(),
            Some(current_settings),
        );
        let transition = RuntimeTransition {
            snapshot: self.snapshot(),
            clear_transient_output: Some(clear_reason(feature)),
        };
        persistence
            .map(|_| transition.clone())
            .map_err(|error| RuntimeTransitionFailure { error, transition })
    }

    pub async fn shutdown(&self) -> RuntimeTransitionResult {
        let _transition = self.transition_lock.lock().await;
        let before = self.snapshot();
        if before.lifecycle_state == LifecycleState::Stopped {
            return Ok(RuntimeTransition {
                snapshot: before,
                clear_transient_output: None,
            });
        }
        let next_revision = self
            .next_revision()
            .map_err(|error| self.failure(error, None))?;
        if before.lifecycle_state == LifecycleState::Running {
            if let Err(error) = self.gate.begin_shutdown() {
                return Err(self.failure(error, None));
            }
        }

        let _report: ShutdownReport = self.supervisor.shutdown(self.config.shutdown_timeout).await;
        if let Err(error) = self.gate.finish_shutdown() {
            return Err(self.failure(error, Some(ClearTransientOutputReason::Shutdown)));
        }
        self.lock_metadata().revision = next_revision;
        Ok(RuntimeTransition {
            snapshot: self.snapshot(),
            clear_transient_output: Some(ClearTransientOutputReason::Shutdown),
        })
    }

    fn next_revision(&self) -> Result<u64, AppError> {
        self.lock_metadata()
            .revision
            .checked_add(1)
            .ok_or_else(AppError::generation_exhausted)
    }

    fn finish_persistence(
        &self,
        revision: u64,
        source: ConfigSource,
        error: Option<&AppError>,
        settings: Option<MvpSettings>,
    ) {
        let mut metadata = self.lock_metadata();
        metadata.revision = revision;
        if let Some(settings) = settings {
            metadata.settings = settings;
        }
        metadata
            .diagnostics
            .retain(|diagnostic| diagnostic.source != source);
        if error.is_some() {
            metadata.diagnostics.push(ConfigDiagnostic::new(
                source,
                ConfigDiagnosticKind::WriteFailed,
                match source {
                    ConfigSource::StartupState => {
                        "Privacy startup state could not be saved; the runtime remains in its safer state."
                    }
                    ConfigSource::Settings => {
                        "Settings could not be saved; the runtime remains in its safer state."
                    }
                },
            ));
        }
        match source {
            ConfigSource::StartupState => metadata.startup_state_dirty = error.is_some(),
            ConfigSource::Settings => metadata.settings_dirty = error.is_some(),
        }
    }

    fn persistence_is_dirty(&self, source: ConfigSource) -> bool {
        let metadata = self.lock_metadata();
        match source {
            ConfigSource::StartupState => metadata.startup_state_dirty,
            ConfigSource::Settings => metadata.settings_dirty,
        }
    }

    fn failure(
        &self,
        error: AppError,
        clear_transient_output: Option<ClearTransientOutputReason>,
    ) -> RuntimeTransitionFailure {
        RuntimeTransitionFailure {
            error,
            transition: RuntimeTransition {
                snapshot: self.snapshot(),
                clear_transient_output,
            },
        }
    }

    fn lock_metadata(&self) -> MutexGuard<'_, RuntimeMetadata> {
        self.metadata
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn capacities(limits: SupervisorLimits) -> impl Iterator<Item = usize> {
    [
        limits.frame_ring_capacity,
        limits.observation_queue_capacity,
        limits.direct_qwen_queue_capacity,
        limits.system_audio_segment_capacity,
        limits.user_voice_queue_capacity,
        limits.system_audio_queue_capacity,
    ]
    .into_iter()
}

fn ratio_to_per_mille(value: f64) -> usize {
    (value * 1_000.0).round() as usize
}

fn clear_reason(feature: Feature) -> ClearTransientOutputReason {
    match feature {
        Feature::Screen => ClearTransientOutputReason::ScreenDisabled,
        Feature::SystemAudio => ClearTransientOutputReason::SystemAudioDisabled,
        Feature::ProactiveResponse => ClearTransientOutputReason::ProactiveResponseDisabled,
    }
}

fn runtime_config_error() -> AppError {
    AppError::new(
        ErrorCode::ConfigRead,
        "Runtime defaults are missing or invalid.",
    )
}

fn map_supervisor_error(error: SupervisorError) -> AppError {
    let message = match error {
        SupervisorError::InvalidLimits => "Runtime queue limits are invalid.",
        SupervisorError::RuntimeUnavailable => "The asynchronous runtime is unavailable.",
        SupervisorError::QueueFull => "A bounded runtime queue is full.",
        SupervisorError::InvalidScope => "The work scope is invalid for this runtime lane.",
        SupervisorError::TaskIdExhausted => "The runtime task identifier is exhausted.",
        SupervisorError::Closed => "The runtime supervisor is closed.",
    };
    AppError::new(ErrorCode::Internal, message)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    const VALID_DEFAULTS: &str = r#"
schema_version = 1
[screen]
capture_interval_seconds = 3
frame_ring_capacity = 8
request_images_min = 1
request_images_max = 3
observation_min_interval_seconds = 5
next_check_min_seconds = 5
next_check_max_seconds = 18
next_check_default_seconds = 10
game_interval_seconds = 3
media_interval_seconds = 6
default_interval_seconds = 10
pending_observation_capacity = 1
[qwen]
max_concurrency = 1
direct_queue_capacity = 4
[audio]
sample_rate_hz = 16000
channels = 1
microphone_max_seconds = 60
microphone_max_bytes = 2097152
system_audio_ring_seconds = 60
system_audio_segment_capacity = 20
user_voice_queue_capacity = 1
system_audio_queue_capacity = 8
[runtime]
shutdown_timeout_ms = 2000
[chat]
available_context_tokens = 6144
evict_at_context_ratio = 0.8
evict_to_context_ratio = 0.7
max_message_chars = 4000
tokenize_timeout_ms = 5000
model_timeout_ms = 120000
"#;

    fn config() -> RuntimeConfig {
        RuntimeConfig::parse(VALID_DEFAULTS).expect("valid runtime defaults")
    }

    fn active_runtime(directory: &Path) -> RuntimeState {
        let store = SettingsStore::new(directory);
        store
            .save_startup_state(PrivacyState::Active)
            .expect("active startup state");
        store
            .save_settings(MvpSettings {
                features: crate::types::FeatureSettings {
                    screen_enabled: true,
                    system_audio_enabled: true,
                    proactive_response_enabled: true,
                },
                excluded_applications: Vec::new(),
                excluded_windows: Vec::new(),
            })
            .expect("enabled settings");
        RuntimeState::load(directory, config()).expect("runtime")
    }

    #[test]
    fn runtime_defaults_are_structured_and_bounded() {
        let parsed = config();
        assert_eq!(parsed.supervisor_limits.frame_ring_capacity, 8);
        assert_eq!(parsed.supervisor_limits.direct_qwen_queue_capacity, 4);
        assert_eq!(parsed.shutdown_timeout, Duration::from_secs(2));
        assert_eq!(parsed.screen.request_images_min, 1);
        assert_eq!(parsed.screen.request_images_max, 3);
        assert_eq!(parsed.screen.next_check_max_seconds, 18);
        assert_eq!(parsed.audio.microphone_sample_limit(), 960_000);
        assert_eq!(parsed.audio.system_audio_sample_limit(), 960_000);

        assert!(RuntimeConfig::parse(
            &VALID_DEFAULTS.replace("direct_queue_capacity = 4", "direct_queue_capacity = 0")
        )
        .is_err());
        assert!(RuntimeConfig::parse(
            &VALID_DEFAULTS.replace("microphone_max_seconds = 60", "microphone_max_seconds = 61")
        )
        .is_err());
        assert!(RuntimeConfig::parse(
            &VALID_DEFAULTS.replace("max_concurrency = 1", "max_concurrency = 2")
        )
        .is_err());
        assert!(RuntimeConfig::parse(
            &VALID_DEFAULTS.replace("shutdown_timeout_ms = 2000", "shutdown_timeout_ms = 30001")
        )
        .is_err());
        assert!(RuntimeConfig::parse(
            &VALID_DEFAULTS.replace("request_images_max = 3", "request_images_max = 4")
        )
        .is_err());
    }

    #[test]
    fn missing_user_configuration_starts_fail_closed() {
        let directory = tempdir().expect("temporary directory");
        let runtime = RuntimeState::load(directory.path(), config()).expect("runtime");
        let snapshot = runtime.snapshot();

        assert_eq!(snapshot.privacy_state, PrivacyState::Paused);
        assert_eq!(snapshot.features, crate::types::FeatureSettings::default());
        assert_eq!(snapshot.diagnostics.len(), 2);
    }

    #[tokio::test]
    async fn pause_remains_effective_when_persistence_fails() {
        let parent = tempdir().expect("temporary parent");
        let config_dir = parent.path().join("config");
        fs::create_dir(&config_dir).expect("config directory");
        let runtime = active_runtime(&config_dir);
        fs::remove_dir_all(&config_dir).expect("remove config directory");
        fs::write(&config_dir, b"blocks-directory-recreation").expect("blocking file");

        let failure = runtime
            .set_privacy_paused(true)
            .await
            .expect_err("persistence must fail");

        assert_eq!(
            failure.transition.snapshot.privacy_state,
            PrivacyState::Paused
        );
        assert_eq!(
            failure.transition.clear_transient_output,
            Some(ClearTransientOutputReason::PrivacyPaused)
        );
        assert!(failure
            .transition
            .snapshot
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind == ConfigDiagnosticKind::WriteFailed));
        assert!(runtime.admit(WorkScope::Direct).is_err());

        fs::remove_file(&config_dir).expect("remove blocking file");
        fs::create_dir(&config_dir).expect("restore config directory");
        let retry = runtime
            .set_privacy_paused(true)
            .await
            .expect("retry the dirty paused state");
        assert_eq!(retry.snapshot.privacy_state, PrivacyState::Paused);
        assert!(!retry.snapshot.diagnostics.iter().any(|diagnostic| {
            diagnostic.source == ConfigSource::StartupState
                && diagnostic.kind == ConfigDiagnosticKind::WriteFailed
        }));
        assert_eq!(
            SettingsStore::new(&config_dir).load().privacy_state,
            PrivacyState::Paused
        );
    }

    #[tokio::test]
    async fn disabled_feature_retries_its_dirty_safe_state() {
        let parent = tempdir().expect("temporary parent");
        let config_dir = parent.path().join("config");
        fs::create_dir(&config_dir).expect("config directory");
        let runtime = active_runtime(&config_dir);
        fs::remove_dir_all(&config_dir).expect("remove config directory");
        fs::write(&config_dir, b"blocks-directory-recreation").expect("blocking file");

        let failure = runtime
            .set_feature_enabled(Feature::Screen, false)
            .await
            .expect_err("persistence must fail");
        assert!(!failure.transition.snapshot.features.screen_enabled);
        assert!(failure
            .transition
            .snapshot
            .diagnostics
            .iter()
            .any(|diagnostic| {
                diagnostic.source == ConfigSource::Settings
                    && diagnostic.kind == ConfigDiagnosticKind::WriteFailed
            }));

        fs::remove_file(&config_dir).expect("remove blocking file");
        fs::create_dir(&config_dir).expect("restore config directory");
        let retry = runtime
            .set_feature_enabled(Feature::Screen, false)
            .await
            .expect("retry the dirty disabled state");
        assert!(!retry.snapshot.features.screen_enabled);
        assert!(!retry.snapshot.diagnostics.iter().any(|diagnostic| {
            diagnostic.source == ConfigSource::Settings
                && diagnostic.kind == ConfigDiagnosticKind::WriteFailed
        }));
        assert!(
            !SettingsStore::new(&config_dir)
                .load()
                .settings
                .features
                .screen_enabled
        );
    }

    #[tokio::test]
    async fn enabling_does_not_open_admission_when_persistence_fails() {
        let parent = tempdir().expect("temporary parent");
        let config_dir = parent.path().join("config");
        fs::create_dir(&config_dir).expect("config directory");
        let store = SettingsStore::new(&config_dir);
        store
            .save_startup_state(PrivacyState::Active)
            .expect("active state");
        store
            .save_settings(MvpSettings::default())
            .expect("disabled settings");
        let runtime = RuntimeState::load(&config_dir, config()).expect("runtime");
        fs::remove_dir_all(&config_dir).expect("remove config directory");
        fs::write(&config_dir, b"blocks-directory-recreation").expect("blocking file");

        runtime
            .set_feature_enabled(Feature::Screen, true)
            .await
            .expect_err("persistence must fail");

        assert!(!runtime.snapshot().features.screen_enabled);
        assert!(runtime.admit(WorkScope::ScreenCapture).is_err());
    }

    #[tokio::test]
    async fn shutdown_is_idempotent_and_finishes_stopped() {
        let directory = tempdir().expect("temporary directory");
        let runtime = active_runtime(directory.path());

        let first = runtime.shutdown().await.expect("first shutdown");
        let second = runtime.shutdown().await.expect("second shutdown");

        assert_eq!(first.snapshot.lifecycle_state, LifecycleState::Stopped);
        assert_eq!(
            first.clear_transient_output,
            Some(ClearTransientOutputReason::Shutdown)
        );
        assert_eq!(second.snapshot.lifecycle_state, LifecycleState::Stopped);
        assert_eq!(second.clear_transient_output, None);
    }
}
