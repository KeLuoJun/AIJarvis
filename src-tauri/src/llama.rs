use std::{
    collections::BTreeMap,
    error::Error,
    fmt, fs, io,
    path::{Component, Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use jsonschema::JSONSchema;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;

use crate::prompts::{PromptId, PromptStore, RenderedPrompt};

const MODEL_CONFIG_SCHEMA_VERSION: u32 = 1;
const HEALTH_PATH: &str = "/health";
const MODELS_PATH: &str = "/v1/models";
const CHAT_PATH: &str = "/v1/chat/completions";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelState {
    Unavailable,
    Starting,
    Ready,
    Busy,
    Degraded,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HealthState {
    Loading,
    Ready,
    Degraded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LlamaError {
    Config,
    ArtifactMissing,
    Spawn,
    AlreadyStarted,
    NotReady,
    ProcessExited,
    Http,
    Timeout,
    Protocol,
    Schema,
    Stopping,
}

impl fmt::Display for LlamaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Config => "llama.cpp configuration is invalid",
            Self::ArtifactMissing => "a required local model artifact is missing",
            Self::Spawn => "llama.cpp could not be started",
            Self::AlreadyStarted => "llama.cpp is already started",
            Self::NotReady => "llama.cpp is not ready",
            Self::ProcessExited => "llama.cpp exited before becoming ready",
            Self::Http => "llama.cpp request failed",
            Self::Timeout => "llama.cpp operation timed out",
            Self::Protocol => "llama.cpp returned an invalid response",
            Self::Schema => "model output did not match its schema",
            Self::Stopping => "llama.cpp could not be stopped",
        })
    }
}

impl Error for LlamaError {}

#[derive(Clone, Debug, Deserialize)]
struct ModelDocument {
    schema_version: u32,
    paths: ModelPaths,
    llama: LlamaExecutable,
    server: ServerConfig,
    qwen: QwenConfig,
}

#[derive(Clone, Debug, Deserialize)]
struct ModelPaths {
    model_root_env: String,
    default_model_root: String,
}

