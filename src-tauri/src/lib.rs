// MSVC localizes its normal import-library notice; Rust 1.98 reports any linker stdout as a warning.
#![allow(unknown_lints, linker_messages)]

pub mod asr;
pub mod asr_worker_protocol;
pub mod audio;
pub mod chat;
mod commands;
pub mod error;
pub mod llama;
pub mod observation;
pub mod privacy;
pub mod prompts;
pub mod scheduler;
pub mod screen;
mod screen_worker;
pub mod sensevoice;
pub mod settings;
pub mod state;
pub mod supervisor;
pub mod types;
mod windows;

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use asr::{
    AsrEngine, DirectUserMessage, DirectUserMessageSink, ObservationTranscriptBuffer,
    ObservationTranscriptSink,
};
use chat::{ChatEngine, LlamaChatModel};
use commands::{
    AppWindowKind, ResourceLayoutStatus, TauriChatEventSink, TauriObservationEventSink,
};
use llama::{LlamaConfig, LlamaService, ReqwestTransport, SystemProcessSpawner};
use observation::{LlamaObservationModel, ObservationEngine};
use prompts::PromptStore;
use scheduler::MonotonicClock;
use screen_worker::ScreenCaptureController;
use sensevoice::{SenseVoiceConfig, SenseVoiceWorkerModel};
use state::{RuntimeConfig, RuntimeState};
use tauri::{Manager, RunEvent, WindowEvent};
use types::LifecycleState;

const MODEL_CONFIG_PATH: &str = "resources/config/model.toml";
const DEFAULT_PERSONA_PATH: &str = "resources/prompts/persona/default.md";
const ASR_WORKER_FILE: &str = "asr_worker.exe";
const ASR_WORKER_DEVELOPMENT_FILE: &str = "asr_worker-x86_64-pc-windows-msvc.exe";
const MODEL_STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
const MODEL_STOP_TIMEOUT: Duration = Duration::from_secs(2);

type ProductionLlamaService = LlamaService<SystemProcessSpawner, ReqwestTransport>;

pub(crate) struct AppState {
    resources: ResourceLayoutStatus,
    runtime: Arc<RuntimeState>,
    chat: Arc<ChatEngine>,
    observation: Arc<ObservationEngine<MonotonicClock>>,
    asr: Option<Arc<AsrEngine>>,
    asr_model: Option<Arc<SenseVoiceWorkerModel>>,
    audio_transcripts: Arc<ObservationTranscriptBuffer>,
    screen_capture: ScreenCaptureController,
    screen_config: screen::ScreenRuntimeConfig,
    llama: Option<Arc<ProductionLlamaService>>,
    llama_start_task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    shutdown_requested: AtomicBool,
    exit_authorized: AtomicBool,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            let resource_dir = app.path().resource_dir()?;
            let resources = commands::validate_resource_layout(&resource_dir);
            let runtime_config = RuntimeConfig::load(
                &resource_dir.join("resources/config/defaults.toml"),
            )?;
            let runtime = Arc::new(RuntimeState::load(
                app.path().app_config_dir()?,
                runtime_config,
            )?);
            let model_resources =
                build_model_resources(app.handle(), &resource_dir, &runtime, runtime_config)?;
            let asr_resources = build_asr_resources(
                &resource_dir,
                &runtime,
                &model_resources.chat,
                runtime_config,
            )?;
            app.manage(AppState {
                resources: resources.clone(),
                runtime,
                chat: model_resources.chat,
                observation: model_resources.observation,
                asr: asr_resources.engine,
                asr_model: asr_resources.model,
                audio_transcripts: asr_resources.transcripts,
                screen_capture: ScreenCaptureController::default(),
                screen_config: runtime_config.screen,
                llama: model_resources.llama,
                llama_start_task: Mutex::new(model_resources.start_task),
                shutdown_requested: AtomicBool::new(false),
                exit_authorized: AtomicBool::new(false),
            });
            {
                let state = app.state::<AppState>();
                let snapshot = state.runtime.snapshot();
                if snapshot.privacy_state == types::PrivacyState::Active
                    && snapshot.features.screen_enabled
                {
                    state.screen_capture.start(
                        Arc::clone(&state.runtime),
                        Arc::clone(&state.observation),
                        Arc::clone(&state.audio_transcripts),
                        state.screen_config,
                    )?;
                }
            }

