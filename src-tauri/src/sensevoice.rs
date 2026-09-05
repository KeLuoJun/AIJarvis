use std::{
    error::Error,
    ffi::OsString,
    fmt, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use serde::Deserialize;

use crate::{
    asr::{AsrFailure, AsrModel, AsrModelFuture, AsrModelInput},
    asr_worker_protocol::{
        read_response, write_request, WorkerRequest, WorkerStatus, MAX_SAMPLE_COUNT, SAMPLE_RATE_HZ,
    },
    llama::{resolve_model_root, safe_model_path},
};

const MODEL_CONFIG_SCHEMA_VERSION: u32 = 1;
const SHERPA_ONNX_CRATE: &str = "sherpa-onnx";
const SHERPA_ONNX_VERSION: &str = "1.13.7";
const REQUIRED_FEATURE_DIM: u32 = 80;
const REQUIRED_LANGUAGE: &str = "auto";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SenseVoiceError {
    Config,
    ArtifactMissing,
}

impl fmt::Display for SenseVoiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Config => "SenseVoice configuration is invalid",
            Self::ArtifactMissing => "a required SenseVoice artifact is missing",
        })
    }
}

impl Error for SenseVoiceError {}

#[derive(Clone, Debug, Deserialize)]
struct ModelDocument {
    schema_version: u32,
    paths: ModelPaths,
    sensevoice: SenseVoiceDocument,
}

#[derive(Clone, Debug, Deserialize)]
struct ModelPaths {
    model_root_env: String,
    default_model_root: String,
}

#[derive(Clone, Debug, Deserialize)]
struct SenseVoiceDocument {
    rust_crate: String,
    rust_crate_version: String,
    sample_rate_hz: u32,
    feature_dim: u32,
    language: String,
    use_itn: bool,
    model: Artifact,
    tokens: Artifact,
}

#[derive(Clone, Debug, Deserialize)]
struct Artifact {
    relative_path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SenseVoiceConfig {
    pub model_root: PathBuf,
    pub model_path: PathBuf,
    pub tokens_path: PathBuf,
    pub sample_rate_hz: u32,
    pub feature_dim: u32,
    pub language: String,
    pub use_itn: bool,
}

impl SenseVoiceConfig {
    pub fn load(path: &Path, model_root: Option<&Path>) -> Result<Self, SenseVoiceError> {
        let text = fs::read_to_string(path).map_err(|_| SenseVoiceError::Config)?;
        let document =
            toml::from_str::<ModelDocument>(&text).map_err(|_| SenseVoiceError::Config)?;
        let source = document.sensevoice;
        if document.schema_version != MODEL_CONFIG_SCHEMA_VERSION
            || source.rust_crate != SHERPA_ONNX_CRATE
            || source.rust_crate_version != SHERPA_ONNX_VERSION
            || source.sample_rate_hz != crate::asr_worker_protocol::SAMPLE_RATE_HZ
            || source.feature_dim != REQUIRED_FEATURE_DIM
            || source.language != REQUIRED_LANGUAGE
            || !source.use_itn
        {
            return Err(SenseVoiceError::Config);
        }
        let root = match model_root {
            Some(root) => root.to_path_buf(),
            None => resolve_model_root(
                &document.paths.model_root_env,
                &document.paths.default_model_root,
            )
            .map_err(|_| SenseVoiceError::Config)?,
        };
        Ok(Self {
            model_path: safe_model_path(&root, &source.model.relative_path)
                .map_err(|_| SenseVoiceError::Config)?,
            tokens_path: safe_model_path(&root, &source.tokens.relative_path)
                .map_err(|_| SenseVoiceError::Config)?,
            model_root: root,
            sample_rate_hz: source.sample_rate_hz,
            feature_dim: source.feature_dim,
            language: source.language,
            use_itn: source.use_itn,
        })
    }

    pub fn validate_artifacts(&self) -> Result<(), SenseVoiceError> {
        if self.model_path.is_file() && self.tokens_path.is_file() {
            Ok(())
        } else {
            Err(SenseVoiceError::ArtifactMissing)
        }
    }

