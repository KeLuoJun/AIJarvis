//! Debug-only diagnostics. No dependency on capture, inference, Tauri, or application state.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender},
        OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};

const QUEUE_CAPACITY: usize = 256;
const MAX_RECORD_BYTES: usize = 4096;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
static LOGGER: OnceLock<Logger> = OnceLock::new();

enum Message {
    Event(Value),
    Stop(mpsc::Sender<()>),
}

struct Logger {
    sender: SyncSender<Message>,
    started: Instant,
    dropped: AtomicU64,
    stopped: AtomicBool,
}

pub(crate) fn initialize(directory: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let path = directory.join(format!("dev-{}.jsonl", std::process::id()));
    if LOGGER.get().is_some() {
        return Ok(path);
    }
    let mut writer = RotatingWriter::new(path.clone(), MAX_FILE_BYTES)?;
    let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
    std::thread::Builder::new()
        .name("dev-log-writer".into())
        .spawn(move || {
            while let Ok(message) = receiver.recv() {
                match message {
                    Message::Event(event) => {
                        if writer.write(&event).is_err() {
                            eprintln!("Development log write failed; logging stopped.");
                            break;
                        }
                    }
                    Message::Stop(ack) => {
                        let _ = ack.send(());
                        break;
                    }
                }
            }
        })?;
    let _ = LOGGER.set(Logger {
        sender,
        started: Instant::now(),
        dropped: AtomicU64::new(0),
        stopped: AtomicBool::new(false),
    });
    record("log.started", || json!({"schema_version": 1}));
    Ok(path)
}

pub(crate) fn record(event: &'static str, fields: impl FnOnce() -> Value) {
    let Some(logger) = LOGGER.get() else { return };
    if logger.stopped.load(Ordering::Relaxed) {
        return;
    }
    let dropped = logger.dropped.swap(0, Ordering::Relaxed);
    let record = json!({
        "unix_ms": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),
        "elapsed_ms": logger.started.elapsed().as_millis(),
        "pid": std::process::id(), "event": event,
        "dropped_before": dropped, "fields": fields(),
    });
    if logger.sender.try_send(Message::Event(record)).is_err() {
        logger
            .dropped
            .fetch_add(dropped.saturating_add(1), Ordering::Relaxed);
    }
}

pub(crate) fn shutdown() {
    let Some(logger) = LOGGER.get() else { return };
    if logger.stopped.swap(true, Ordering::Relaxed) {
        return;
    }
    let (ack, receiver) = mpsc::channel();
    if logger.sender.try_send(Message::Stop(ack)).is_ok() {
        let _ = receiver.recv_timeout(Duration::from_millis(250));
    }
}

struct RotatingWriter {
    path: PathBuf,
    file: Option<File>,
    length: u64,
    limit: u64,
}

impl RotatingWriter {
    fn new(path: PathBuf, limit: u64) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let length = file.metadata()?.len();
        Ok(Self {
            path,
            file: Some(file),
            length,
            limit,
        })
    }

    fn write(&mut self, event: &Value) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(event)?;
        if bytes.len() > MAX_RECORD_BYTES {
            bytes = serde_json::to_vec(&json!({
                "event": "log.record_too_large", "omitted_bytes": bytes.len()
            }))?;
        }
        bytes.push(b'\n');
        if self.length + bytes.len() as u64 > self.limit && self.length > 0 {
            self.file.take();
            let previous = self.path.with_extension("previous.jsonl");
            match fs::remove_file(&previous) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            fs::rename(&self.path, previous)?;
            self.file = Some(File::create(&self.path)?);
            self.length = 0;
        }
        self.file
            .as_mut()
            .expect("open log file")
            .write_all(&bytes)?;
        self.length += bytes.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_are_structured_rotated_and_oversized_payloads_are_omitted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("dev-test.jsonl");
        let mut writer = RotatingWriter::new(path.clone(), 160).unwrap();
        for sequence in 0..30 {
            writer
                .write(&json!({"event":"test", "sequence":sequence}))
                .unwrap();
        }
        drop(writer);
        for file in [path.clone(), path.with_extension("previous.jsonl")] {
            let content = fs::read_to_string(file).unwrap();
            assert!(content.len() <= 160);
            assert!(content
                .lines()
                .all(|line| serde_json::from_str::<Value>(line).is_ok()));
        }
        let mut writer = RotatingWriter::new(path.clone(), MAX_FILE_BYTES).unwrap();
        writer
            .write(&json!({"body": "sensitive".repeat(MAX_RECORD_BYTES)}))
            .unwrap();
        drop(writer);
        let content = fs::read_to_string(path).unwrap();
        assert!(content.contains("log.record_too_large"));
        assert!(!content.contains("sensitive"));
    }
}