#[derive(Clone, Debug, Deserialize)]
struct LlamaExecutable {
    executable: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ServerConfig {
    subcommand: String,
    alias: String,
    host: String,
    port: u16,
    context_size: u32,
    reasoning: String,
}

#[derive(Clone, Debug, Deserialize)]
struct QwenConfig {
    model: Artifact,
    mmproj: Artifact,
    sampling: SamplingConfig,
}

#[derive(Clone, Debug, Deserialize)]
struct Artifact {
    relative_path: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct SamplingConfig {
    temperature: f32,
    top_p: f32,
    top_k: u32,
    min_p: f32,
    presence_penalty: f32,
    repeat_penalty: f32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchSpec {
    pub executable: PathBuf,
    pub args: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct LlamaConfig {
    base_url: String,
    alias: String,
    launch: LaunchSpec,
    model_path: PathBuf,
    mmproj_path: PathBuf,
    sampling: SamplingConfig,
}

impl LlamaConfig {
    pub fn load(
        path: &Path,
        model_root: Option<&Path>,
        executable: Option<&Path>,
    ) -> Result<Self, LlamaError> {
        let text = fs::read_to_string(path).map_err(|_| LlamaError::Config)?;
        let document = toml::from_str::<ModelDocument>(&text).map_err(|_| LlamaError::Config)?;
        if document.schema_version != MODEL_CONFIG_SCHEMA_VERSION
            || document.server.subcommand != "serve"
            || document.server.reasoning != "off"
            || document.server.host != "127.0.0.1"
            || document.server.port == 0
            || document.server.alias.trim().is_empty()
            || document.server.context_size == 0
            || document.llama.executable != "llama.exe"
            || !sampling_is_valid(document.qwen.sampling)
        {
            return Err(LlamaError::Config);
        }
        let root = match model_root {
            Some(path) => path.to_path_buf(),
            None => resolve_model_root(&document.paths)?,
        };
        let model_path = safe_model_path(&root, &document.qwen.model.relative_path)?;
        let mmproj_path = safe_model_path(&root, &document.qwen.mmproj.relative_path)?;
        let executable = executable
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(&document.llama.executable));
        let args = vec![
            document.server.subcommand.clone(),
            "--model".into(),
            model_path.to_string_lossy().into_owned(),
            "--mmproj".into(),
            mmproj_path.to_string_lossy().into_owned(),
            "--reasoning".into(),
            document.server.reasoning.clone(),
            "--alias".into(),
            document.server.alias.clone(),
            "--host".into(),
            document.server.host.clone(),
            "--port".into(),
            document.server.port.to_string(),
            "--ctx-size".into(),
            document.server.context_size.to_string(),
            "--no-ui".into(),
        ];
        Ok(Self {
            base_url: format!("http://{}:{}", document.server.host, document.server.port),
            alias: document.server.alias,
            launch: LaunchSpec { executable, args },
            model_path,
            mmproj_path,
            sampling: document.qwen.sampling,
        })
    }

    pub fn launch_spec(&self) -> &LaunchSpec {
        &self.launch
    }
}

fn resolve_model_root(paths: &ModelPaths) -> Result<PathBuf, LlamaError> {
    if let Some(value) = std::env::var_os(&paths.model_root_env) {
        return Ok(PathBuf::from(value));
    }
    let local = std::env::var("LOCALAPPDATA").map_err(|_| LlamaError::Config)?;
    Ok(PathBuf::from(
        paths.default_model_root.replace("%LOCALAPPDATA%", &local),
    ))
}

fn safe_model_path(root: &Path, relative: &str) -> Result<PathBuf, LlamaError> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(LlamaError::Config);
    }
    Ok(root.join(path))
}

fn sampling_is_valid(sampling: SamplingConfig) -> bool {
    sampling.temperature.is_finite()
        && sampling.temperature >= 0.0
        && sampling.top_p.is_finite()
        && (0.0..=1.0).contains(&sampling.top_p)
        && sampling.top_k > 0
        && sampling.min_p.is_finite()
        && (0.0..=1.0).contains(&sampling.min_p)
        && sampling.presence_penalty.is_finite()
        && sampling.repeat_penalty.is_finite()
        && sampling.repeat_penalty > 0.0
}

pub trait ManagedProcess: Send {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    fn kill(&mut self) -> io::Result<()>;
}

impl ManagedProcess for Child {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Child::try_wait(self)
    }

    fn kill(&mut self) -> io::Result<()> {
        Child::kill(self)
    }
}

pub trait ProcessSpawner: Send + Sync {
    fn spawn(&self, spec: &LaunchSpec) -> io::Result<Box<dyn ManagedProcess>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemProcessSpawner;

impl ProcessSpawner for SystemProcessSpawner {
    fn spawn(&self, spec: &LaunchSpec) -> io::Result<Box<dyn ManagedProcess>> {
        let mut command = Command::new(&spec.executable);
        configure_process(&mut command);
        let child = command
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Box::new(child))
    }
}

#[cfg(windows)]
fn configure_process(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn configure_process(_command: &mut Command) {}

#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Value,
}

#[allow(async_fn_in_trait)]
pub trait HttpTransport: Send + Sync {
    async fn get(&self, url: &str, timeout: Duration) -> Result<HttpResponse, LlamaError>;
    async fn post(
        &self,
        url: &str,
        body: Value,
        timeout: Duration,
    ) -> Result<HttpResponse, LlamaError>;
}

#[derive(Clone, Debug)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl HttpTransport for ReqwestTransport {
    async fn get(&self, url: &str, timeout: Duration) -> Result<HttpResponse, LlamaError> {
        let response = self
            .client
            .get(url)
            .timeout(timeout)
            .send()
            .await
            .map_err(map_reqwest)?;
        decode_http(response).await
    }

    async fn post(
        &self,
        url: &str,
        body: Value,
        timeout: Duration,
    ) -> Result<HttpResponse, LlamaError> {
        let response = self
            .client
            .post(url)
            .json(&body)
            .timeout(timeout)
            .send()
            .await
            .map_err(map_reqwest)?;
        decode_http(response).await
    }
}

