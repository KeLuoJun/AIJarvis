use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Serialize;
use serde_json::{json, Value};

use crate::{
    llama::{HttpTransport, LlamaError, LlamaService, ModelOutput, ModelRequest, ProcessSpawner},
    prompts::PromptId,
};

const NOTE_DIRECTORY_NAME: &str = "AI Jarvis 课程笔记";
const MAX_MODEL_IMAGES: usize = 4;
const MAX_TRANSCRIPT_CHARS: usize = 48_000;
const MIN_COURSE_SUMMARY_DURATION: Duration = Duration::from_secs(30);
const RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CourseNoteReady {
    pub title: String,
    pub path: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CourseNoteFailed {
    pub message: String,
}

pub trait CourseNoteEventSink: Send + Sync {
    fn ready(&self, event: CourseNoteReady);
    fn failed(&self, event: CourseNoteFailed);
}

pub struct NullCourseNoteEventSink;

impl CourseNoteEventSink for NullCourseNoteEventSink {
    fn ready(&self, _event: CourseNoteReady) {}
    fn failed(&self, _event: CourseNoteFailed) {}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CourseSummaryError {
    ContextTooLarge,
    NotReady,
    Failed,
}

pub type CourseSummaryFuture<'a> = Pin<
    Box<
        dyn Future<Output = Result<crate::llama::CourseSummaryOutput, CourseSummaryError>>
            + Send
            + 'a,
    >,
>;

pub trait CourseSummaryModel: Send + Sync {
    fn summarize<'a>(
        &'a self,
        transcript: String,
        image_names: Vec<String>,
        image_data_urls: Vec<String>,
    ) -> CourseSummaryFuture<'a>;
}

pub struct UnavailableCourseSummaryModel;

impl CourseSummaryModel for UnavailableCourseSummaryModel {
    fn summarize<'a>(
        &'a self,
        _transcript: String,
        _image_names: Vec<String>,
        _image_data_urls: Vec<String>,
    ) -> CourseSummaryFuture<'a> {
        Box::pin(async { Err(CourseSummaryError::NotReady) })
    }
}

pub struct LlamaCourseSummaryModel<S, T> {
    service: Arc<LlamaService<S, T>>,
}

impl<S, T> LlamaCourseSummaryModel<S, T> {
    pub fn new(service: Arc<LlamaService<S, T>>) -> Self {
        Self { service }
    }
}

impl<S, T> CourseSummaryModel for LlamaCourseSummaryModel<S, T>
where
    S: ProcessSpawner + 'static,
    T: HttpTransport + 'static,
{
    fn summarize<'a>(
        &'a self,
        transcript: String,
        image_names: Vec<String>,
        image_data_urls: Vec<String>,
    ) -> CourseSummaryFuture<'a> {
        Box::pin(async move {
            let variables = BTreeMap::from([
                (
                    "course_transcript".into(),
                    Value::String(transcript.chars().take(MAX_TRANSCRIPT_CHARS).collect()),
                ),
                ("course_images".into(), json!(image_names)),
            ]);
            match self
                .service
                .request(
                    ModelRequest {
                        prompt: PromptId::CourseSummary,
                        variables,
                        image_data_urls,
                    },
                    Duration::from_secs(180),
                )
                .await
                .map_err(|error| match error {
                    LlamaError::BadRequest => CourseSummaryError::ContextTooLarge,
                    LlamaError::NotReady => CourseSummaryError::NotReady,
                    _ => CourseSummaryError::Failed,
                })? {
                ModelOutput::CourseSummary(output) => Ok(output),
                _ => Err(CourseSummaryError::Failed),
            }
        })
    }
}

struct ActiveCourse {
    id: u64,
    directory: PathBuf,
    frame_count: u64,
    last_frame_at: Option<Instant>,
    started_at: CourseStartedAt,
}

enum CourseStartedAt {
    Monotonic(Instant),
    UnixMillis(u64),
}

impl CourseStartedAt {
    fn elapsed(&self) -> Duration {
        match self {
            Self::Monotonic(started_at) => started_at.elapsed(),
            Self::UnixMillis(started_at) => {
                Duration::from_millis(unix_time_millis().saturating_sub(*started_at))
            }
        }
    }
}

struct CourseNotesState {
    active: Option<ActiveCourse>,
    completion_id: u64,
    processing: bool,
    retry_not_before: Option<Instant>,
    notified_failures: BTreeSet<u64>,
}

