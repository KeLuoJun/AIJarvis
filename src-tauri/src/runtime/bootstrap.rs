use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use reqwest::{header, Client, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter};

pub(crate) const BOOTSTRAP_PROGRESS_EVENT: &str = "bootstrap-progress";
const EMIT_INTERVAL: Duration = Duration::from_millis(120);
const DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const SOURCE_DOWNLOAD_ATTEMPTS: usize = 3;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum BootstrapPhase {
    Checking,
    Downloading,
    Verifying,
    Starting,
    Complete,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BootstrapProgress {
    pub phase: BootstrapPhase,
    pub step: String,
    pub label: String,
    pub detail: String,
    pub artifact_index: usize,
    pub artifact_count: usize,
    pub current_bytes: u64,
    pub current_total_bytes: u64,
    pub completed_bytes: u64,
    pub total_bytes: u64,
}

impl BootstrapProgress {
    pub(crate) fn status(
        phase: BootstrapPhase,
        step: impl Into<String>,
        label: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            phase,
            step: step.into(),
            label: label.into(),
            detail: detail.into(),
            artifact_index: 0,
            artifact_count: 0,
            current_bytes: 0,
            current_total_bytes: 0,
            completed_bytes: 0,
            total_bytes: 0,
        }
    }
}

pub(crate) fn emit_progress(app: &AppHandle, progress: BootstrapProgress) {
    if let Err(error) = app.emit(BOOTSTRAP_PROGRESS_EVENT, progress) {
        eprintln!("bootstrap progress event could not be emitted: {error}");
    }
}

#[derive(Debug, Deserialize)]
struct ModelDocument {
    schema_version: u32,
    paths: ModelPaths,
    qwen: QwenDocument,
    sensevoice: SenseVoiceDocument,
}

#[derive(Debug, Deserialize)]
struct ModelPaths {
    model_root_env: String,
    default_model_root: String,
}

#[derive(Debug, Deserialize)]
struct QwenDocument {
    model: ArtifactDocument,
    mmproj: ArtifactDocument,
}

#[derive(Debug, Deserialize)]
struct SenseVoiceDocument {
    model: ArtifactDocument,
    tokens: ArtifactDocument,
}

#[derive(Clone, Debug, Deserialize)]
struct ArtifactDocument {
    relative_path: String,
    size_bytes: u64,
    sha256: String,
    primary_url: String,
    fallback_url: String,
}

#[derive(Clone, Debug)]
struct Artifact {
    id: &'static str,
    label: &'static str,
    path: PathBuf,
    expected_size: u64,
    expected_sha256: String,
    primary_url: String,
    fallback_url: String,
}

#[derive(Debug)]
enum DownloadAttemptError {
    Transient(String),
    Source(String),
    Local(String),
}

impl DownloadAttemptError {
    fn message(&self) -> &str {
        match self {
            Self::Transient(message) | Self::Source(message) | Self::Local(message) => message,
        }
    }
}

pub(crate) async fn prepare_models(app: &AppHandle, config_path: &Path) -> Result<(), String> {
    let artifacts = load_artifacts(config_path)?;
    let total_bytes = artifacts
        .iter()
        .map(|artifact| artifact.expected_size)
        .sum();
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| "无法初始化模型下载器。".to_owned())?;
    let mut completed_bytes = 0;

    for (index, artifact) in artifacts.iter().enumerate() {
        let ordinal = index + 1;
        emit_artifact_progress(
            app,
            BootstrapPhase::Checking,
            artifact,
            ordinal,
            artifacts.len(),
            0,
            completed_bytes,
            total_bytes,
            "检查本地文件",
        );

        if verify_artifact(
            app,
            artifact,
            ordinal,
            artifacts.len(),
            completed_bytes,
            total_bytes,
        )
        .await?
        {
            completed_bytes += artifact.expected_size;
            continue;
        }

        quarantine_invalid_file(&artifact.path)?;
        download_artifact(
            app,
            &client,
            artifact,
            ordinal,
            artifacts.len(),
            completed_bytes,
            total_bytes,
        )
        .await?;
        completed_bytes += artifact.expected_size;
    }

    Ok(())
}