    pub fn worker_launch_spec(
        &self,
        executable: impl Into<PathBuf>,
        config_path: &Path,
    ) -> WorkerLaunchSpec {
        WorkerLaunchSpec {
            executable: executable.into(),
            args: vec![
                "--config".into(),
                config_path.as_os_str().to_owned(),
                "--model-root".into(),
                self.model_root.as_os_str().to_owned(),
            ],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerLaunchSpec {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
}

trait WorkerProcess: Send {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    fn kill(&mut self) -> io::Result<()>;
}

impl WorkerProcess for Child {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Child::try_wait(self)
    }

    fn kill(&mut self) -> io::Result<()> {
        Child::kill(self)
    }
}

struct SpawnedWorker {
    process: Box<dyn WorkerProcess>,
    input: Box<dyn Write + Send>,
    output: Box<dyn Read + Send>,
}

trait WorkerSpawner: Send + Sync {
    fn spawn(&self, spec: &WorkerLaunchSpec) -> io::Result<SpawnedWorker>;
}

#[derive(Clone, Copy, Debug, Default)]
struct SystemWorkerSpawner;

impl WorkerSpawner for SystemWorkerSpawner {
    fn spawn(&self, spec: &WorkerLaunchSpec) -> io::Result<SpawnedWorker> {
        let mut command = Command::new(&spec.executable);
        configure_process(&mut command);
        let mut child = command
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let Some(input) = child.stdin.take() else {
            let _ = child.kill();
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ASR worker stdin was not piped",
            ));
        };
        let Some(output) = child.stdout.take() else {
            let _ = child.kill();
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ASR worker stdout was not piped",
            ));
        };
        Ok(SpawnedWorker {
            process: Box::new(child),
            input: Box::new(input),
            output: Box::new(output),
        })
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

struct WorkerClient {
    spec: WorkerLaunchSpec,
    spawner: Arc<dyn WorkerSpawner>,
    process: Mutex<Option<Box<dyn WorkerProcess>>>,
    input: Mutex<Option<Box<dyn Write + Send>>>,
    output: Mutex<Option<Box<dyn Read + Send>>>,
    request_lock: Mutex<()>,
    next_request_id: AtomicU64,
}

impl WorkerClient {
    fn new(spec: WorkerLaunchSpec, spawner: Arc<dyn WorkerSpawner>) -> Arc<Self> {
        Arc::new(Self {
            spec,
            spawner,
            process: Mutex::new(None),
            input: Mutex::new(None),
            output: Mutex::new(None),
            request_lock: Mutex::new(()),
            next_request_id: AtomicU64::new(1),
        })
    }

    fn allocate_request_id(&self) -> Result<u64, AsrFailure> {
        self.next_request_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| AsrFailure::Inference)
    }

    fn transcribe(
        &self,
        request_id: u64,
        input: AsrModelInput,
        cancelled: &AtomicBool,
    ) -> Result<String, AsrFailure> {
        let _request = self.lock(&self.request_lock);
        if cancelled.load(Ordering::Acquire) {
            return Err(AsrFailure::Inference);
        }
        self.ensure_worker()?;
        if cancelled.load(Ordering::Acquire) {
            self.cancel_worker();
            return Err(AsrFailure::Inference);
        }
        let request = WorkerRequest {
            request_id,
            sample_rate_hz: input.sample_rate_hz,
            samples: input.samples,
        };
        let write_result = self
            .lock(&self.input)
            .as_mut()
            .ok_or(AsrFailure::ModelUnavailable)
            .and_then(|writer| {
                write_request(writer, &request).map_err(|_| AsrFailure::ModelUnavailable)
            });
        if let Err(error) = write_result {
            self.invalidate_worker();
            return Err(error);
        }
        let response = self
            .lock(&self.output)
            .as_mut()
            .ok_or(AsrFailure::ModelUnavailable)
            .and_then(|reader| read_response(reader).map_err(|_| AsrFailure::ModelUnavailable));
        let response = match response {
            Ok(response) if response.request_id == request_id => response,
            Ok(_) => {
                self.invalidate_worker();
                return Err(AsrFailure::Inference);
            }
            Err(error) => {
                self.invalidate_worker();
                return Err(error);
            }
        };
        match response.status {
            WorkerStatus::Success => Ok(response.transcript),
            WorkerStatus::InvalidAudio => Err(AsrFailure::InvalidAudio),
            WorkerStatus::InferenceFailed => Err(AsrFailure::Inference),
        }
    }