pub struct CourseNotes {
    work_root: Option<PathBuf>,
    desktop_root: Option<PathBuf>,
    model: Arc<dyn CourseSummaryModel>,
    sink: Arc<dyn CourseNoteEventSink>,
    state: Mutex<CourseNotesState>,
}

impl CourseNotes {
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self {
            work_root: None,
            desktop_root: None,
            model: Arc::new(UnavailableCourseSummaryModel),
            sink: Arc::new(NullCourseNoteEventSink),
            state: Mutex::new(CourseNotesState {
                active: None,
                completion_id: 0,
                processing: false,
                retry_not_before: None,
                notified_failures: BTreeSet::new(),
            }),
        })
    }

    pub fn new(
        work_root: PathBuf,
        desktop_root: PathBuf,
        model: Arc<dyn CourseSummaryModel>,
        sink: Arc<dyn CourseNoteEventSink>,
    ) -> Arc<Self> {
        Arc::new(Self {
            work_root: Some(work_root),
            desktop_root: Some(desktop_root),
            model,
            sink,
            state: Mutex::new(CourseNotesState {
                active: None,
                completion_id: 0,
                processing: false,
                retry_not_before: None,
                notified_failures: BTreeSet::new(),
            }),
        })
    }

    pub fn sync(self: &Arc<Self>, active: bool, completion_id: u64) {
        let Some(work_root) = &self.work_root else {
            return;
        };
        let pending = {
            let mut state = self.lock_state();
            if active && state.active.is_none() {
                let id = unix_time_millis();
                let directory = work_root.join(format!("course-{id}"));
                if fs::create_dir_all(directory.join("images")).is_ok() {
                    crate::dev_event!("course_notes.session_started", "session_id" => id,
                        "directory" => directory.to_string_lossy());
                    state.active = Some(ActiveCourse {
                        id,
                        directory,
                        frame_count: 0,
                        last_frame_at: None,
                        started_at: CourseStartedAt::Monotonic(Instant::now()),
                    });
                }
            }
            let finished = if completion_id > state.completion_id {
                state.completion_id = completion_id;
                state.active.take()
            } else {
                None
            };
            let retry_ready = state
                .retry_not_before
                .map_or(true, |deadline| Instant::now() >= deadline);
            let recovered = if finished.is_none() && !active && !state.processing && retry_ready {
                find_pending_course(work_root)
            } else {
                None
            };
            let pending = finished.or(recovered);
            if pending.is_some() && !state.processing {
                state.processing = true;
            }
            pending
        };
        if let Some(session) = pending {
            let service = Arc::clone(self);
            tauri::async_runtime::spawn(async move {
                let session_id = session.id;
                let success = service.finish(session).await;
                let mut state = service.lock_state();
                state.processing = false;
                state.retry_not_before = (!success).then(|| Instant::now() + RETRY_DELAY);
                let notify_failure = if success {
                    state.notified_failures.remove(&session_id);
                    false
                } else {
                    state.notified_failures.insert(session_id)
                };
                drop(state);
                if notify_failure {
                    service.sink.failed(CourseNoteFailed {
                        message: "课程材料已保留，但笔记暂时生成失败，系统会在后台自动重试。"
                            .into(),
                    });
                }
            });
        }
    }

    pub fn record(&self, image_data_urls: &[String], audio_observations: &str) {
        let mut state = self.lock_state();
        let Some(session) = state.active.as_mut() else {
            return;
        };
        append_transcript(&session.directory, audio_observations);
        if image_data_urls.is_empty()
            || session
                .last_frame_at
                .is_some_and(|at| at.elapsed() < Duration::from_secs(6))
        {
            return;
        }
        let Some(bytes) = decode_data_url(image_data_urls.last().expect("checked non-empty"))
        else {
            return;
        };
        session.frame_count = session.frame_count.saturating_add(1);
        let name = format!("frame-{:06}.png", session.frame_count);
        if fs::write(session.directory.join("images").join(name), bytes).is_ok() {
            session.last_frame_at = Some(Instant::now());
        }
    }

    async fn finish(&self, session: ActiveCourse) -> bool {
        let duration = session.started_at.elapsed();
        if !should_generate_course_summary(duration) {
            let _ = fs::remove_dir_all(&session.directory);
            crate::dev_event!("course_notes.skipped", "session_id" => session.id,
                "duration_seconds" => duration.as_secs(), "reason" => "course_too_short");
            return true;
        }
        let transcript =
            fs::read_to_string(session.directory.join("transcript.txt")).unwrap_or_default();
        let images = list_images(&session.directory.join("images"));
        let selected = evenly_spaced(&images, MAX_MODEL_IMAGES);
        let image_inputs = selected
            .iter()
            .filter_map(|path| {
                let name = path.file_name()?.to_str()?.to_owned();
                let bytes = fs::read(path).ok()?;
                Some((
                    name,
                    format!("data:image/png;base64,{}", STANDARD.encode(bytes)),
                ))
            })
            .collect::<Vec<_>>();
        crate::dev_event!("course_notes.generation_started", "session_id" => session.id,
            "transcript_chars" => transcript.chars().count(), "recorded_images" => images.len(),
            "selected_images" => image_inputs.len());
        let mut attempt_size = image_inputs.len();
        let summary = loop {
            let attempt_images = evenly_spaced(&image_inputs, attempt_size);
            let names = attempt_images
                .iter()
                .map(|(name, _)| name.clone())
                .collect();
            let data_urls = attempt_images
                .iter()
                .map(|(_, data)| data.clone())
                .collect();
            match self
                .model
                .summarize(transcript.clone(), names, data_urls)
                .await
            {
                Ok(summary) => break summary,
                Err(CourseSummaryError::ContextTooLarge) if attempt_size > 1 => {
                    attempt_size = (attempt_size / 2).max(1);
                    crate::dev_event!("course_notes.context_retry", "session_id" => session.id,
                        "selected_images" => attempt_size);
                }
                Err(_error) => {
                    crate::dev_event!("course_notes.generation_failed", "session_id" => session.id,
                        "error" => format!("{_error:?}"), "retry_seconds" => RETRY_DELAY.as_secs());
                    return false;
                }
            }
        };
        let Some(desktop_root) = &self.desktop_root else {
            crate::dev_event!("course_notes.write_failed", "session_id" => session.id,
                "error" => "desktop_unavailable");
            return false;
        };
        let note_root = desktop_root.join(NOTE_DIRECTORY_NAME);
        let note_dir = note_root.join(format!("{}-{}", session.id, sanitize_title(&summary.title)));
        if fs::create_dir_all(&note_root).is_err()
            || copy_directory(&session.directory.join("images"), &note_dir.join("images")).is_err()
        {
            crate::dev_event!("course_notes.write_failed", "session_id" => session.id,
                "error" => "directory_or_image_copy");
            return false;
        }
        let temporary = note_dir.join("笔记.md.tmp");
        let final_path = note_dir.join("笔记.md");
        let document = format!(
            "# {}\n\n{}\n",
            summary.title.trim(),
            summary.markdown.trim()
        );
        if fs::write(&temporary, document).is_err()
            || fs::rename(&temporary, &final_path).is_err()
            || fs::read_to_string(&final_path).is_err()
        {
            crate::dev_event!("course_notes.write_failed", "session_id" => session.id,
                "error" => "note_commit_or_readback");
            return false;
        }
        let _ = fs::remove_dir_all(&session.directory);
        crate::dev_event!("course_notes.ready", "session_id" => session.id,
            "path" => final_path.to_string_lossy());
        self.sink.ready(CourseNoteReady {
            title: summary.title,
            path: final_path.to_string_lossy().into_owned(),
            message: "刚才的网课笔记已经整理好，放到桌面啦。".into(),
        });
        true
    }

    fn lock_state(&self) -> MutexGuard<'_, CourseNotesState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn find_pending_course(work_root: &Path) -> Option<ActiveCourse> {
    let mut sessions = fs::read_dir(work_root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            if !entry.file_type().ok()?.is_dir() {
                return None;
            }
            let name = entry.file_name();
            let id = name.to_str()?.strip_prefix("course-")?.parse().ok()?;
            let directory = entry.path();
            let frame_count = list_images(&directory.join("images")).len() as u64;
            (directory.join("transcript.txt").is_file() || frame_count > 0).then_some(
                ActiveCourse {
                    id,
                    directory,
                    frame_count,
                    last_frame_at: None,
                    started_at: CourseStartedAt::UnixMillis(id),
                },
            )
        })
        .collect::<Vec<_>>();
    sessions.sort_by_key(|session| session.id);
    let session = sessions.into_iter().next()?;
    crate::dev_event!("course_notes.recovered", "session_id" => session.id,
        "directory" => session.directory.to_string_lossy(), "recorded_images" => session.frame_count);
    Some(session)
}

