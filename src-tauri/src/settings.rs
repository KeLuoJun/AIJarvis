use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tempfile::Builder;

use crate::{
    error::AppError,
    types::{ConfigDiagnostic, ConfigDiagnosticKind, ConfigSource, FeatureSettings, PrivacyState},
};

const STARTUP_STATE_FILE: &str = "startup-state.json";
const SETTINGS_FILE: &str = "settings.json";
const SCHEMA_VERSION: u32 = 1;
const MAX_CONFIG_BYTES: usize = 64 * 1024;
const MAX_EXCLUSIONS_PER_KIND: usize = 64;
const MAX_EXCLUSION_CHARS: usize = 256;
const WINDOWS_PERSIST_RETRY_LIMIT: usize = 64;
const WINDOWS_PERSIST_RETRY_DELAY: Duration = Duration::from_millis(2);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MvpSettings {
    pub features: FeatureSettings,
    pub excluded_applications: Vec<String>,
    pub excluded_windows: Vec<String>,
}

impl Default for MvpSettings {
    fn default() -> Self {
        Self {
            features: FeatureSettings {
                screen_enabled: false,
                system_audio_enabled: false,
                proactive_response_enabled: false,
            },
            excluded_applications: Vec::new(),
            excluded_windows: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedSettings {
    pub privacy_state: PrivacyState,
    pub settings: MvpSettings,
    pub diagnostics: Vec<ConfigDiagnostic>,
}

#[derive(Clone, Debug)]
pub struct SettingsStore {
    config_dir: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl SettingsStore {
    pub fn new(config_dir: impl Into<PathBuf>) -> Self {
        Self {
            config_dir: config_dir.into(),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn load(&self) -> LoadedSettings {
        let (privacy_state, startup_diagnostic) =
            load_startup_file(&self.config_dir.join(STARTUP_STATE_FILE));
        let (settings, settings_diagnostic) =
            load_settings_file(&self.config_dir.join(SETTINGS_FILE));
        let diagnostics = [startup_diagnostic, settings_diagnostic]
            .into_iter()
            .flatten()
            .collect();

        LoadedSettings {
            privacy_state,
            settings,
            diagnostics,
        }
    }

    pub fn save_startup_state(&self, privacy_state: PrivacyState) -> Result<(), AppError> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let persisted = StartupStateFile {
            schema_version: SCHEMA_VERSION,
            privacy_paused: matches!(privacy_state, PrivacyState::Paused),
        };

        atomic_json_write(
            &self.config_dir,
            STARTUP_STATE_FILE,
            &persisted,
            ConfigSource::StartupState,
        )
    }

    pub fn save_settings(&self, settings: MvpSettings) -> Result<MvpSettings, AppError> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let normalized = normalize_settings(settings)
            .map_err(|()| AppError::config_write(ConfigSource::Settings))?;
        let persisted = SettingsFile::from(&normalized);

        atomic_json_write(
            &self.config_dir,
            SETTINGS_FILE,
            &persisted,
            ConfigSource::Settings,
        )?;

        Ok(normalized)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StartupStateFile {
    schema_version: u32,
    privacy_paused: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SettingsFile {
    schema_version: u32,
    screen_enabled: bool,
    system_audio_enabled: bool,
    proactive_response_enabled: bool,
    excluded_applications: Vec<String>,
    excluded_windows: Vec<String>,
}

impl From<&MvpSettings> for SettingsFile {
    fn from(settings: &MvpSettings) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            screen_enabled: settings.features.screen_enabled,
            system_audio_enabled: settings.features.system_audio_enabled,
            proactive_response_enabled: settings.features.proactive_response_enabled,
            excluded_applications: settings.excluded_applications.clone(),
            excluded_windows: settings.excluded_windows.clone(),
        }
    }
}

enum ConfigFile {
    Missing,
    Invalid,
    ReadFailed,
    Contents(Vec<u8>),
}

fn read_config_file(path: &Path) -> ConfigFile {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return ConfigFile::Missing,
        Err(_) => return ConfigFile::ReadFailed,
    };
    let mut contents = Vec::new();
    if file
        .take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut contents)
        .is_err()
    {
        return ConfigFile::ReadFailed;
    }
    if contents.len() > MAX_CONFIG_BYTES {
        ConfigFile::Invalid
    } else {
        ConfigFile::Contents(contents)
    }
}

fn load_startup_file(path: &Path) -> (PrivacyState, Option<ConfigDiagnostic>) {
    let diagnostic =
        |kind, message| ConfigDiagnostic::new(ConfigSource::StartupState, kind, message);

    match read_config_file(path) {
        ConfigFile::Missing => (
            PrivacyState::Paused,
            Some(diagnostic(
                ConfigDiagnosticKind::Missing,
                "Privacy startup state is missing; privacy remains paused.",
            )),
        ),
        ConfigFile::Invalid => (
            PrivacyState::Paused,
            Some(diagnostic(
                ConfigDiagnosticKind::Invalid,
                "Privacy startup state is invalid; privacy remains paused.",
            )),
        ),
        ConfigFile::ReadFailed => (
            PrivacyState::Paused,
            Some(diagnostic(
                ConfigDiagnosticKind::ReadFailed,
                "Privacy startup state could not be read; privacy remains paused.",
            )),
        ),
        ConfigFile::Contents(contents) => {
            match serde_json::from_slice::<StartupStateFile>(&contents) {
                Ok(file) if file.schema_version == SCHEMA_VERSION => {
                    let state = if file.privacy_paused {
                        PrivacyState::Paused
                    } else {
                        PrivacyState::Active
                    };
                    (state, None)
                }
                _ => (
                    PrivacyState::Paused,
                    Some(diagnostic(
                        ConfigDiagnosticKind::Invalid,
                        "Privacy startup state is invalid; privacy remains paused.",
                    )),
                ),
            }
        }
    }
}

fn load_settings_file(path: &Path) -> (MvpSettings, Option<ConfigDiagnostic>) {
    let diagnostic = |kind, message| ConfigDiagnostic::new(ConfigSource::Settings, kind, message);
    let fallback = || MvpSettings::default();

    match read_config_file(path) {
        ConfigFile::Missing => (
            fallback(),
            Some(diagnostic(
                ConfigDiagnosticKind::Missing,
                "Settings are missing; all optional features remain disabled.",
            )),
        ),
        ConfigFile::Invalid => (
            fallback(),
            Some(diagnostic(
                ConfigDiagnosticKind::Invalid,
                "Settings are invalid; all optional features remain disabled.",
            )),
        ),
        ConfigFile::ReadFailed => (
            fallback(),
            Some(diagnostic(
                ConfigDiagnosticKind::ReadFailed,
                "Settings could not be read; all optional features remain disabled.",
            )),
        ),
        ConfigFile::Contents(contents) => {
            let parsed = serde_json::from_slice::<SettingsFile>(&contents)
                .ok()
                .filter(|file| file.schema_version == SCHEMA_VERSION)
                .map(|file| MvpSettings {
                    features: FeatureSettings {
                        screen_enabled: file.screen_enabled,
                        system_audio_enabled: file.system_audio_enabled,
                        proactive_response_enabled: file.proactive_response_enabled,
                    },
                    excluded_applications: file.excluded_applications,
                    excluded_windows: file.excluded_windows,
                })
                .and_then(|settings| normalize_settings(settings).ok());

            match parsed {
                Some(settings) => (settings, None),
                None => (
                    fallback(),
                    Some(diagnostic(
                        ConfigDiagnosticKind::Invalid,
                        "Settings are invalid; all optional features remain disabled.",
                    )),
                ),
            }
        }
    }
}

fn normalize_settings(mut settings: MvpSettings) -> Result<MvpSettings, ()> {
    settings.excluded_applications = normalize_exclusions(settings.excluded_applications)?;
    settings.excluded_windows = normalize_exclusions(settings.excluded_windows)?;
    Ok(settings)
}

fn normalize_exclusions(entries: Vec<String>) -> Result<Vec<String>, ()> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();

    for entry in entries {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.chars().count() > MAX_EXCLUSION_CHARS {
            return Err(());
        }
        if seen.insert(trimmed.to_owned()) {
            if normalized.len() == MAX_EXCLUSIONS_PER_KIND {
                return Err(());
            }
            normalized.push(trimmed.to_owned());
        }
    }

    Ok(normalized)
}

fn atomic_json_write<T: Serialize>(
    config_dir: &Path,
    file_name: &str,
    value: &T,
    source: ConfigSource,
) -> Result<(), AppError> {
    atomic_json_write_with_hook(config_dir, file_name, value, source, || Ok(()))
}

fn atomic_json_write_with_hook<T: Serialize, F: FnOnce() -> io::Result<()>>(
    config_dir: &Path,
    file_name: &str,
    value: &T,
    source: ConfigSource,
    before_persist: F,
) -> Result<(), AppError> {
    let serialized =
        serde_json::to_vec_pretty(value).map_err(|_| AppError::config_write(source))?;
    if serialized.len().saturating_add(1) > MAX_CONFIG_BYTES {
        return Err(AppError::config_write(source));
    }
    fs::create_dir_all(config_dir).map_err(|_| AppError::config_write(source))?;

    let mut temporary = Builder::new()
        .prefix(".ai-jarvis-")
        .suffix(".tmp")
        .tempfile_in(config_dir)
        .map_err(|_| AppError::config_write(source))?;
    temporary
        .write_all(&serialized)
        .and_then(|()| temporary.write_all(b"\n"))
        .and_then(|()| temporary.flush())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|_| AppError::config_write(source))?;

    before_persist().map_err(|_| AppError::config_write(source))?;
    let target = config_dir.join(file_name);
    let mut pending = temporary;
    for attempt in 0..=WINDOWS_PERSIST_RETRY_LIMIT {
        match pending.persist(&target) {
            Ok(_) => return Ok(()),
            Err(failure) => {
                if attempt == WINDOWS_PERSIST_RETRY_LIMIT
                    || !retryable_windows_persist_error(&failure.error)
                {
                    return Err(AppError::config_write(source));
                }
                pending = failure.file;
                thread::sleep(WINDOWS_PERSIST_RETRY_DELAY);
            }
        }
    }
    unreachable!("bounded persist loop always returns")
}

#[cfg(windows)]
fn retryable_windows_persist_error(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5 | 32))
}