fn load_artifacts(config_path: &Path) -> Result<Vec<Artifact>, String> {
    let text = fs::read_to_string(config_path).map_err(|_| "无法读取模型配置。".to_owned())?;
    let document =
        toml::from_str::<ModelDocument>(&text).map_err(|_| "模型配置格式无效。".to_owned())?;
    if document.schema_version != 1 {
        return Err("模型配置版本不受支持。".to_owned());
    }
    let root = resolve_model_root(&document.paths)?;
    let definitions = [
        ("qwenModel", "Qwen 对话模型", document.qwen.model),
        ("qwenVision", "Qwen 视觉适配器", document.qwen.mmproj),
        (
            "senseVoiceModel",
            "SenseVoice 语音模型",
            document.sensevoice.model,
        ),
        (
            "senseVoiceTokens",
            "SenseVoice 词表",
            document.sensevoice.tokens,
        ),
    ];

    definitions
        .into_iter()
        .map(|(id, label, source)| {
            if source.size_bytes == 0
                || source.sha256.len() != 64
                || !source.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                || !source.primary_url.starts_with("https://")
                || !source.fallback_url.starts_with("https://")
            {
                return Err("模型下载配置无效。".to_owned());
            }
            let relative = Path::new(&source.relative_path);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|part| !matches!(part, Component::Normal(_)))
            {
                return Err("模型文件路径无效。".to_owned());
            }
            Ok(Artifact {
                id,
                label,
                path: root.join(relative),
                expected_size: source.size_bytes,
                expected_sha256: source.sha256.to_ascii_lowercase(),
                primary_url: source.primary_url,
                fallback_url: source.fallback_url,
            })
        })
        .collect()
}

fn resolve_model_root(paths: &ModelPaths) -> Result<PathBuf, String> {
    if paths.model_root_env.trim().is_empty() {
        return Err("模型目录配置无效。".to_owned());
    }
    if let Some(value) = std::env::var_os(&paths.model_root_env) {
        let path = PathBuf::from(value);
        return path
            .is_absolute()
            .then_some(path)
            .ok_or_else(|| "模型目录必须是绝对路径。".to_owned());
    }
    let local = std::env::var("LOCALAPPDATA").map_err(|_| "无法定位本地模型目录。")?;
    let expanded = paths.default_model_root.replace("%LOCALAPPDATA%", &local);
    let path = PathBuf::from(expanded);
    if !path.is_absolute() || path.to_string_lossy().contains('%') {
        return Err("模型目录配置无效。".to_owned());
    }
    Ok(path)
}

async fn verify_artifact(
    app: &AppHandle,
    artifact: &Artifact,
    artifact_index: usize,
    artifact_count: usize,
    completed_bytes: u64,
    total_bytes: u64,
) -> Result<bool, String> {
    let Ok(metadata) = fs::metadata(&artifact.path) else {
        return Ok(false);
    };
    if !metadata.is_file() || metadata.len() != artifact.expected_size {
        return Ok(false);
    }

    let app = app.clone();
    let artifact = artifact.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut file = File::open(&artifact.path).map_err(|_| "无法读取模型文件。".to_owned())?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 4 * 1024 * 1024];
        let mut read_bytes = 0_u64;
        let mut last_emit = Instant::now() - EMIT_INTERVAL;
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|_| "读取模型文件失败。".to_owned())?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            read_bytes += count as u64;
            if last_emit.elapsed() >= EMIT_INTERVAL || read_bytes == artifact.expected_size {
                emit_artifact_progress(
                    &app,
                    BootstrapPhase::Verifying,
                    &artifact,
                    artifact_index,
                    artifact_count,
                    read_bytes,
                    completed_bytes,
                    total_bytes,
                    "校验文件完整性",
                );
                last_emit = Instant::now();
            }
        }
        Ok(format!("{:x}", hasher.finalize()) == artifact.expected_sha256)
    })
    .await
    .map_err(|_| "模型校验任务意外停止。".to_owned())?
}

