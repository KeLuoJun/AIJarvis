use std::{
    fmt, fs,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};

use crate::{
    chat::{ChatAccepted, ChatEventSink, ChatStatusEvent, ChatTurn},
    error::AppError,
    observation::{ObservationEvent, ObservationEventSink},
    state::{RuntimeTransition, RuntimeTransitionFailure},
    types::{ClearTransientOutputEvent, ClearTransientOutputReason, Feature, RuntimeSnapshot},
    windows, AppState,
};

const MODEL_CONFIG_PATH: &str = "resources/config/model.toml";
const DEFAULTS_CONFIG_PATH: &str = "resources/config/defaults.toml";
const QWEN_PREFLIGHT_PATH: &str = "resources/prompts/probes/qwen-preflight.json";
const PET_MANIFEST_PATH: &str = "assets/pet/manifest.json";
pub(crate) const RUNTIME_STATE_CHANGED_EVENT: &str = "runtime-state-changed";
pub(crate) const CLEAR_TRANSIENT_OUTPUT_EVENT: &str = "clear-transient-output";
pub(crate) const CHAT_STATUS_EVENT: &str = "chat-status";
pub(crate) const OBSERVATION_RESULT_EVENT: &str = "observation-result";
pub(crate) const PET_CONTAINER_WIDTH: u32 = 320;
pub(crate) const PET_CONTAINER_HEIGHT: u32 = 480;

pub(crate) struct TauriChatEventSink {
    app: AppHandle,
}