fn unix_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn should_generate_course_summary(duration: Duration) -> bool {
    // A one-minute session is long enough to produce useful notes. Using a
    // strict `>` here discarded sessions ending exactly at the configured
    // threshold, which is common when the confirmation timeout ends them.
    duration >= MIN_COURSE_SUMMARY_DURATION
}

fn append_transcript(directory: &Path, input: &str) {
    let Ok(Value::Array(entries)) = serde_json::from_str(input) else {
        return;
    };
    let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("transcript.txt"))
    else {
        return;
    };
    for entry in entries {
        if let Some(text) = entry.get("text").and_then(Value::as_str) {
            let text = text.trim();
            if !text.is_empty() {
                let _ = writeln!(file, "{text}");
            }
        }
    }
}

fn decode_data_url(value: &str) -> Option<Vec<u8>> {
    let encoded = value.strip_prefix("data:image/png;base64,")?;
    STANDARD
        .decode(encoded)
        .ok()
        .filter(|bytes| bytes.len() <= 16 * 1024 * 1024)
}

fn list_images(directory: &Path) -> Vec<PathBuf> {
    let mut images = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("png"))
        .collect::<Vec<_>>();
    images.sort();
    images
}

fn evenly_spaced<T: Clone>(paths: &[T], limit: usize) -> Vec<T> {
    if paths.len() <= limit {
        return paths.to_vec();
    }
    if limit == 1 {
        return paths.last().cloned().into_iter().collect();
    }
    (0..limit)
        .map(|index| paths[index * (paths.len() - 1) / (limit - 1)].clone())
        .collect()
}