#[cfg(not(windows))]
fn retryable_windows_persist_error(_error: &io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Barrier,
        },
        thread,
    };

    use serde_json::{json, Value};
    use tempfile::tempdir;

    use super::*;

    fn enabled_settings() -> MvpSettings {
        MvpSettings {
            features: FeatureSettings {
                screen_enabled: true,
                system_audio_enabled: true,
                proactive_response_enabled: true,
            },
            excluded_applications: vec!["  keepass.exe  ".into(), "keepass.exe".into()],
            excluded_windows: vec!["  Payroll  ".into(), "".into()],
        }
    }

    fn diagnostic_for(loaded: &LoadedSettings, source: ConfigSource) -> &ConfigDiagnostic {
        loaded
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.source == source)
            .expect("source diagnostic")
    }

    fn assert_conservative(settings: &MvpSettings) {
        assert!(!settings.features.screen_enabled);
        assert!(!settings.features.system_audio_enabled);
        assert!(!settings.features.proactive_response_enabled);
        assert!(settings.excluded_applications.is_empty());
        assert!(settings.excluded_windows.is_empty());
    }

    fn assert_no_temporary_files(config_dir: &Path) {
        let remnants = fs::read_dir(config_dir)
            .expect("config directory")
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.starts_with(".ai-jarvis-") && name.ends_with(".tmp"))
            .collect::<Vec<_>>();
        assert!(remnants.is_empty(), "temporary remnants: {remnants:?}");
    }

    #[test]
    fn missing_files_load_paused_and_conservative_with_diagnostics() {
        let directory = tempdir().expect("temporary directory");
        let loaded = SettingsStore::new(directory.path()).load();

        assert_eq!(loaded.privacy_state, PrivacyState::Paused);
        assert_conservative(&loaded.settings);
        assert_eq!(loaded.diagnostics.len(), 2);
        assert_eq!(
            diagnostic_for(&loaded, ConfigSource::StartupState).kind,
            ConfigDiagnosticKind::Missing
        );
        assert_eq!(
            diagnostic_for(&loaded, ConfigSource::Settings).kind,
            ConfigDiagnosticKind::Missing
        );
    }

    #[test]
    fn malformed_startup_files_always_load_paused() {
        let invalid_documents = [
            br#"{"schemaVersion":1,"privacyPaused":"yes"}"#.as_slice(),
            br#"{"schemaVersion":1,"privacyPaused":false"#.as_slice(),
            br#"{"schemaVersion":2,"privacyPaused":false}"#.as_slice(),
            br#"{"schemaVersion":1,"privacyPaused":false,"extra":true}"#.as_slice(),
        ];

        for document in invalid_documents {
            let directory = tempdir().expect("temporary directory");
            fs::write(directory.path().join(STARTUP_STATE_FILE), document).expect("invalid file");
            let loaded = SettingsStore::new(directory.path()).load();

            assert_eq!(loaded.privacy_state, PrivacyState::Paused);
            assert_eq!(
                diagnostic_for(&loaded, ConfigSource::StartupState).kind,
                ConfigDiagnosticKind::Invalid
            );
        }
    }

    #[test]
    fn malformed_settings_files_always_load_conservative_defaults() {
        let invalid_documents = [
            json!({
                "schemaVersion": 1,
                "screenEnabled": "yes",
                "systemAudioEnabled": true,
                "proactiveResponseEnabled": true,
                "excludedApplications": [],
                "excludedWindows": []
            })
            .to_string(),
            "{\"schemaVersion\":1".into(),
            json!({
                "schemaVersion": 2,
                "screenEnabled": true,
                "systemAudioEnabled": true,
                "proactiveResponseEnabled": true,
                "excludedApplications": [],
                "excludedWindows": []
            })
            .to_string(),
            json!({
                "schemaVersion": 1,
                "screenEnabled": true,
                "systemAudioEnabled": true,
                "proactiveResponseEnabled": true,
                "excludedApplications": [],
                "excludedWindows": [],
                "extra": true
            })
            .to_string(),
        ];

        for document in invalid_documents {
            let directory = tempdir().expect("temporary directory");
            fs::write(directory.path().join(SETTINGS_FILE), document).expect("invalid file");
            let loaded = SettingsStore::new(directory.path()).load();

            assert_conservative(&loaded.settings);
            assert_eq!(
                diagnostic_for(&loaded, ConfigSource::Settings).kind,
                ConfigDiagnosticKind::Invalid
            );
        }
    }

    #[test]
    fn damaged_startup_state_does_not_discard_valid_feature_settings() {
        let directory = tempdir().expect("temporary directory");
        let store = SettingsStore::new(directory.path());
        let expected = store
            .save_settings(enabled_settings())
            .expect("valid settings");
        fs::write(directory.path().join(STARTUP_STATE_FILE), b"not-json")
            .expect("damaged startup state");

        let loaded = store.load();

        assert_eq!(loaded.privacy_state, PrivacyState::Paused);
        assert_eq!(loaded.settings, expected);
        assert_eq!(loaded.diagnostics.len(), 1);
        assert_eq!(
            diagnostic_for(&loaded, ConfigSource::StartupState).kind,
            ConfigDiagnosticKind::Invalid
        );
    }

    #[test]
    fn damaged_settings_do_not_override_a_valid_startup_state() {
        let directory = tempdir().expect("temporary directory");
        let store = SettingsStore::new(directory.path());
        store
            .save_startup_state(PrivacyState::Active)
            .expect("valid startup state");
        fs::write(directory.path().join(SETTINGS_FILE), b"not-json").expect("damaged settings");

        let loaded = store.load();

        assert_eq!(loaded.privacy_state, PrivacyState::Active);
        assert_conservative(&loaded.settings);
        assert_eq!(loaded.diagnostics.len(), 1);
        assert_eq!(
            diagnostic_for(&loaded, ConfigSource::Settings).kind,
            ConfigDiagnosticKind::Invalid
        );
    }

    #[test]
    fn save_recovers_bad_files_and_persists_flat_normalized_schema() {
        let directory = tempdir().expect("temporary directory");
        fs::write(directory.path().join(STARTUP_STATE_FILE), b"not-json")
            .expect("bad startup file");
        fs::write(directory.path().join(SETTINGS_FILE), b"not-json").expect("bad settings file");
        let store = SettingsStore::new(directory.path());

        store
            .save_startup_state(PrivacyState::Active)
            .expect("save startup state");
        let normalized = store
            .save_settings(enabled_settings())
            .expect("save settings");

        assert_eq!(
            normalized.excluded_applications,
            vec!["keepass.exe".to_owned()]
        );
        assert_eq!(normalized.excluded_windows, vec!["Payroll".to_owned()]);
        let loaded = store.load();
        assert_eq!(loaded.privacy_state, PrivacyState::Active);
        assert_eq!(loaded.settings, normalized);
        assert!(loaded.diagnostics.is_empty());

        let document: Value = serde_json::from_slice(
            &fs::read(directory.path().join(SETTINGS_FILE)).expect("settings bytes"),
        )
        .expect("settings JSON");
        assert_eq!(document["schemaVersion"], 1);
        assert_eq!(document["screenEnabled"], true);
        assert!(document.get("features").is_none());
        assert_no_temporary_files(directory.path());
    }

    #[test]
    fn failure_before_replace_preserves_the_complete_old_target() {
        let directory = tempdir().expect("temporary directory");
        let target = directory.path().join(SETTINGS_FILE);
        let old_contents = b"complete-old-settings\n";
        fs::write(&target, old_contents).expect("old target");
        let replacement = SettingsFile::from(&enabled_settings());

        let result = atomic_json_write_with_hook(
            directory.path(),
            SETTINGS_FILE,
            &replacement,
            ConfigSource::Settings,
            || Err(io::Error::other("injected failure")),
        );

        assert!(result.is_err());
        assert_eq!(fs::read(target).expect("old target bytes"), old_contents);
        assert_no_temporary_files(directory.path());
    }

    #[test]
    fn cloned_store_serializes_concurrent_writers_without_partial_json() {
        const WRITERS: usize = 6;
        const WRITES_PER_THREAD: usize = 4;

        let directory = tempdir().expect("temporary directory");
        let store = Arc::new(SettingsStore::new(directory.path()));
        let barrier = Arc::new(Barrier::new(WRITERS));
        let mut workers = Vec::new();

        for writer in 0..WRITERS {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                for sequence in 0..WRITES_PER_THREAD {
                    let enabled = (writer + sequence) % 2 == 0;
                    store
                        .save_startup_state(if enabled {
                            PrivacyState::Active
                        } else {
                            PrivacyState::Paused
                        })
                        .expect("concurrent startup save");
                    store
                        .save_settings(MvpSettings {
                            features: FeatureSettings {
                                screen_enabled: enabled,
                                system_audio_enabled: !enabled,
                                proactive_response_enabled: enabled,
                            },
                            excluded_applications: vec![format!("app-{writer}-{sequence}")],
                            excluded_windows: vec![format!("window-{writer}-{sequence}")],
                        })
                        .expect("concurrent settings save");
                }
            }));
        }

        for worker in workers {
            worker.join().expect("writer thread");
        }

        let loaded = store.load();
        assert!(loaded.diagnostics.is_empty());
        assert_eq!(loaded.settings.excluded_applications.len(), 1);
        assert_eq!(loaded.settings.excluded_windows.len(), 1);
        assert_no_temporary_files(directory.path());
    }

    #[test]
    fn concurrent_reader_observes_only_complete_json_documents() {
        let directory = tempdir().expect("temporary directory");
        let store = SettingsStore::new(directory.path());
        store
            .save_settings(enabled_settings())
            .expect("initial settings");

        let target = directory.path().join(SETTINGS_FILE);
        let start = Arc::new(Barrier::new(2));
        let finished = Arc::new(AtomicBool::new(false));
        let reader_start = Arc::clone(&start);
        let reader_finished = Arc::clone(&finished);
        let reader = thread::spawn(move || {
            reader_start.wait();
            let mut reads = 0;
            while !reader_finished.load(Ordering::Acquire) {
                let bytes = fs::read(&target).expect("read current settings");
                let document = serde_json::from_slice::<SettingsFile>(&bytes)
                    .expect("reader must observe a complete JSON document");
                assert_eq!(document.schema_version, SCHEMA_VERSION);
                reads += 1;
                thread::yield_now();
            }
            reads
        });

        start.wait();
        for sequence in 0..100 {
            store
                .save_settings(MvpSettings {
                    features: FeatureSettings {
                        screen_enabled: sequence % 2 == 0,
                        system_audio_enabled: sequence % 3 == 0,
                        proactive_response_enabled: sequence % 5 == 0,
                    },
                    excluded_applications: vec![format!("app-{sequence}")],
                    excluded_windows: vec![format!("window-{sequence}")],
                })
                .expect("replace settings");
        }
        finished.store(true, Ordering::Release);

        assert!(reader.join().expect("reader thread") > 0);
        assert_no_temporary_files(directory.path());
    }

    #[test]
    fn oversized_exclusion_input_is_rejected_without_replacing_settings() {
        let directory = tempdir().expect("temporary directory");
        let store = SettingsStore::new(directory.path());
        let saved = store
            .save_settings(enabled_settings())
            .expect("initial settings");
        let old_contents = fs::read(directory.path().join(SETTINGS_FILE)).expect("old settings");
        let mut invalid = saved;
        invalid.excluded_windows = (0..=MAX_EXCLUSIONS_PER_KIND)
            .map(|index| format!("window-{index}"))
            .collect();

        assert!(store.save_settings(invalid).is_err());
        assert_eq!(
            fs::read(directory.path().join(SETTINGS_FILE)).expect("current settings"),
            old_contents
        );
        assert_no_temporary_files(directory.path());
    }

    #[test]
    fn serialized_size_limit_prevents_writing_a_file_that_cannot_be_reloaded() {
        let directory = tempdir().expect("temporary directory");
        let store = SettingsStore::new(directory.path());
        let valid = MvpSettings {
            excluded_applications: vec!["界".repeat(MAX_EXCLUSION_CHARS)],
            ..enabled_settings()
        };
        let normalized = store.save_settings(valid).expect("valid UTF-8 settings");
        assert_eq!(store.load().settings, normalized);

        let old_contents = fs::read(directory.path().join(SETTINGS_FILE)).expect("old settings");
        let oversized = MvpSettings {
            features: normalized.features,
            excluded_applications: (0..MAX_EXCLUSIONS_PER_KIND)
                .map(|index| format!("{index:02}{}", "界".repeat(MAX_EXCLUSION_CHARS - 2)))
                .collect(),
            excluded_windows: (0..MAX_EXCLUSIONS_PER_KIND)
                .map(|index| format!("{index:02}{}", "窗".repeat(MAX_EXCLUSION_CHARS - 2)))
                .collect(),
        };

        assert!(store.save_settings(oversized).is_err());
        assert_eq!(
            fs::read(directory.path().join(SETTINGS_FILE)).expect("current settings"),
            old_contents
        );
        assert_no_temporary_files(directory.path());
    }

    #[test]
    fn diagnostics_do_not_expose_paths_or_invalid_contents() {
        let directory = tempdir().expect("temporary directory");
        let secret = "private-window-title";
        fs::write(directory.path().join(SETTINGS_FILE), secret).expect("invalid settings");

        let loaded = SettingsStore::new(directory.path()).load();
        let diagnostic = diagnostic_for(&loaded, ConfigSource::Settings);
        assert!(!diagnostic.message.contains(secret));
        assert!(!diagnostic
            .message
            .contains(&directory.path().display().to_string()));
    }
}