fn map_reqwest(error: reqwest::Error) -> LlamaError {
    if error.is_timeout() {
        LlamaError::Timeout
    } else {
        LlamaError::Http
    }
}

async fn decode_http(response: reqwest::Response) -> Result<HttpResponse, LlamaError> {
    let status = response.status().as_u16();
    let body = response
        .json::<Value>()
        .await
        .map_err(|_| LlamaError::Protocol)?;
    Ok(HttpResponse { status, body })
}

#[derive(Clone, Debug)]
pub struct ModelRequest {
    pub prompt: PromptId,
    pub variables: BTreeMap<String, Value>,
    pub image_data_urls: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Scene {
    Game,
    Course,
    Meeting,
    Video,
    Coding,
    Reading,
    Work,
    Chat,
    Other,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ObservationOutput {
    pub scene: Scene,
    pub need_response: bool,
    pub next_check_seconds: u8,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DirectChatOutput {
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelOutput {
    Observation(ObservationOutput),
    DirectChat(DirectChatOutput),
}

struct ProcessState {
    model_state: ModelState,
    child: Option<Box<dyn ManagedProcess>>,
}

pub struct LlamaService<S = SystemProcessSpawner, T = ReqwestTransport> {
    config: LlamaConfig,
    prompts: PromptStore,
    spawner: S,
    transport: T,
    process: Arc<Mutex<ProcessState>>,
    lifecycle_lock: AsyncMutex<()>,
    request_lock: AsyncMutex<()>,
}

impl<S: ProcessSpawner, T: HttpTransport> LlamaService<S, T> {
    pub fn new(config: LlamaConfig, prompts: PromptStore, spawner: S, transport: T) -> Self {
        Self {
            config,
            prompts,
            spawner,
            transport,
            process: Arc::new(Mutex::new(ProcessState {
                model_state: ModelState::Unavailable,
                child: None,
            })),
            lifecycle_lock: AsyncMutex::new(()),
            request_lock: AsyncMutex::new(()),
        }
    }

    pub fn state(&self) -> ModelState {
        self.lock_process().model_state
    }

    pub fn base_url(&self) -> &str {
        &self.config.base_url
    }

    pub fn model_alias(&self) -> &str {
        &self.config.alias
    }

    pub async fn start(&self, startup_timeout: Duration) -> Result<(), LlamaError> {
        let _lifecycle = self.lifecycle_lock.lock().await;
        if !self.config.model_path.is_file() || !self.config.mmproj_path.is_file() {
            return Err(LlamaError::ArtifactMissing);
        }
        {
            let mut state = self.lock_process();
            if state.child.is_some() {
                return Err(LlamaError::AlreadyStarted);
            }
            state.model_state = ModelState::Starting;
            match self.spawner.spawn(&self.config.launch) {
                Ok(child) => state.child = Some(child),
                Err(_) => {
                    state.model_state = ModelState::Degraded;
                    return Err(LlamaError::Spawn);
                }
            }
        }
        let deadline = tokio::time::Instant::now() + startup_timeout;
        loop {
            if self.process_exited()? {
                self.set_state(ModelState::Degraded);
                return Err(LlamaError::ProcessExited);
            }
            match self.probe_health(Duration::from_secs(2)).await {
                Err(LlamaError::Http | LlamaError::Timeout) => {}
                Err(error) => return self.fail_start(error),
                Ok(HealthState::Ready) => break,
                Ok(HealthState::Loading) => {}
                Ok(HealthState::Degraded) => return self.fail_start(LlamaError::Protocol),
            }
            if tokio::time::Instant::now() >= deadline {
                return self.fail_start(LlamaError::Timeout);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if let Err(error) = self.validate_models(Duration::from_secs(2)).await {
            return self.fail_start(error);
        }
        self.set_state(ModelState::Ready);
        Ok(())
    }

    pub async fn probe_health(&self, timeout: Duration) -> Result<HealthState, LlamaError> {
        let response = self
            .transport
            .get(&format!("{}{}", self.config.base_url, HEALTH_PATH), timeout)
            .await?;
        if response.status == StatusCode::SERVICE_UNAVAILABLE.as_u16() {
            return Ok(HealthState::Loading);
        }
        if response.status == StatusCode::OK.as_u16()
            && response.body.get("status").and_then(Value::as_str) == Some("ok")
        {
            Ok(HealthState::Ready)
        } else {
            Ok(HealthState::Degraded)
        }
    }

    async fn validate_models(&self, timeout: Duration) -> Result<(), LlamaError> {
        let response = self
            .transport
            .get(&format!("{}{}", self.config.base_url, MODELS_PATH), timeout)
            .await?;
        let found = response.status == 200
            && response
                .body
                .get("data")
                .and_then(Value::as_array)
                .is_some_and(|models| {
                    models.iter().any(|model| {
                        model.get("id").and_then(Value::as_str) == Some(&self.config.alias)
                    })
                });
        if found {
            Ok(())
        } else {
            self.set_state(ModelState::Degraded);
            Err(LlamaError::Protocol)
        }
    }

    pub async fn request(
        &self,
        request: ModelRequest,
        timeout: Duration,
    ) -> Result<ModelOutput, LlamaError> {
        let _single_flight = self.request_lock.lock().await;
        if self.state() != ModelState::Ready {
            return Err(LlamaError::NotReady);
        }
        self.set_state(ModelState::Busy);
        let result = self.perform_request(request, timeout).await;
        let mut process = self.lock_process();
        if process.model_state == ModelState::Busy {
            process.model_state = if result.is_ok() {
                ModelState::Ready
            } else {
                ModelState::Degraded
            };
        }
        result
    }

    async fn perform_request(
        &self,
        request: ModelRequest,
        timeout: Duration,
    ) -> Result<ModelOutput, LlamaError> {
        let prompt = self
            .prompts
            .render(request.prompt, &request.variables)
            .map_err(|_| LlamaError::Config)?;
        let body = build_request_body(&self.config, &prompt, &request.image_data_urls);
        let response = self
            .transport
            .post(
                &format!("{}{}", self.config.base_url, CHAT_PATH),
                body,
                timeout,
            )
            .await?;
        if response.status != 200 {
            return Err(LlamaError::Http);
        }
        parse_chat_response(request.prompt, &prompt.schema, &response.body)
    }

    pub async fn stop(&self, timeout: Duration) -> Result<(), LlamaError> {
        let _lifecycle = self.lifecycle_lock.lock().await;
        let deadline = tokio::time::Instant::now() + timeout;
        {
            let mut state = self.lock_process();
            let Some(child) = state.child.as_mut() else {
                state.model_state = ModelState::Stopped;
                return Ok(());
            };
            if child.kill().is_err() {
                state.model_state = ModelState::Degraded;
                return Err(LlamaError::Stopping);
            }
        }
        loop {
            if self.process_exited()? {
                self.set_state(ModelState::Stopped);
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                self.set_state(ModelState::Degraded);
                return Err(LlamaError::Timeout);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn process_exited(&self) -> Result<bool, LlamaError> {
        let mut state = self.lock_process();
        let Some(child) = state.child.as_mut() else {
            return Ok(false);
        };
        if child
            .try_wait()
            .map_err(|_| LlamaError::ProcessExited)?
            .is_some()
        {
            state.child = None;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn fail_start(&self, error: LlamaError) -> Result<(), LlamaError> {
        let mut state = self.lock_process();
        let terminated = state
            .child
            .as_mut()
            .map_or(true, |child| child.kill().is_ok());
        if terminated {
            state.child = None;
        }
        state.model_state = ModelState::Degraded;
        Err(error)
    }

    fn set_state(&self, model_state: ModelState) {
        self.lock_process().model_state = model_state;
    }

    fn lock_process(&self) -> MutexGuard<'_, ProcessState> {
        self.process
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<S, T> Drop for LlamaService<S, T> {
    fn drop(&mut self) {
        let Ok(mut state) = self.process.lock() else {
            return;
        };
        if let Some(child) = state.child.as_mut() {
            let _ = child.kill();
        }
    }
}

fn build_request_body(config: &LlamaConfig, prompt: &RenderedPrompt, images: &[String]) -> Value {
    let user_content = if images.is_empty() {
        Value::String(prompt.user.clone())
    } else {
        let mut content = vec![json!({"type": "text", "text": prompt.user})];
        content.extend(
            images
                .iter()
                .map(|url| json!({"type": "image_url", "image_url": {"url": url}})),
        );
        Value::Array(content)
    };
    json!({
        "model": config.alias,
        "messages": [
            {"role": "system", "content": prompt.system},
            {"role": "user", "content": user_content}
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": {"name": prompt.id.as_str(), "strict": true, "schema": prompt.schema}
        },
        "max_tokens": prompt.max_output_tokens,
        "temperature": config.sampling.temperature,
        "top_p": config.sampling.top_p,
        "top_k": config.sampling.top_k,
        "min_p": config.sampling.min_p,
        "presence_penalty": config.sampling.presence_penalty,
        "repeat_penalty": config.sampling.repeat_penalty
    })
}

pub fn parse_chat_response(
    id: PromptId,
    schema: &Value,
    response: &Value,
) -> Result<ModelOutput, LlamaError> {
    let message = response
        .pointer("/choices/0/message")
        .ok_or(LlamaError::Protocol)?;
    if message
        .get("reasoning_content")
        .is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
    {
        return Err(LlamaError::Protocol);
    }
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .ok_or(LlamaError::Protocol)?;
    if content.to_ascii_lowercase().contains("<think>") {
        return Err(LlamaError::Protocol);
    }
    let value: Value = serde_json::from_str(content).map_err(|_| LlamaError::Protocol)?;
    let compiled = JSONSchema::compile(schema).map_err(|_| LlamaError::Schema)?;
    if !compiled.is_valid(&value) {
        return Err(LlamaError::Schema);
    }
    match id {
        PromptId::Observation => {
            let output = serde_json::from_value::<ObservationOutput>(value)
                .map_err(|_| LlamaError::Schema)?;
            if output.need_response == output.message.trim().is_empty() {
                return Err(LlamaError::Schema);
            }
            Ok(ModelOutput::Observation(output))
        }
        PromptId::DirectChat => {
            let output = serde_json::from_value::<DirectChatOutput>(value)
                .map_err(|_| LlamaError::Schema)?;
            if output.message.trim().is_empty() {
                return Err(LlamaError::Schema);
            }
            Ok(ModelOutput::DirectChat(output))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use super::*;
    use tempfile::tempdir;

    struct FakeProcess {
        exited: Arc<AtomicBool>,
    }

    impl ManagedProcess for FakeProcess {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(self.exited.load(Ordering::Acquire).then(fake_exit_status))
        }

        fn kill(&mut self) -> io::Result<()> {
            self.exited.store(true, Ordering::Release);
            Ok(())
        }
    }

    #[cfg(windows)]
    fn fake_exit_status() -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    #[cfg(unix)]
    fn fake_exit_status() -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    #[derive(Clone)]
    struct FakeSpawner {
        exited: Arc<AtomicBool>,
    }

    impl ProcessSpawner for FakeSpawner {
        fn spawn(&self, _: &LaunchSpec) -> io::Result<Box<dyn ManagedProcess>> {
            Ok(Box::new(FakeProcess {
                exited: Arc::clone(&self.exited),
            }))
        }
    }

    #[derive(Clone, Default)]
    struct FakeTransport {
        gets: Arc<Mutex<VecDeque<HttpResponse>>>,
        posts: Arc<Mutex<VecDeque<HttpResponse>>>,
        bodies: Arc<Mutex<Vec<Value>>>,
        post_error: Arc<Mutex<Option<LlamaError>>>,
        post_delay: Arc<Mutex<Duration>>,
        active_posts: Arc<AtomicUsize>,
        max_active_posts: Arc<AtomicUsize>,
    }

    impl HttpTransport for FakeTransport {
        async fn get(&self, _: &str, _: Duration) -> Result<HttpResponse, LlamaError> {
            self.gets
                .lock()
                .expect("gets")
                .pop_front()
                .ok_or(LlamaError::Http)
        }

        async fn post(
            &self,
            _: &str,
            body: Value,
            _: Duration,
        ) -> Result<HttpResponse, LlamaError> {
            self.bodies.lock().expect("bodies").push(body);
            let active = self.active_posts.fetch_add(1, Ordering::AcqRel) + 1;
            self.max_active_posts.fetch_max(active, Ordering::AcqRel);
            let delay = *self.post_delay.lock().expect("post delay");
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            self.active_posts.fetch_sub(1, Ordering::AcqRel);
            if let Some(error) = self.post_error.lock().expect("post error").take() {
                return Err(error);
            }
            self.posts
                .lock()
                .expect("posts")
                .pop_front()
                .ok_or(LlamaError::Http)
        }
    }

    fn fixture_config(directory: &Path) -> LlamaConfig {
        let root = directory.join("models");
        fs::create_dir_all(root.join("qwen")).expect("models");
        fs::write(root.join("qwen/model.gguf"), b"x").expect("model");
        fs::write(root.join("qwen/mmproj.gguf"), b"x").expect("mmproj");
        let config = directory.join("model.toml");
        fs::write(
            &config,
            "schema_version=1\n[paths]\nmodel_root_env='TEST_MODEL_ROOT'\ndefault_model_root='%LOCALAPPDATA%/x'\n[llama]\nexecutable='llama.exe'\n[server]\nsubcommand='serve'\nalias='qwen3.5-4b'\nhost='127.0.0.1'\nport=8080\ncontext_size=8192\nreasoning='off'\n[qwen.model]\nrelative_path='qwen/model.gguf'\n[qwen.mmproj]\nrelative_path='qwen/mmproj.gguf'\n[qwen.sampling]\ntemperature=0.7\ntop_p=0.8\ntop_k=20\nmin_p=0.0\npresence_penalty=1.5\nrepeat_penalty=1.0\n",
        )
        .expect("config");
        LlamaConfig::load(&config, Some(&root), None).expect("llama config")
    }

    fn prompt_store() -> PromptStore {
        PromptStore::load(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("workspace root"),
        )
        .expect("prompts")
    }

    #[tokio::test]
    async fn lifecycle_treats_503_as_loading_then_stops_cleanly() {
        let directory = tempdir().expect("temp");
        let transport = FakeTransport::default();
        transport.gets.lock().expect("gets").extend([
            HttpResponse {
                status: 503,
                body: json!({"status": "loading"}),
            },
            HttpResponse {
                status: 200,
                body: json!({"status": "ok"}),
            },
            HttpResponse {
                status: 200,
                body: json!({"data": [{"id": "qwen3.5-4b"}]}),
            },
        ]);
        let service = LlamaService::new(
            fixture_config(directory.path()),
            prompt_store(),
            FakeSpawner {
                exited: Arc::new(AtomicBool::new(false)),
            },
            transport,
        );
        service.start(Duration::from_secs(1)).await.expect("start");
        assert_eq!(service.state(), ModelState::Ready);
        service.stop(Duration::from_secs(1)).await.expect("stop");
        assert_eq!(service.state(), ModelState::Stopped);
    }

    #[tokio::test]
    async fn startup_timeout_cleans_up_the_spawned_process() {
        let directory = tempdir().expect("temp");
        let exited = Arc::new(AtomicBool::new(false));
        let service = LlamaService::new(
            fixture_config(directory.path()),
            prompt_store(),
            FakeSpawner {
                exited: Arc::clone(&exited),
            },
            FakeTransport::default(),
        );

        assert_eq!(
            service.start(Duration::from_millis(1)).await,
            Err(LlamaError::Timeout)
        );
        assert!(exited.load(Ordering::Acquire));
        assert_eq!(service.state(), ModelState::Degraded);
    }

    #[test]
    fn launch_argv_is_local_non_thinking_and_not_shell_joined() {
        let directory = tempdir().expect("temp");
        let config = fixture_config(directory.path());
        let spec = config.launch_spec();
        assert!(spec
            .args
            .windows(2)
            .any(|pair| pair == ["--reasoning", "off"]));
        assert!(spec.args.contains(&"--no-ui".to_owned()));
        assert!(!spec
            .args
            .iter()
            .any(|arg| arg.contains("enable_thinking") || arg == "-hf"));
    }

    #[test]
    fn model_paths_cannot_escape_the_configured_root() {
        let directory = tempdir().expect("temp");
        let root = directory.path().join("models");
        fs::create_dir_all(&root).expect("models");
        let config = directory.path().join("model.toml");
        fs::write(
            &config,
            "schema_version=1\n[paths]\nmodel_root_env='TEST_MODEL_ROOT'\ndefault_model_root='%LOCALAPPDATA%/x'\n[llama]\nexecutable='llama.exe'\n[server]\nsubcommand='serve'\nalias='qwen3.5-4b'\nhost='127.0.0.1'\nport=8080\ncontext_size=8192\nreasoning='off'\n[qwen.model]\nrelative_path='../outside.gguf'\n[qwen.mmproj]\nrelative_path='qwen/mmproj.gguf'\n[qwen.sampling]\ntemperature=0.7\ntop_p=0.8\ntop_k=20\nmin_p=0.0\npresence_penalty=1.5\nrepeat_penalty=1.0\n",
        )
        .expect("config");
        assert!(matches!(
            LlamaConfig::load(&config, Some(&root), None),
            Err(LlamaError::Config)
        ));
    }

    #[test]
    fn response_validation_rejects_wrappers_extras_truncation_and_thinking() {
        let schema = prompt_store()
            .render(
                PromptId::DirectChat,
                &BTreeMap::from([
                    ("persona".into(), json!("p")),
                    ("runtime_state".into(), json!("r")),
                    ("user_message".into(), json!("u")),
                ]),
            )
            .expect("prompt")
            .schema;
        for body in [
            json!({"choices": [{"message": {"content": "```json\n{\"message\":\"ok\"}\n```"}}]}),
            json!({"choices": [{"message": {"content": "{\"message\":\"ok\",\"extra\":true}"}}]}),
            json!({"choices": [{"message": {"content": "{\"message\":"}}]}),
            json!({"choices": [{"message": {"content": "<think>hidden</think>{\"message\":\"ok\"}"}}]}),
            json!({"choices": [{"message": {"content": "{\"message\":\"ok\"}", "reasoning_content": "hidden"}}]}),
        ] {
            assert!(parse_chat_response(PromptId::DirectChat, &schema, &body).is_err());
        }
    }

    #[tokio::test]
    async fn request_uses_schema_preserves_image_order_and_omits_reasoning_overrides() {
        let directory = tempdir().expect("temp");
        let transport = FakeTransport::default();
        transport.gets.lock().expect("gets").extend([
            HttpResponse {
                status: 200,
                body: json!({"status": "ok"}),
            },
            HttpResponse {
                status: 200,
                body: json!({"data": [{"id": "qwen3.5-4b"}]}),
            },
        ]);
        transport
            .posts
            .lock()
            .expect("posts")
            .push_back(HttpResponse {
                status: 200,
                body: json!({"choices": [{"message": {"content": "{\"scene\":\"coding\",\"need_response\":false,\"next_check_seconds\":10,\"message\":\"\"}"}}]}),
            });
        let service = LlamaService::new(
            fixture_config(directory.path()),
            prompt_store(),
            FakeSpawner {
                exited: Arc::new(AtomicBool::new(false)),
            },
            transport.clone(),
        );
        service.start(Duration::from_secs(1)).await.expect("start");
        let variables = BTreeMap::from([
            ("persona".into(), json!("p")),
            ("runtime_state".into(), json!("r")),
            ("audio_observations".into(), json!("a")),
        ]);
        service
            .request(
                ModelRequest {
                    prompt: PromptId::Observation,
                    variables,
                    image_data_urls: vec!["data:first".into(), "data:second".into()],
                },
                Duration::from_secs(1),
            )
            .await
            .expect("request");
        let body = transport
            .bodies
            .lock()
            .expect("bodies")
            .pop()
            .expect("body");
        let serialized = body.to_string();
        assert!(
            serialized.find("data:first").expect("first")
                < serialized.find("data:second").expect("second")
        );
        assert!(body.get("reasoning").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("chat_template_kwargs").is_none());
        assert_eq!(body.get("repeat_penalty"), Some(&json!(1.0)));
    }

    #[tokio::test]
    async fn request_timeout_has_one_error_and_moves_service_to_degraded() {
        let directory = tempdir().expect("temp");
        let transport = FakeTransport::default();
        transport.gets.lock().expect("gets").extend([
            HttpResponse {
                status: 200,
                body: json!({"status": "ok"}),
            },
            HttpResponse {
                status: 200,
                body: json!({"data": [{"id": "qwen3.5-4b"}]}),
            },
        ]);
        *transport.post_error.lock().expect("post error") = Some(LlamaError::Timeout);
        let service = LlamaService::new(
            fixture_config(directory.path()),
            prompt_store(),
            FakeSpawner {
                exited: Arc::new(AtomicBool::new(false)),
            },
            transport,
        );
        service.start(Duration::from_secs(1)).await.expect("start");
        let request = ModelRequest {
            prompt: PromptId::DirectChat,
            variables: BTreeMap::from([
                ("persona".into(), json!("p")),
                ("runtime_state".into(), json!("r")),
                ("user_message".into(), json!("u")),
            ]),
            image_data_urls: Vec::new(),
        };
        assert_eq!(
            service.request(request, Duration::from_millis(1)).await,
            Err(LlamaError::Timeout)
        );
        assert_eq!(service.state(), ModelState::Degraded);
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_transport_flight() {
        let directory = tempdir().expect("temp");
        let transport = FakeTransport::default();
        transport.gets.lock().expect("gets").extend([
            HttpResponse {
                status: 200,
                body: json!({"status": "ok"}),
            },
            HttpResponse {
                status: 200,
                body: json!({"data": [{"id": "qwen3.5-4b"}]}),
            },
        ]);
        transport
            .posts
            .lock()
            .expect("posts")
            .extend([0, 1].map(|index| HttpResponse {
                status: 200,
                body: json!({"choices": [{"message": {"content": format!("{{\"message\":\"reply {index}\"}}")}}]}),
            }));
        *transport.post_delay.lock().expect("post delay") = Duration::from_millis(10);
        let service = LlamaService::new(
            fixture_config(directory.path()),
            prompt_store(),
            FakeSpawner {
                exited: Arc::new(AtomicBool::new(false)),
            },
            transport.clone(),
        );
        service.start(Duration::from_secs(1)).await.expect("start");
        let request = ModelRequest {
            prompt: PromptId::DirectChat,
            variables: BTreeMap::from([
                ("persona".into(), json!("p")),
                ("runtime_state".into(), json!("r")),
                ("user_message".into(), json!("u")),
            ]),
            image_data_urls: Vec::new(),
        };

        let (first, second) = tokio::join!(
            service.request(request.clone(), Duration::from_secs(1)),
            service.request(request, Duration::from_secs(1))
        );
        assert!(first.is_ok() && second.is_ok());
        assert_eq!(transport.max_active_posts.load(Ordering::Acquire), 1);
    }
}