fn sanitize_title(title: &str) -> String {
    let value = title
        .chars()
        .filter(|character| {
            !matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            )
        })
        .take(48)
        .collect::<String>();
    let value = value.trim().trim_end_matches('.');
    if value.is_empty() {
        "课程笔记".into()
    } else {
        value.into()
    }
}

fn copy_directory(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            fs::copy(entry.path(), target.join(entry.file_name()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeModel;

    impl CourseSummaryModel for FakeModel {
        fn summarize<'a>(
            &'a self,
            transcript: String,
            image_names: Vec<String>,
            _image_data_urls: Vec<String>,
        ) -> CourseSummaryFuture<'a> {
            Box::pin(async move {
                assert!(transcript.contains("矩阵表示线性变换"));
                assert_eq!(image_names, vec!["frame-000001.png"]);
                Ok(crate::llama::CourseSummaryOutput {
                    title: "矩阵/线性变换".into(),
                    markdown: "## 重点内容\n![板书](images/frame-000001.png)".into(),
                })
            })
        }
    }

    struct ContextLimitedModel {
        attempts: AtomicUsize,
    }

    impl CourseSummaryModel for ContextLimitedModel {
        fn summarize<'a>(
            &'a self,
            _transcript: String,
            image_names: Vec<String>,
            _image_data_urls: Vec<String>,
        ) -> CourseSummaryFuture<'a> {
            Box::pin(async move {
                self.attempts.fetch_add(1, Ordering::Relaxed);
                if image_names.len() > 1 {
                    return Err(CourseSummaryError::ContextTooLarge);
                }
                Ok(crate::llama::CourseSummaryOutput {
                    title: "上下文降级".into(),
                    markdown: "## 重点内容\n已减少图片后生成。".into(),
                })
            })
        }
    }

    struct CountingModel {
        attempts: AtomicUsize,
    }

    impl CourseSummaryModel for CountingModel {
        fn summarize<'a>(
            &'a self,
            _transcript: String,
            _image_names: Vec<String>,
            _image_data_urls: Vec<String>,
        ) -> CourseSummaryFuture<'a> {
            Box::pin(async move {
                self.attempts.fetch_add(1, Ordering::Relaxed);
                Ok(crate::llama::CourseSummaryOutput {
                    title: "短课程".into(),
                    markdown: "不应生成".into(),
                })
            })
        }
    }

    #[derive(Default)]
    struct FakeSink(Mutex<Vec<CourseNoteReady>>);

    impl CourseNoteEventSink for FakeSink {
        fn ready(&self, event: CourseNoteReady) {
            self.0.lock().unwrap().push(event);
        }

        fn failed(&self, _event: CourseNoteFailed) {}
    }

    #[tokio::test]
    async fn completed_course_is_summarized_on_desktop_and_work_data_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        let desktop = root.path().join("desktop");
        let session_dir = work.join("course-7");
        fs::create_dir_all(session_dir.join("images")).unwrap();
        fs::write(session_dir.join("transcript.txt"), "矩阵表示线性变换\n").unwrap();
        fs::write(session_dir.join("images/frame-000001.png"), b"png").unwrap();
        let sink = Arc::new(FakeSink::default());
        let service = CourseNotes::new(work, desktop.clone(), Arc::new(FakeModel), sink.clone());

        service
            .finish(ActiveCourse {
                id: 7,
                directory: session_dir.clone(),
                frame_count: 1,
                last_frame_at: None,
                started_at: CourseStartedAt::UnixMillis(0),
            })
            .await;

        let events = sink.0.lock().unwrap();
        assert_eq!(events.len(), 1);
        let note = PathBuf::from(&events[0].path);
        assert!(note.starts_with(desktop.join(NOTE_DIRECTORY_NAME)));
        assert!(fs::read_to_string(note).unwrap().contains("## 重点内容"));
        assert!(!session_dir.exists());
    }

    #[tokio::test]
    async fn course_not_exceeding_one_minute_is_discarded_without_summary() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        let desktop = root.path().join("desktop");
        let model = Arc::new(CountingModel {
            attempts: AtomicUsize::new(0),
        });
        let sink = Arc::new(FakeSink::default());
        let service = CourseNotes::new(work.clone(), desktop.clone(), model.clone(), sink.clone());

        service.sync(true, 0);
        service.record(&[], r#"[{"text":"简短课程内容"}]"#);
        service.sync(false, 1);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !service.lock_state().processing {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("short course should be discarded promptly");

        assert_eq!(model.attempts.load(Ordering::Relaxed), 0);
        assert!(sink.0.lock().unwrap().is_empty());
        assert!(!desktop.exists());
        assert!(fs::read_dir(work).unwrap().next().is_none());
    }

    #[test]
    fn summary_duration_must_reach_thirty_seconds() {
        assert!(!should_generate_course_summary(Duration::from_millis(
            29_999
        )));
        assert!(should_generate_course_summary(Duration::from_secs(30)));
        assert!(should_generate_course_summary(Duration::from_millis(
            30_001
        )));
    }

    #[tokio::test]
    async fn context_limit_retries_with_fewer_images() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        let desktop = root.path().join("desktop");
        let session_dir = work.join("course-8");
        fs::create_dir_all(session_dir.join("images")).unwrap();
        fs::write(session_dir.join("transcript.txt"), "课程内容\n").unwrap();
        for index in 1..=4 {
            fs::write(
                session_dir
                    .join("images")
                    .join(format!("frame-{index:06}.png")),
                b"png",
            )
            .unwrap();
        }
        let model = Arc::new(ContextLimitedModel {
            attempts: AtomicUsize::new(0),
        });
        let sink = Arc::new(FakeSink::default());
        let service = CourseNotes::new(work, desktop, model.clone(), sink.clone());

        service
            .finish(ActiveCourse {
                id: 8,
                directory: session_dir.clone(),
                frame_count: 4,
                last_frame_at: None,
                started_at: CourseStartedAt::UnixMillis(0),
            })
            .await;

        assert!(model.attempts.load(Ordering::Relaxed) > 1);
        assert_eq!(sink.0.lock().unwrap().len(), 1);
        assert!(!session_dir.exists());
    }

    #[tokio::test]
    async fn sync_recovers_an_unfinished_course_from_disk() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        let desktop = root.path().join("desktop");
        let session_dir = work.join("course-9");
        fs::create_dir_all(session_dir.join("images")).unwrap();
        fs::write(session_dir.join("transcript.txt"), "矩阵表示线性变换\n").unwrap();
        fs::write(session_dir.join("images/frame-000001.png"), b"png").unwrap();
        let sink = Arc::new(FakeSink::default());
        let service = CourseNotes::new(work, desktop, Arc::new(FakeModel), sink.clone());

        service.sync(false, 0);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !sink.0.lock().unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("stale course should be recovered");

        assert!(!session_dir.exists());
    }
}