    fn ensure_worker(&self) -> Result<(), AsrFailure> {
        let running = {
            let mut process = self.lock(&self.process);
            match process.as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(None) => true,
                    Ok(Some(_)) | Err(_) => {
                        *process = None;
                        false
                    }
                },
                None => false,
            }
        };
        if running {
            return Ok(());
        }
        *self.lock(&self.input) = None;
        *self.lock(&self.output) = None;
        let worker = self
            .spawner
            .spawn(&self.spec)
            .map_err(|_| AsrFailure::ModelUnavailable)?;
        *self.lock(&self.input) = Some(worker.input);
        *self.lock(&self.output) = Some(worker.output);
        *self.lock(&self.process) = Some(worker.process);
        Ok(())
    }

    fn cancel_worker(&self) {
        if let Some(process) = self.lock(&self.process).as_mut() {
            let _ = process.kill();
        }
    }

    fn invalidate_worker(&self) {
        {
            let mut process = self.lock(&self.process);
            if let Some(child) = process.as_mut() {
                let _ = child.kill();
            }
            *process = None;
        }
        *self.lock(&self.input) = None;
        *self.lock(&self.output) = None;
    }

    fn lock<'a, T>(&self, mutex: &'a Mutex<T>) -> MutexGuard<'a, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for WorkerClient {
    fn drop(&mut self) {
        if let Ok(process) = self.process.get_mut() {
            if let Some(child) = process.as_mut() {
                let _ = child.kill();
            }
        }
    }
}

pub struct SenseVoiceWorkerModel {
    client: Arc<WorkerClient>,
}

impl SenseVoiceWorkerModel {
    pub fn new(spec: WorkerLaunchSpec) -> Self {
        Self::with_spawner(spec, Arc::new(SystemWorkerSpawner))
    }

    fn with_spawner(spec: WorkerLaunchSpec, spawner: Arc<dyn WorkerSpawner>) -> Self {
        Self {
            client: WorkerClient::new(spec, spawner),
        }
    }

    pub fn stop(&self) {
        self.client.invalidate_worker();
    }
}

impl AsrModel for SenseVoiceWorkerModel {
    fn transcribe<'a>(&'a self, input: AsrModelInput) -> AsrModelFuture<'a> {
        let client = Arc::clone(&self.client);
        Box::pin(async move {
            if input.sample_rate_hz != SAMPLE_RATE_HZ
                || input.samples.is_empty()
                || input.samples.len() > MAX_SAMPLE_COUNT
                || input
                    .samples
                    .iter()
                    .any(|sample| !sample.is_finite() || !(-1.0..=1.0).contains(sample))
            {
                return Err(AsrFailure::InvalidAudio);
            }
            let request_id = client.allocate_request_id()?;
            let cancelled = Arc::new(AtomicBool::new(false));
            let work_client = Arc::clone(&client);
            let work_cancelled = Arc::clone(&cancelled);
            let task = tokio::task::spawn_blocking(move || {
                work_client.transcribe(request_id, input, &work_cancelled)
            });
            let mut cancellation = CancelWorkerOnDrop {
                client,
                cancelled,
                armed: true,
            };
            match task.await {
                Ok(result) => {
                    cancellation.armed = false;
                    result
                }
                Err(_) => Err(AsrFailure::Inference),
            }
        })
    }
}

struct CancelWorkerOnDrop {
    client: Arc<WorkerClient>,
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

impl Drop for CancelWorkerOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
            self.client.cancel_worker();
        }
    }
}