impl TauriChatEventSink {
    pub(crate) fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl ChatEventSink for TauriChatEventSink {
    fn emit(&self, event: ChatStatusEvent) {
        if let Err(error) = self.app.emit(CHAT_STATUS_EVENT, event) {
            eprintln!("chat status event could not be emitted: {error}");
        }
    }
}

pub(crate) struct TauriObservationEventSink {
    app: AppHandle,
}

impl TauriObservationEventSink {
    pub(crate) fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl ObservationEventSink for TauriObservationEventSink {
    fn emit(&self, event: ObservationEvent) {
        if let Err(error) = self.app.emit(OBSERVATION_RESULT_EVENT, event) {
            eprintln!("observation result event could not be emitted: {error}");
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AppWindowKind {
    Control,
    Pet,
    Danmaku,
}

impl AppWindowKind {
    pub const ALL: [Self; 3] = [Self::Control, Self::Pet, Self::Danmaku];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Pet => "pet",
            Self::Danmaku => "danmaku",
        }
    }
}

impl fmt::Display for AppWindowKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<&str> for AppWindowKind {
    type Error = String;

    fn try_from(label: &str) -> Result<Self, Self::Error> {
        match label {
            "control" => Ok(Self::Control),
            "pet" => Ok(Self::Pet),
            "danmaku" => Ok(Self::Danmaku),
            _ => Err(format!("unknown application window label: {label}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowMutation {
    Show(AppWindowKind),
    Close(AppWindowKind),
    Quit,
}

fn authorize_window_mutation(caller_label: &str, mutation: WindowMutation) -> Result<(), String> {
    let caller = AppWindowKind::try_from(caller_label)?;
    let allowed = matches!(
        (caller, mutation),
        (AppWindowKind::Control, _)
            | (
                AppWindowKind::Pet,
                WindowMutation::Show(AppWindowKind::Control)
            )
            | (
                AppWindowKind::Pet,
                WindowMutation::Close(AppWindowKind::Pet)
            )
    );

    allowed
        .then_some(())
        .ok_or_else(|| "the calling window is not allowed to perform this action".to_owned())
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceStatus {
    pub model_config: bool,
    pub defaults_config: bool,
    pub pet_manifest: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResourceLayoutStatus {
    pub api: ResourceStatus,
    pub qwen_preflight: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapState {
    app_version: String,
    current_window: AppWindowKind,
    resource_status: ResourceStatus,
    windows: Vec<WindowState>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowState {
    kind: AppWindowKind,
    open: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PetManifest {
    schema_version: u32,
    container: PetContainer,
    states: PetStates,
}

#[derive(Debug, Deserialize)]
struct PetContainer {
    width: u32,
    height: u32,
}

#[derive(Debug, Deserialize)]
struct PetStates {
    idle: PetState,
    normal: PetState,
    paused: PetState,
}

#[derive(Debug, Deserialize)]
struct PetState {
    animation: String,
    still: String,
}

#[tauri::command]
pub fn get_bootstrap_state(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<BootstrapState, String> {
    let current_window = AppWindowKind::try_from(window.label())?;
    let windows = AppWindowKind::ALL
        .into_iter()
        .map(|kind| WindowState {
            kind,
            open: app.get_webview_window(kind.as_str()).is_some(),
        })
        .collect();

    Ok(BootstrapState {
        app_version: app.package_info().version.to_string(),
        current_window,
        resource_status: state.resources.api.clone(),
        windows,
    })
}

#[tauri::command]
pub async fn show_app_window(
    app: AppHandle,
    window: WebviewWindow,
    kind: AppWindowKind,
) -> Result<(), String> {
    authorize_window_mutation(window.label(), WindowMutation::Show(kind))?;
    windows::show_app_window(&app, kind).map_err(|error| error.to_string())
}

#[tauri::command]
pub fn close_app_window(
    app: AppHandle,
    window: WebviewWindow,
    kind: AppWindowKind,
) -> Result<(), String> {
    authorize_window_mutation(window.label(), WindowMutation::Close(kind))?;
    if let Some(window) = app.get_webview_window(kind.as_str()) {
        window.close().map_err(|error| error.to_string())?;
    }

    Ok(())
}

#[tauri::command]
pub fn get_runtime_state(state: State<'_, AppState>) -> RuntimeSnapshot {
    state.runtime.snapshot()
}

#[tauri::command]
pub async fn set_privacy_paused(
    app: AppHandle,
    state: State<'_, AppState>,
    paused: bool,
) -> Result<RuntimeSnapshot, AppError> {
    let result = state.runtime.set_privacy_paused(paused).await;
    if paused {
        state.observation.cancel();
    }
    complete_runtime_transition(&app, result)
}

#[tauri::command]
pub async fn set_feature_enabled(
    app: AppHandle,
    state: State<'_, AppState>,
    feature: Feature,
    enabled: bool,
) -> Result<RuntimeSnapshot, AppError> {
    let result = state.runtime.set_feature_enabled(feature, enabled).await;
    if !enabled {
        match feature {
            Feature::Screen | Feature::ProactiveResponse => state.observation.cancel(),
            Feature::SystemAudio => state.observation.clear_audio_pending(),
        }
    }
    complete_runtime_transition(&app, result)
}

#[tauri::command]
pub async fn send_text_message(
    state: State<'_, AppState>,
    message: String,
) -> Result<ChatAccepted, AppError> {
    state.chat.submit_text(message)
}

#[tauri::command]
pub fn get_chat_history(state: State<'_, AppState>) -> Vec<ChatTurn> {
    state.chat.history()
}

#[tauri::command]
pub fn clear_chat_history(state: State<'_, AppState>) {
    state.chat.clear();
}

#[tauri::command]
pub fn quit_app(app: AppHandle, window: WebviewWindow) -> Result<(), String> {
    authorize_window_mutation(window.label(), WindowMutation::Quit)?;
    crate::request_shutdown_and_exit(&app);
    Ok(())
}

pub(crate) fn publish_runtime_transition(app: &AppHandle, transition: &RuntimeTransition) {
    if let Some(reason) = transition.clear_transient_output {
        publish_clear_transient_output(
            app,
            transition.snapshot.revision,
            transition.snapshot.runtime_generation,
            reason,
        );
    }
    if let Err(error) = app.emit(RUNTIME_STATE_CHANGED_EVENT, &transition.snapshot) {
        eprintln!("runtime state event could not be emitted: {error}");
    }
}

fn publish_clear_transient_output(
    app: &AppHandle,
    revision: u64,
    runtime_generation: u64,
    reason: ClearTransientOutputReason,
) {
    let event = ClearTransientOutputEvent {
        revision,
        runtime_generation,
        reason,
    };
    if let Err(error) = app.emit(CLEAR_TRANSIENT_OUTPUT_EVENT, event) {
        eprintln!("transient output clear event could not be emitted: {error}");
    }
}

fn complete_runtime_transition(
    app: &AppHandle,
    result: Result<RuntimeTransition, RuntimeTransitionFailure>,
) -> Result<RuntimeSnapshot, AppError> {
    match result {
        Ok(transition) => {
            publish_runtime_transition(app, &transition);
            Ok(transition.snapshot)
        }
        Err(failure) => {
            publish_runtime_transition(app, &failure.transition);
            Err(failure.error)
        }
    }
}

pub(crate) fn validate_resource_layout(resource_dir: &Path) -> ResourceLayoutStatus {
    ResourceLayoutStatus {
        api: ResourceStatus {
            model_config: readable_nonempty_file(&resource_dir.join(MODEL_CONFIG_PATH)),
            defaults_config: readable_nonempty_file(&resource_dir.join(DEFAULTS_CONFIG_PATH)),
            pet_manifest: validate_pet_manifest(&resource_dir.join(PET_MANIFEST_PATH)),
        },
        qwen_preflight: readable_nonempty_file(&resource_dir.join(QWEN_PREFLIGHT_PATH)),
    }
}

fn readable_nonempty_file(path: &Path) -> bool {
    fs::read(path)
        .map(|contents| !contents.is_empty())
        .unwrap_or(false)
}

fn validate_pet_manifest(manifest_path: &Path) -> bool {
    let Ok(contents) = fs::read(manifest_path) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_slice::<PetManifest>(&contents) else {
        return false;
    };

    if manifest.schema_version != 1
        || manifest.container.width != PET_CONTAINER_WIDTH
        || manifest.container.height != PET_CONTAINER_HEIGHT
    {
        return false;
    }

    let expected_assets = [
        (manifest.states.idle.animation, "idle.gif"),
        (manifest.states.idle.still, "idle.png"),
        (manifest.states.normal.animation, "normal.gif"),
        (manifest.states.normal.still, "normal.png"),
        (manifest.states.paused.animation, "closed-eyes.gif"),
        (manifest.states.paused.still, "closed-eyes.png"),
    ];
    let Some(manifest_dir) = manifest_path.parent() else {
        return false;
    };

    expected_assets.into_iter().all(|(asset, expected)| {
        asset == expected
            && safe_manifest_asset(manifest_dir, &asset).is_some_and(|path| path.is_file())
    })
}

fn safe_manifest_asset(manifest_dir: &Path, asset: &str) -> Option<PathBuf> {
    let relative = Path::new(asset);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }

    Some(manifest_dir.join(relative))
}

#[cfg(test)]
mod tests {
    use std::{fs, future::Future};

    use tempfile::tempdir;

    use super::*;

    const VALID_MANIFEST: &str = r#"{
      "schemaVersion": 1,
      "container": { "width": 320, "height": 480 },
      "states": {
        "idle": { "animation": "idle.gif", "still": "idle.png" },
        "normal": { "animation": "normal.gif", "still": "normal.png" },
        "paused": { "animation": "closed-eyes.gif", "still": "closed-eyes.png" }
      }
    }"#;

    #[test]
    fn parses_only_known_window_labels() {
        for kind in AppWindowKind::ALL {
            assert_eq!(AppWindowKind::try_from(kind.as_str()), Ok(kind));
        }
        assert!(AppWindowKind::try_from("settings").is_err());
        assert!(AppWindowKind::try_from("Pet").is_err());
    }

    #[test]
    fn show_window_command_stays_async() {
        fn assert_async_command<FutureResult>(
            _command: fn(AppHandle, WebviewWindow, AppWindowKind) -> FutureResult,
        ) where
            FutureResult: Future<Output = Result<(), String>>,
        {
        }

        assert_async_command(show_app_window);
    }

    #[test]
    fn window_mutations_are_authorized_by_caller_and_target() {
        for target in AppWindowKind::ALL {
            assert_eq!(
                authorize_window_mutation("control", WindowMutation::Show(target)),
                Ok(())
            );
            assert_eq!(
                authorize_window_mutation("control", WindowMutation::Close(target)),
                Ok(())
            );
        }
        assert_eq!(
            authorize_window_mutation("control", WindowMutation::Quit),
            Ok(())
        );

        assert_eq!(
            authorize_window_mutation("pet", WindowMutation::Show(AppWindowKind::Control)),
            Ok(())
        );
        assert_eq!(
            authorize_window_mutation("pet", WindowMutation::Close(AppWindowKind::Pet)),
            Ok(())
        );
        for denied in [
            WindowMutation::Show(AppWindowKind::Pet),
            WindowMutation::Show(AppWindowKind::Danmaku),
            WindowMutation::Close(AppWindowKind::Control),
            WindowMutation::Close(AppWindowKind::Danmaku),
            WindowMutation::Quit,
        ] {
            assert!(authorize_window_mutation("pet", denied).is_err());
        }

        for denied in [
            WindowMutation::Show(AppWindowKind::Control),
            WindowMutation::Close(AppWindowKind::Danmaku),
            WindowMutation::Quit,
        ] {
            assert!(authorize_window_mutation("danmaku", denied).is_err());
        }
        assert!(authorize_window_mutation("unknown", WindowMutation::Quit).is_err());
    }

    #[test]
    fn bootstrap_state_serializes_the_fixed_ipc_dto() {
        let state = BootstrapState {
            app_version: "0.1.0".to_owned(),
            current_window: AppWindowKind::Pet,
            resource_status: ResourceStatus {
                model_config: true,
                defaults_config: false,
                pet_manifest: true,
            },
            windows: vec![WindowState {
                kind: AppWindowKind::Pet,
                open: true,
            }],
        };

        assert_eq!(
            serde_json::to_value(state).expect("serialize bootstrap state"),
            serde_json::json!({
                "appVersion": "0.1.0",
                "currentWindow": "pet",
                "resourceStatus": {
                    "modelConfig": true,
                    "defaultsConfig": false,
                    "petManifest": true
                },
                "windows": [{ "kind": "pet", "open": true }]
            })
        );
    }

    #[test]
    fn runtime_event_names_are_a_fixed_frontend_contract() {
        assert_eq!(RUNTIME_STATE_CHANGED_EVENT, "runtime-state-changed");
        assert_eq!(CLEAR_TRANSIENT_OUTPUT_EVENT, "clear-transient-output");
        assert_eq!(CHAT_STATUS_EVENT, "chat-status");
        assert_eq!(OBSERVATION_RESULT_EVENT, "observation-result");
    }

    #[test]
    fn validates_the_complete_mapped_resource_layout() {
        let root = tempdir().expect("temporary resource directory");
        write_valid_resource_layout(root.path());

        assert_eq!(
            validate_resource_layout(root.path()),
            ResourceLayoutStatus {
                api: ResourceStatus {
                    model_config: true,
                    defaults_config: true,
                    pet_manifest: true,
                },
                qwen_preflight: true,
            }
        );
    }

    #[test]
    fn validates_the_mapped_prompt_probe_separately_from_the_ipc_dto() {
        let root = tempdir().expect("temporary resource directory");
        write_valid_resource_layout(root.path());
        fs::remove_file(root.path().join(QWEN_PREFLIGHT_PATH))
            .expect("remove the required prompt probe");

        let layout = validate_resource_layout(root.path());
        assert!(!layout.qwen_preflight);
        assert!(layout.api.model_config);
        assert!(layout.api.defaults_config);
        assert!(layout.api.pet_manifest);
    }

    #[test]
    fn pet_manifest_requires_every_declared_asset() {
        let root = tempdir().expect("temporary resource directory");
        write_valid_resource_layout(root.path());
        fs::remove_file(root.path().join("assets/pet/normal.gif"))
            .expect("remove one required asset");

        let status = validate_resource_layout(root.path());
        assert!(status.api.model_config);
        assert!(status.api.defaults_config);
        assert!(!status.api.pet_manifest);
    }

    #[test]
    fn pet_manifest_rejects_an_unknown_schema_version() {
        let root = tempdir().expect("temporary resource directory");
        write_valid_resource_layout(root.path());
        let manifest = VALID_MANIFEST.replace("\"schemaVersion\": 1", "\"schemaVersion\": 2");
        fs::write(root.path().join(PET_MANIFEST_PATH), manifest).expect("replace manifest");

        assert!(!validate_resource_layout(root.path()).api.pet_manifest);
    }

    #[test]
    fn pet_manifest_rejects_non_contract_container_dimensions() {
        let root = tempdir().expect("temporary resource directory");
        write_valid_resource_layout(root.path());
        let manifest_path = root.path().join(PET_MANIFEST_PATH);
        let mut manifest =
            serde_json::from_str::<serde_json::Value>(VALID_MANIFEST).expect("valid fixture JSON");

        manifest["container"]["width"] = serde_json::json!(PET_CONTAINER_WIDTH + 1);
        fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("serialize wrong-width manifest"),
        )
        .expect("write wrong-width manifest");
        assert!(!validate_resource_layout(root.path()).api.pet_manifest);

        manifest["container"]["width"] = serde_json::json!(PET_CONTAINER_WIDTH);
        manifest["container"]["height"] = serde_json::json!(PET_CONTAINER_HEIGHT - 1);
        fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("serialize wrong-height manifest"),
        )
        .expect("write wrong-height manifest");
        assert!(!validate_resource_layout(root.path()).api.pet_manifest);
    }

    fn write_valid_resource_layout(root: &Path) {
        let config_dir = root.join("resources/config");
        let prompt_dir = root.join("resources/prompts/probes");
        let pet_dir = root.join("assets/pet");
        fs::create_dir_all(&config_dir).expect("create config directory");
        fs::create_dir_all(&prompt_dir).expect("create prompt directory");
        fs::create_dir_all(&pet_dir).expect("create pet directory");
        fs::write(config_dir.join("model.toml"), "schema_version = 1\n")
            .expect("write model config");
        fs::write(config_dir.join("defaults.toml"), "schema_version = 1\n")
            .expect("write defaults config");
        fs::write(
            prompt_dir.join("qwen-preflight.json"),
            "{\"schema_version\":1}\n",
        )
        .expect("write prompt probe");
        fs::write(pet_dir.join("manifest.json"), VALID_MANIFEST).expect("write pet manifest");
        for asset in [
            "idle.gif",
            "idle.png",
            "normal.gif",
            "normal.png",
            "closed-eyes.gif",
            "closed-eyes.png",
        ] {
            fs::write(pet_dir.join(asset), b"asset").expect("write pet asset");
        }
    }
}