            if smoke_test_enabled() {
                for kind in AppWindowKind::ALL {
                    windows::show_app_window(app.handle(), kind)?;
                }
                eprintln!(
                    "AI_JARVIS_SMOKE windows=control,pet,danmaku resources:model_config={},defaults_config={},qwen_preflight={},pet_manifest={}",
                    resources.api.model_config,
                    resources.api.defaults_config,
                    resources.qwen_preflight,
                    resources.api.pet_manifest
                );

                let app_handle = app.handle().clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(750));
                    app_handle.exit(0);
                });
            } else {
                windows::show_app_window(app.handle(), AppWindowKind::Pet)?;
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_bootstrap_state,
            commands::get_runtime_state,
            commands::set_privacy_paused,
            commands::set_feature_enabled,
            commands::send_text_message,
            commands::get_chat_history,
            commands::clear_chat_history,
            commands::show_app_window,
            commands::close_app_window,
            commands::quit_app,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build AI Jarvis");

    app.run(|app, event| match event {
        RunEvent::ExitRequested { api, .. } => {
            let state = app.state::<AppState>();
            if !state.exit_authorized.load(Ordering::Acquire)
                && state.runtime.snapshot().lifecycle_state != LifecycleState::Stopped
            {
                api.prevent_exit();
                request_shutdown_and_exit(app);
            }
        }
        RunEvent::WindowEvent {
            label,
            event: WindowEvent::Destroyed,
            ..
        } => {
            // Tauri removes this label from its manager before emitting Destroyed.
            let remaining_windows = app.webview_windows();
            if windows::should_exit_after_window_destroyed(&label, remaining_windows.keys()) {
                request_shutdown_and_exit(app);
            }
        }
        _ => {}
    });
}

struct ModelResources {
    chat: Arc<ChatEngine>,
    observation: Arc<ObservationEngine<MonotonicClock>>,
    llama: Option<Arc<ProductionLlamaService>>,
    start_task: Option<tauri::async_runtime::JoinHandle<()>>,
}

struct AsrResources {
    engine: Option<Arc<AsrEngine>>,
    model: Option<Arc<SenseVoiceWorkerModel>>,
    transcripts: Arc<ObservationTranscriptBuffer>,
}

struct ChatDirectMessageSink {
    chat: Arc<ChatEngine>,
}

impl DirectUserMessageSink for ChatDirectMessageSink {
    fn emit(&self, message: DirectUserMessage) {
        let _ = self.chat.submit_text(message.text);
    }
}

fn build_model_resources(
    app: &tauri::AppHandle,
    resource_dir: &Path,
    runtime: &Arc<RuntimeState>,
    config: RuntimeConfig,
) -> Result<ModelResources, error::AppError> {
    let chat_sink: Arc<dyn chat::ChatEventSink> = Arc::new(TauriChatEventSink::new(app.clone()));
    let observation_sink: Arc<dyn observation::ObservationEventSink> =
        Arc::new(TauriObservationEventSink::new(app.clone()));
    let model = LlamaConfig::load(&resource_dir.join(MODEL_CONFIG_PATH), None, None);
    let prompts = PromptStore::load(resource_dir);
    let persona = fs::read_to_string(resource_dir.join(DEFAULT_PERSONA_PATH))
        .ok()
        .filter(|value| !value.trim().is_empty());

    let (Ok(model), Ok(prompts), Some(persona)) = (model, prompts, persona) else {
        return Ok(ModelResources {
            chat: ChatEngine::unavailable(Arc::clone(runtime), chat_sink, config.chat)?,
            observation: ObservationEngine::unavailable(
                Arc::clone(runtime),
                observation_sink,
                config.screen,
                MonotonicClock::default(),
            )?,
            llama: None,
            start_task: None,
        });
    };

    let llama = Arc::new(LlamaService::new(
        model,
        prompts,
        SystemProcessSpawner,
        ReqwestTransport::default(),
    ));
    let chat_model = Arc::new(LlamaChatModel::new(Arc::clone(&llama), config.chat));
    let observation_model = Arc::new(LlamaObservationModel::new(
        Arc::clone(&llama),
        config.chat.model_timeout,
    ));
    let chat = ChatEngine::new(
        Arc::clone(runtime),
        chat_model,
        chat_sink,
        persona.clone(),
        config.chat,
    )?;
    let observation = ObservationEngine::new(
        Arc::clone(runtime),
        observation_model,
        observation_sink,
        persona,
        config.screen,
        MonotonicClock::default(),
    )?;
    let startup_llama = Arc::clone(&llama);
    let start_task = tauri::async_runtime::spawn(async move {
        if let Err(error) = startup_llama.start(MODEL_STARTUP_TIMEOUT).await {
            eprintln!("local model could not be started: {error}");
        }
    });

    Ok(ModelResources {
        chat,
        observation,
        llama: Some(llama),
        start_task: Some(start_task),
    })
}