async fn download_artifact(
    app: &AppHandle,
    client: &Client,
    artifact: &Artifact,
    artifact_index: usize,
    artifact_count: usize,
    completed_bytes: u64,
    total_bytes: u64,
) -> Result<(), String> {
    if let Some(parent) = artifact.path.parent() {
        fs::create_dir_all(parent).map_err(|_| "无法创建模型目录。".to_owned())?;
    }
    let partial = artifact.path.with_extension(format!(
        "{}partial",
        artifact
            .path
            .extension()
            .map(|value| format!("{}.", value.to_string_lossy()))
            .unwrap_or_default()
    ));

    if fs::metadata(&partial)
        .map(|metadata| metadata.len() == artifact.expected_size)
        .unwrap_or(false)
    {
        fs::rename(&partial, &artifact.path)
            .map_err(|_| "无法安装已下载的模型文件。".to_owned())?;
        if verify_artifact(
            app,
            artifact,
            artifact_index,
            artifact_count,
            completed_bytes,
            total_bytes,
        )
        .await?
        {
            return Ok(());
        }
        quarantine_invalid_file(&artifact.path)?;
    }

    let sources = [
        ("镜像源", &artifact.primary_url),
        ("官方源", &artifact.fallback_url),
    ];
    let mut source_failures = Vec::with_capacity(sources.len());
    for (source_label, url) in sources {
        let mut attempt = 1;
        loop {
            emit_download_status(
                app,
                artifact,
                artifact_index,
                artifact_count,
                completed_bytes,
                total_bytes,
                &partial,
                format!("{source_label}：第 {attempt}/{SOURCE_DOWNLOAD_ATTEMPTS} 次连接"),
            );
            let result = download_from_source(
                app,
                client,
                artifact,
                &partial,
                url,
                artifact_index,
                artifact_count,
                completed_bytes,
                total_bytes,
                source_label,
            )
            .await;

            let error = match result {
                Ok(()) => {
                    fs::rename(&partial, &artifact.path)
                        .map_err(|_| "无法安装已下载的模型文件。".to_owned())?;
                    if verify_artifact(
                        app,
                        artifact,
                        artifact_index,
                        artifact_count,
                        completed_bytes,
                        total_bytes,
                    )
                    .await?
                    {
                        return Ok(());
                    }
                    quarantine_invalid_file(&artifact.path)?;
                    DownloadAttemptError::Transient("下载数据未通过完整性校验".to_owned())
                }
                Err(DownloadAttemptError::Local(message)) => return Err(message),
                Err(error) => error,
            };

            if matches!(error, DownloadAttemptError::Transient(_))
                && attempt < SOURCE_DOWNLOAD_ATTEMPTS
            {
                let delay = retry_delay(attempt);
                emit_download_status(
                    app,
                    artifact,
                    artifact_index,
                    artifact_count,
                    completed_bytes,
                    total_bytes,
                    &partial,
                    format!(
                        "{source_label}第 {attempt} 次失败：{}；{} 秒后断点续传",
                        error.message(),
                        delay.as_secs()
                    ),
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
            }

            source_failures.push(format!(
                "{source_label}（尝试 {attempt} 次）：{}",
                error.message()
            ));
            break;
        }
    }
    Err(format!(
        "{}下载失败。{}",
        artifact.label,
        source_failures.join("；")
    ))
}

#[allow(clippy::too_many_arguments)]
async fn download_from_source(
    app: &AppHandle,
    client: &Client,
    artifact: &Artifact,
    partial: &Path,
    url: &str,
    artifact_index: usize,
    artifact_count: usize,
    completed_bytes: u64,
    total_bytes: u64,
    source_label: &str,
) -> Result<(), DownloadAttemptError> {
    let existing = fs::metadata(partial).map(|value| value.len()).unwrap_or(0);
    if existing > artifact.expected_size {
        quarantine_invalid_file(partial).map_err(DownloadAttemptError::Local)?;
    }
    let resume_at = fs::metadata(partial).map(|value| value.len()).unwrap_or(0);
    let mut request = client.get(url);
    if resume_at > 0 {
        request = request.header(header::RANGE, format!("bytes={resume_at}-"));
    }
    let response = request.send().await.map_err(|error| {
        DownloadAttemptError::Transient(format!("连接失败{}", reqwest_error_suffix(&error)))
    })?;
    if !response.status().is_success() {
        let message = format!("服务器返回 HTTP {}", response.status());
        return Err(if retryable_status(response.status()) {
            DownloadAttemptError::Transient(message)
        } else {
            DownloadAttemptError::Source(message)
        });
    }

    let append = resume_at > 0 && response.status() == StatusCode::PARTIAL_CONTENT;
    let mut received = if append { resume_at } else { 0 };
    let mut options = fs::OpenOptions::new();
    options.create(true).write(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    let mut file = options
        .open(partial)
        .map_err(|_| DownloadAttemptError::Local("无法写入模型下载文件。".to_owned()))?;
    let mut stream = response.bytes_stream();
    let mut last_emit = Instant::now() - EMIT_INTERVAL;
    loop {
        let next_chunk = tokio::time::timeout(DOWNLOAD_IDLE_TIMEOUT, stream.next())
            .await
            .map_err(|_| {
                DownloadAttemptError::Transient(format!(
                    "{} 秒未收到数据",
                    DOWNLOAD_IDLE_TIMEOUT.as_secs()
                ))
            })?;
        let Some(chunk) = next_chunk else {
            break;
        };
        let chunk = chunk.map_err(|error| {
            DownloadAttemptError::Transient(format!("传输中断{}", reqwest_error_suffix(&error)))
        })?;
        file.write_all(&chunk).map_err(|_| {
            DownloadAttemptError::Local("写入模型文件失败，请检查磁盘空间。".to_owned())
        })?;
        received = received.saturating_add(chunk.len() as u64);
        if received > artifact.expected_size {
            return Err(DownloadAttemptError::Source(
                "下载大小超出配置预期".to_owned(),
            ));
        }
        if last_emit.elapsed() >= EMIT_INTERVAL || received == artifact.expected_size {
            emit_artifact_progress(
                app,
                BootstrapPhase::Downloading,
                artifact,
                artifact_index,
                artifact_count,
                received,
                completed_bytes,
                total_bytes,
                &format!("正在从{source_label}下载"),
            );
            last_emit = Instant::now();
        }
    }
    file.flush()
        .map_err(|_| DownloadAttemptError::Local("保存模型文件失败。".to_owned()))?;
    if received != artifact.expected_size {
        return Err(DownloadAttemptError::Transient(format!(
            "连接提前结束，已保存 {received}/{} 字节",
            artifact.expected_size
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_download_status(
    app: &AppHandle,
    artifact: &Artifact,
    artifact_index: usize,
    artifact_count: usize,
    completed_bytes: u64,
    total_bytes: u64,
    partial: &Path,
    detail: String,
) {
    let current_bytes = fs::metadata(partial)
        .map(|metadata| metadata.len().min(artifact.expected_size))
        .unwrap_or(0);
    emit_artifact_progress(
        app,
        BootstrapPhase::Downloading,
        artifact,
        artifact_index,
        artifact_count,
        current_bytes,
        completed_bytes,
        total_bytes,
        &detail,
    );
}

fn retry_delay(failed_attempt: usize) -> Duration {
    Duration::from_secs(1_u64 << failed_attempt.saturating_sub(1).min(2))
}

fn retryable_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn reqwest_error_suffix(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "（超时）"
    } else if error.is_connect() {
        "（无法建立连接）"
    } else {
        ""
    }
}

fn quarantine_invalid_file(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let quarantine = path.with_extension(format!(
        "{}invalid.{timestamp}",
        path.extension()
            .map(|value| format!("{}.", value.to_string_lossy()))
            .unwrap_or_default()
    ));
    fs::rename(path, quarantine).map_err(|_| "无法隔离损坏的模型文件。".to_owned())
}

#[allow(clippy::too_many_arguments)]
fn emit_artifact_progress(
    app: &AppHandle,
    phase: BootstrapPhase,
    artifact: &Artifact,
    artifact_index: usize,
    artifact_count: usize,
    current_bytes: u64,
    completed_bytes: u64,
    total_bytes: u64,
    detail: &str,
) {
    emit_progress(
        app,
        BootstrapProgress {
            phase,
            step: artifact.id.to_owned(),
            label: artifact.label.to_owned(),
            detail: detail.to_owned(),
            artifact_index,
            artifact_count,
            current_bytes,
            current_total_bytes: artifact.expected_size,
            completed_bytes: completed_bytes.saturating_add(current_bytes),
            total_bytes,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_model_paths_that_escape_the_root() {
        let source = ArtifactDocument {
            relative_path: "../outside.gguf".to_owned(),
            size_bytes: 1,
            sha256: "a".repeat(64),
            primary_url: "https://example.test/a".to_owned(),
            fallback_url: "https://example.test/b".to_owned(),
        };
        let relative = Path::new(&source.relative_path);
        assert!(relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_))));
    }

    #[test]
    fn status_progress_has_no_fake_download_total() {
        let status = BootstrapProgress::status(
            BootstrapPhase::Checking,
            "resources",
            "运行资源",
            "检查配置",
        );
        assert_eq!(status.total_bytes, 0);
        assert_eq!(status.artifact_count, 0);
    }

    #[test]
    fn transient_source_failures_use_bounded_backoff() {
        assert_eq!(SOURCE_DOWNLOAD_ATTEMPTS, 3);
        assert_eq!(DOWNLOAD_IDLE_TIMEOUT, Duration::from_secs(30));
        assert_eq!(retry_delay(1), Duration::from_secs(1));
        assert_eq!(retry_delay(2), Duration::from_secs(2));
        assert_eq!(retry_delay(3), Duration::from_secs(4));
        assert_eq!(retry_delay(20), Duration::from_secs(4));
    }

    #[test]
    fn retries_only_transient_http_statuses() {
        assert!(retryable_status(StatusCode::REQUEST_TIMEOUT));
        assert!(retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(retryable_status(StatusCode::BAD_GATEWAY));
        assert!(!retryable_status(StatusCode::NOT_FOUND));
        assert!(!retryable_status(StatusCode::UNAUTHORIZED));
    }
}