pub fn clean_sensevoice_text(value: &str) -> String {
    let mut text = value.trim();
    while let Some(rest) = text.strip_prefix("<|") {
        let Some(end) = rest.find("|>") else {
            break;
        };
        text = rest[end + 2..].trim_start();
    }
    text.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        io::Cursor,
        sync::{atomic::AtomicUsize, Condvar},
        time::Duration,
    };

    use tempfile::tempdir;
    use tokio::{sync::Notify, time::timeout};

    use super::*;

    fn fixture(root: &Path) -> PathBuf {
        let config = root.join("model.toml");
        fs::write(
            &config,
            "schema_version=1\n[paths]\nmodel_root_env='TEST_SENSEVOICE_ROOT'\ndefault_model_root='%LOCALAPPDATA%/models'\n[sensevoice]\nrust_crate='sherpa-onnx'\nrust_crate_version='1.13.7'\nsample_rate_hz=16000\nfeature_dim=80\nlanguage='auto'\nuse_itn=true\nmodel={relative_path='sensevoice/model.onnx'}\ntokens={relative_path='sensevoice/tokens.txt'}\n",
        )
        .expect("fixture config");
        config
    }

    #[test]
    fn production_config_resolves_the_pinned_sensevoice_artifacts() {
        let directory = tempdir().expect("directory");
        let config = SenseVoiceConfig::load(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../resources/config/model.toml")
                .as_path(),
            Some(directory.path()),
        )
        .expect("production config");
        assert_eq!(
            config.model_path,
            directory.path().join("sensevoice/model.int8.onnx")
        );
        assert_eq!(
            config.tokens_path,
            directory.path().join("sensevoice/tokens.txt")
        );
        assert_eq!(config.sample_rate_hz, 16_000);
        assert_eq!(config.feature_dim, 80);
        assert_eq!(config.language, "auto");
        assert!(config.use_itn);
    }

    #[test]
    fn config_rejects_version_format_and_path_drift() {
        let directory = tempdir().expect("directory");
        for (valid, drifted) in [
            ("rust_crate_version='1.13.7'", "rust_crate_version='1.13.8'"),
            ("sample_rate_hz=16000", "sample_rate_hz=8000"),
            ("feature_dim=80", "feature_dim=40"),
            ("language='auto'", "language='zh'"),
            ("use_itn=true", "use_itn=false"),
            (
                "model={relative_path='sensevoice/model.onnx'}",
                "model={relative_path='../outside.onnx'}",
            ),
        ] {
            let config = fixture(directory.path());
            let text = fs::read_to_string(&config).expect("fixture text");
            fs::write(&config, text.replace(valid, drifted)).expect("drift fixture");
            assert_eq!(
                SenseVoiceConfig::load(&config, Some(directory.path())),
                Err(SenseVoiceError::Config)
            );
        }
    }

    #[test]
    fn artifact_validation_requires_both_files() {
        let directory = tempdir().expect("directory");
        let config_path = fixture(directory.path());
        let config = SenseVoiceConfig::load(&config_path, Some(directory.path())).expect("config");
        assert_eq!(
            config.validate_artifacts(),
            Err(SenseVoiceError::ArtifactMissing)
        );
        fs::create_dir_all(config.model_path.parent().expect("parent")).expect("model dir");
        fs::write(&config.model_path, b"model").expect("model");
        fs::write(&config.tokens_path, b"tokens").expect("tokens");
        assert_eq!(config.validate_artifacts(), Ok(()));
    }

    #[test]
    fn sensevoice_metadata_tags_are_not_forwarded_as_transcript_text() {
        assert_eq!(
            clean_sensevoice_text(" <|zh|><|NEUTRAL|><|Speech|><|woitn|> 你好，Jarvis。 "),
            "你好，Jarvis。"
        );
        assert_eq!(
            clean_sensevoice_text("<|en|><|NEUTRAL|><|Speech|><|itn|>hello"),
            "hello"
        );
        assert_eq!(clean_sensevoice_text("<|zh|><|NEUTRAL|><|nospeech|>"), "");
        assert_eq!(
            clean_sensevoice_text("ordinary <|token|> content"),
            "ordinary <|token|> content"
        );
    }

    #[test]
    fn worker_launch_arguments_preserve_paths_as_distinct_os_values() {
        let directory = tempdir().expect("directory");
        let config_path = fixture(directory.path());
        let config = SenseVoiceConfig::load(&config_path, Some(Path::new("C:\\Models & local")))
            .expect("config");
        let launch = config.worker_launch_spec("C:\\App path\\asr_worker.exe", &config_path);
        assert_eq!(
            launch.executable,
            PathBuf::from("C:\\App path\\asr_worker.exe")
        );
        assert_eq!(
            launch.args,
            vec![
                OsString::from("--config"),
                config_path.into_os_string(),
                OsString::from("--model-root"),
                OsString::from("C:\\Models & local"),
            ]
        );
    }

    #[derive(Default)]
    struct FakeProcessState {
        killed: Mutex<bool>,
        changed: Condvar,
    }

    impl FakeProcessState {
        fn kill(&self) {
            *self.killed.lock().expect("killed state") = true;
            self.changed.notify_all();
        }

        fn is_killed(&self) -> bool {
            *self.killed.lock().expect("killed state")
        }
    }

    struct FakeProcess {
        state: Arc<FakeProcessState>,
    }

    impl WorkerProcess for FakeProcess {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(self.state.is_killed().then(fake_exit_status))
        }

        fn kill(&mut self) -> io::Result<()> {
            self.state.kill();
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

    struct BlockingOutput {
        state: Arc<FakeProcessState>,
        started: Arc<Notify>,
    }

    impl Read for BlockingOutput {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            self.started.notify_one();
            let mut killed = self.state.killed.lock().expect("killed state");
            while !*killed {
                killed = self.state.changed.wait(killed).expect("kill notification");
            }
            Ok(0)
        }
    }

    enum FakeOutput {
        Immediate(Vec<u8>),
        Blocking {
            state: Arc<FakeProcessState>,
            started: Arc<Notify>,
        },
    }

    struct FakeSpawner {
        outputs: Mutex<VecDeque<FakeOutput>>,
        spawn_count: AtomicUsize,
    }

    impl FakeSpawner {
        fn new(outputs: impl IntoIterator<Item = FakeOutput>) -> Arc<Self> {
            Arc::new(Self {
                outputs: Mutex::new(outputs.into_iter().collect()),
                spawn_count: AtomicUsize::new(0),
            })
        }
    }

    impl WorkerSpawner for FakeSpawner {
        fn spawn(&self, _spec: &WorkerLaunchSpec) -> io::Result<SpawnedWorker> {
            self.spawn_count.fetch_add(1, Ordering::AcqRel);
            let output = self
                .outputs
                .lock()
                .expect("fake outputs")
                .pop_front()
                .expect("fake worker output");
            let (state, output): (Arc<FakeProcessState>, Box<dyn Read + Send>) = match output {
                FakeOutput::Immediate(bytes) => (
                    Arc::new(FakeProcessState::default()),
                    Box::new(Cursor::new(bytes)),
                ),
                FakeOutput::Blocking { state, started } => (
                    Arc::clone(&state),
                    Box::new(BlockingOutput { state, started }),
                ),
            };
            Ok(SpawnedWorker {
                process: Box::new(FakeProcess { state }),
                input: Box::new(io::sink()),
                output,
            })
        }
    }

    fn response_bytes(request_id: u64, transcript: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        crate::asr_worker_protocol::write_response(
            &mut bytes,
            &crate::asr_worker_protocol::WorkerResponse {
                request_id,
                status: WorkerStatus::Success,
                transcript: transcript.into(),
            },
        )
        .expect("response");
        bytes
    }

    fn input() -> AsrModelInput {
        AsrModelInput {
            sample_rate_hz: SAMPLE_RATE_HZ,
            samples: vec![0.0; 160],
        }
    }

    fn test_launch() -> WorkerLaunchSpec {
        WorkerLaunchSpec {
            executable: "fake-worker".into(),
            args: Vec::new(),
        }
    }

    #[tokio::test]
    async fn successful_requests_reuse_one_long_lived_worker() {
        let mut responses = response_bytes(1, "first");
        responses.extend(response_bytes(2, "second"));
        let spawner = FakeSpawner::new([FakeOutput::Immediate(responses)]);
        let model = SenseVoiceWorkerModel::with_spawner(test_launch(), spawner.clone());

        assert_eq!(model.transcribe(input()).await, Ok("first".into()));
        assert_eq!(model.transcribe(input()).await, Ok("second".into()));
        assert_eq!(spawner.spawn_count.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn dropping_inference_kills_worker_and_next_request_restarts_cleanly() {
        let first_state = Arc::new(FakeProcessState::default());
        let started = Arc::new(Notify::new());
        let spawner = FakeSpawner::new([
            FakeOutput::Blocking {
                state: Arc::clone(&first_state),
                started: Arc::clone(&started),
            },
            FakeOutput::Immediate(response_bytes(2, "after restart")),
        ]);
        let model = Arc::new(SenseVoiceWorkerModel::with_spawner(
            test_launch(),
            spawner.clone(),
        ));
        let inference = {
            let model = Arc::clone(&model);
            tokio::spawn(async move { model.transcribe(input()).await })
        };
        timeout(Duration::from_secs(1), started.notified())
            .await
            .expect("worker request started");
        inference.abort();
        let _ = inference.await;
        timeout(Duration::from_secs(1), async {
            while !first_state.is_killed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancel killed worker");

        assert_eq!(
            timeout(Duration::from_secs(1), model.transcribe(input()))
                .await
                .expect("restarted request"),
            Ok("after restart".into())
        );
        assert_eq!(spawner.spawn_count.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn invalid_audio_is_rejected_without_spawning_worker() {
        let spawner = FakeSpawner::new([]);
        let model = SenseVoiceWorkerModel::with_spawner(test_launch(), spawner.clone());
        assert_eq!(
            model
                .transcribe(AsrModelInput {
                    sample_rate_hz: 8_000,
                    samples: vec![0.0],
                })
                .await,
            Err(AsrFailure::InvalidAudio)
        );
        assert_eq!(spawner.spawn_count.load(Ordering::Acquire), 0);
    }
}