fn build_asr_resources(
    resource_dir: &Path,
    runtime: &Arc<RuntimeState>,
    chat: &Arc<ChatEngine>,
    config: RuntimeConfig,
) -> Result<AsrResources, error::AppError> {
    let transcripts = ObservationTranscriptBuffer::new(config.audio.system_audio_queue_capacity)?;
    let unavailable = || AsrResources {
        engine: None,
        model: None,
        transcripts: Arc::clone(&transcripts),
    };
    let model_config_path = resource_dir.join(MODEL_CONFIG_PATH);
    let Ok(model_config) = SenseVoiceConfig::load(&model_config_path, None) else {
        return Ok(unavailable());
    };
    if model_config.validate_artifacts().is_err() {
        return Ok(unavailable());
    }
    let Some(worker_path) = resolve_asr_worker_path(resource_dir) else {
        return Ok(unavailable());
    };
    let model = Arc::new(SenseVoiceWorkerModel::new(
        model_config.worker_launch_spec(worker_path, &model_config_path),
    ));
    let model_interface: Arc<dyn asr::AsrModel> = model.clone();
    let observation_sink: Arc<dyn ObservationTranscriptSink> = transcripts.clone();
    let direct_sink: Arc<dyn DirectUserMessageSink> = Arc::new(ChatDirectMessageSink {
        chat: Arc::clone(chat),
    });
    let engine = AsrEngine::new(
        Arc::clone(runtime),
        model_interface,
        observation_sink,
        direct_sink,
        config.audio.system_audio_queue_capacity,
    )?;
    Ok(AsrResources {
        engine: Some(engine),
        model: Some(model),
        transcripts,
    })
}

fn resolve_asr_worker_path(resource_dir: &Path) -> Option<PathBuf> {
    let mut candidates = vec![resource_dir.join(ASR_WORKER_FILE)];
    if let Ok(current_executable) = std::env::current_exe() {
        if let Some(directory) = current_executable.parent() {
            candidates.push(directory.join(ASR_WORKER_FILE));
        }
    }
    if cfg!(debug_assertions) {
        candidates.push(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("binaries")
                .join(ASR_WORKER_DEVELOPMENT_FILE),
        );
    }
    candidates.into_iter().find(|path| {
        fs::metadata(path)
            .map(|metadata| metadata.is_file() && metadata.len() > 0)
            .unwrap_or(false)
    })
}

pub(crate) fn request_shutdown_and_exit(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    if state
        .shutdown_requested
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    let runtime = Arc::clone(&state.runtime);
    let chat = Arc::clone(&state.chat);
    let observation = Arc::clone(&state.observation);
    let asr = state.asr.clone();
    let asr_model = state.asr_model.clone();
    state.screen_capture.stop();
    state.audio_transcripts.clear();
    if let Some(asr) = &asr {
        asr.cancel_pending();
    }
    let llama = state.llama.clone();
    let start_task = state
        .llama_start_task
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        observation.cancel();
        if let Some(asr_model) = asr_model {
            asr_model.stop();
        }
        if let Some(mut start_task) = start_task {
            start_task.abort();
            let _ = (&mut start_task).await;
        }
        match runtime.shutdown().await {
            Ok(transition) => commands::publish_runtime_transition(&app, &transition),
            Err(failure) => {
                commands::publish_runtime_transition(&app, &failure.transition);
                eprintln!("runtime shutdown failed: {}", failure.error);
            }
        }
        chat.clear();
        if let Some(llama) = llama {
            if let Err(error) = llama.stop(MODEL_STOP_TIMEOUT).await {
                eprintln!("local model could not be stopped cleanly: {error}");
            }
        }
        app.state::<AppState>()
            .exit_authorized
            .store(true, Ordering::Release);
        app.exit(0);
    });
}

fn smoke_test_enabled() -> bool {
    std::env::var("AI_JARVIS_SMOKE_TEST").as_deref() == Ok("1")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    #[test]
    fn bundle_maps_only_the_declared_external_resources() {
        let config: Value = serde_json::from_str(include_str!("../tauri.conf.json"))
            .expect("tauri.conf.json must be valid JSON");
        let resources = config["bundle"]["resources"]
            .as_object()
            .expect("bundle.resources must be a source-to-target map");

        assert_eq!(resources.len(), 8);
        assert_eq!(resources["binaries/onnxruntime.dll"], "onnxruntime.dll");
        assert_eq!(
            resources["binaries/onnxruntime_providers_shared.dll"],
            "onnxruntime_providers_shared.dll"
        );
        assert_eq!(
            resources["binaries/sherpa-onnx-c-api.dll"],
            "sherpa-onnx-c-api.dll"
        );
        assert_eq!(
            resources["binaries/sherpa-onnx-cxx-api.dll"],
            "sherpa-onnx-cxx-api.dll"
        );
        assert_eq!(resources["../resources/config/"], "resources/config/");
        assert_eq!(resources["../resources/prompts/"], "resources/prompts/");
        assert_eq!(resources["../resources/schemas/"], "resources/schemas/");
        assert_eq!(resources["../public/assets/pet/"], "assets/pet/");
        assert_eq!(
            config["bundle"]["externalBin"],
            serde_json::json!(["binaries/asr_worker"])
        );
        assert_eq!(config["app"]["windows"], Value::Array(Vec::new()));
    }

    #[test]
    fn production_and_development_csp_are_explicit_and_minimal() {
        let config: Value = serde_json::from_str(include_str!("../tauri.conf.json"))
            .expect("tauri.conf.json must be valid JSON");
        let security = &config["app"]["security"];

        assert_eq!(
            security["csp"],
            "default-src 'self'; script-src 'self'; connect-src ipc: http://ipc.localhost; img-src 'self' asset: http://asset.localhost; style-src 'self' 'unsafe-inline'; object-src 'none'"
        );
        assert_eq!(
            security["devCsp"],
            "default-src 'self'; script-src 'self' 'unsafe-inline'; connect-src ipc: http://ipc.localhost ws://localhost:1420 ws://localhost:1421; img-src 'self' asset: http://asset.localhost; style-src 'self' 'unsafe-inline'; object-src 'none'"
        );

        for policy in [
            security["csp"].as_str().expect("production CSP string"),
            security["devCsp"].as_str().expect("development CSP string"),
        ] {
            assert!(!policy.contains('*'));
            assert!(!policy.contains("'unsafe-eval'"));
            assert!(!policy.contains("https:"));
        }
    }

    #[test]
    fn capabilities_are_split_by_window_with_minimal_permissions() {
        fn permission_set(capability: &Value) -> BTreeSet<&str> {
            capability["permissions"]
                .as_array()
                .expect("permissions must be an array")
                .iter()
                .map(|permission| permission.as_str().expect("permission identifier"))
                .collect()
        }

        let control: Value = serde_json::from_str(include_str!("../capabilities/default.json"))
            .expect("control capability must be valid JSON");
        let pet: Value = serde_json::from_str(include_str!("../capabilities/pet.json"))
            .expect("pet capability must be valid JSON");
        let danmaku: Value = serde_json::from_str(include_str!("../capabilities/danmaku.json"))
            .expect("danmaku capability must be valid JSON");

        assert_eq!(control["identifier"], "control");
        assert_eq!(control["windows"], serde_json::json!(["control"]));
        assert_eq!(pet["identifier"], "pet");
        assert_eq!(pet["windows"], serde_json::json!(["pet"]));
        assert_eq!(danmaku["identifier"], "danmaku");
        assert_eq!(danmaku["windows"], serde_json::json!(["danmaku"]));

        let control_permissions = permission_set(&control);
        assert_eq!(
            control_permissions,
            BTreeSet::from([
                "allow-clear-chat-history",
                "allow-close-app-window",
                "allow-get-bootstrap-state",
                "allow-get-chat-history",
                "allow-get-runtime-state",
                "allow-quit-app",
                "allow-send-text-message",
                "allow-set-feature-enabled",
                "allow-set-privacy-paused",
                "allow-show-app-window",
                "core:event:allow-listen",
                "core:event:allow-unlisten",
                "core:window:allow-start-dragging",
            ])
        );
        assert_eq!(
            permission_set(&pet),
            BTreeSet::from([
                "allow-clear-chat-history",
                "allow-close-app-window",
                "allow-get-chat-history",
                "allow-get-runtime-state",
                "allow-send-text-message",
                "allow-show-app-window",
                "core:event:allow-listen",
                "core:event:allow-unlisten",
                "core:window:allow-start-dragging",
            ])
        );
        assert!(permission_set(&danmaku).is_empty());

        for permissions in [
            control_permissions,
            permission_set(&pet),
            permission_set(&danmaku),
        ] {
            assert!(permissions.iter().all(|permission| {
                !permission.contains("shell")
                    && !permission.contains("fs")
                    && !permission.contains("http")
            }));
        }
    }

    #[test]
    fn custom_permissions_expose_exactly_the_registered_commands() {
        let permissions: toml::Value = toml::from_str(include_str!("../permissions/app.toml"))
            .expect("custom permissions must be valid TOML");
        let commands = permissions["permission"]
            .as_array()
            .expect("permission entries")
            .iter()
            .flat_map(|permission| {
                permission["commands"]["allow"]
                    .as_array()
                    .expect("allowed commands")
            })
            .map(|command| command.as_str().expect("command name"))
            .collect::<BTreeSet<_>>();

        assert_eq!(
            commands,
            BTreeSet::from([
                "clear_chat_history",
                "close_app_window",
                "get_bootstrap_state",
                "get_chat_history",
                "get_runtime_state",
                "quit_app",
                "send_text_message",
                "set_feature_enabled",
                "set_privacy_paused",
                "show_app_window",
            ])
        );
    }
}
