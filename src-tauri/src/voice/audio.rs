use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use crate::error::{AppError, ErrorCode};

pub const REQUIRED_SAMPLE_RATE_HZ: u32 = 16_000;
pub const REQUIRED_CHANNELS: u16 = 1;
pub const MICROPHONE_HARD_MAX_SECONDS: u64 = 60;
pub const MICROPHONE_HARD_MAX_BYTES: usize = 2 * 1024 * 1024;
pub const SYSTEM_AUDIO_HARD_MAX_SECONDS: u64 = 60;

static RECORDING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioRuntimeConfig {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub microphone_max_seconds: u64,
    pub microphone_max_bytes: usize,
    pub system_audio_ring_seconds: u64,
    pub system_audio_segment_capacity: usize,
    pub user_voice_queue_capacity: usize,
    pub system_audio_queue_capacity: usize,
}

impl AudioRuntimeConfig {
    pub fn is_valid(self) -> bool {
        let Some(max_samples) =
            (self.sample_rate_hz as u64).checked_mul(self.microphone_max_seconds)
        else {
            return false;
        };
        self.sample_rate_hz == REQUIRED_SAMPLE_RATE_HZ
            && self.channels == REQUIRED_CHANNELS
            && (1..=MICROPHONE_HARD_MAX_SECONDS).contains(&self.microphone_max_seconds)
            && (2..=MICROPHONE_HARD_MAX_BYTES).contains(&self.microphone_max_bytes)
            && (1..=SYSTEM_AUDIO_HARD_MAX_SECONDS).contains(&self.system_audio_ring_seconds)
            && self.system_audio_segment_capacity > 0
            && self.user_voice_queue_capacity > 0
            && self.system_audio_queue_capacity > 0
            && max_samples <= usize::MAX as u64
    }

    pub fn microphone_sample_limit(self) -> usize {
        let duration_limit = self.sample_rate_hz as usize * self.microphone_max_seconds as usize;
        duration_limit.min(self.microphone_max_bytes / std::mem::size_of::<i16>())
    }

    pub fn system_audio_sample_limit(self) -> usize {
        self.sample_rate_hz as usize * self.system_audio_ring_seconds as usize
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pcm16Mono {
    samples: Vec<i16>,
}

impl Pcm16Mono {
    fn new(samples: Vec<i16>) -> Result<Self, AppError> {
        if samples.is_empty() {
            return Err(AppError::invalid_argument("Audio input is empty."));
        }
        Ok(Self { samples })
    }

    pub fn samples(&self) -> &[i16] {
        &self.samples
    }

    pub fn byte_len(&self) -> usize {
        self.samples.len() * std::mem::size_of::<i16>()
    }

    pub fn normalized_samples(&self) -> Vec<f32> {
        self.samples
            .iter()
            .map(|sample| f32::from(*sample) / 32_768.0)
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemAudioSegment {
    pub sequence: u64,
    pub captured_at_ms: u64,
    pcm: Pcm16Mono,
}

impl SystemAudioSegment {
    pub fn new(
        sequence: u64,
        captured_at_ms: u64,
        samples: Vec<i16>,
        config: AudioRuntimeConfig,
    ) -> Result<Self, AppError> {
        if samples.len() > config.system_audio_sample_limit() {
            return Err(AppError::invalid_argument(
                "System audio exceeds the bounded ring duration.",
            ));
        }
        Ok(Self {
            sequence,
            captured_at_ms,
            pcm: Pcm16Mono::new(samples)?,
        })
    }

    pub fn pcm(&self) -> &Pcm16Mono {
        &self.pcm
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserVoiceSegment {
    pub request_id: u64,
    pcm: Pcm16Mono,
}

#[derive(Default)]
struct UserVoiceGateState {
    next_request_id: u64,
    active_request_id: Option<u64>,
}

#[derive(Clone, Default)]
pub struct UserVoiceGate {
    state: Arc<Mutex<UserVoiceGateState>>,
}

impl UserVoiceGate {
    pub fn reserve(&self) -> Result<UserVoiceReservation, AppError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.active_request_id.is_some() {
            return Err(AppError::new(
                ErrorCode::QueueFull,
                "The user voice ASR lane is full.",
            ));
        }
        state.next_request_id = state.next_request_id.checked_add(1).ok_or_else(|| {
            AppError::new(
                ErrorCode::GenerationExhausted,
                "The user voice request identifier was exhausted.",
            )
        })?;
        let request_id = state.next_request_id;
        state.active_request_id = Some(request_id);
        Ok(UserVoiceReservation {
            permit: Some(UserVoicePermit {
                request_id,
                state: Arc::clone(&self.state),
            }),
        })
    }

    pub fn is_reserved(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_request_id
            .is_some()
    }
}

struct UserVoicePermit {
    request_id: u64,
    state: Arc<Mutex<UserVoiceGateState>>,
}

impl Drop for UserVoicePermit {
    fn drop(&mut self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.active_request_id == Some(self.request_id) {
            state.active_request_id = None;
        }
    }
}

pub struct UserVoiceReservation {
    permit: Option<UserVoicePermit>,
}

impl UserVoiceReservation {
    pub fn request_id(&self) -> u64 {
        self.permit
            .as_ref()
            .expect("voice reservation always owns its permit")
            .request_id
    }

    pub fn start_recording(
        mut self,
        store: &MicrophoneTempStore,
        config: AudioRuntimeConfig,
    ) -> Result<ReservedMicrophoneRecording, AppError> {
        let recording = store.start(self.request_id(), config)?;
        Ok(ReservedMicrophoneRecording {
            recording,
            permit: self.permit.take(),
        })
    }
}

pub struct ReservedMicrophoneRecording {
    recording: MicrophoneRecording,
    permit: Option<UserVoicePermit>,
}

impl ReservedMicrophoneRecording {
    pub fn request_id(&self) -> u64 {
        self.recording.request_id
    }

    pub fn append(&mut self, samples: &[i16]) -> Result<RecordingAppendOutcome, AppError> {
        self.recording.append(samples)
    }

    pub fn finish(mut self) -> Result<ReservedUserVoiceSegment, AppError> {
        let segment = self.recording.finish()?;
        Ok(ReservedUserVoiceSegment {
            segment,
            _permit: self.permit.take(),
        })
    }

    #[cfg(test)]
    fn path(&self) -> &Path {
        self.recording.path()
    }
}

pub struct ReservedUserVoiceSegment {
    segment: UserVoiceSegment,
    _permit: Option<UserVoicePermit>,
}

impl ReservedUserVoiceSegment {
    pub fn segment(&self) -> &UserVoiceSegment {
        &self.segment
    }
}

impl UserVoiceSegment {
    pub fn pcm(&self) -> &Pcm16Mono {
        &self.pcm
    }
}

pub struct SystemAudioRing {
    segments: VecDeque<SystemAudioSegment>,
    total_samples: usize,
    sample_capacity: usize,
    segment_capacity: usize,
}

impl SystemAudioRing {
    pub fn new(config: AudioRuntimeConfig) -> Self {
        Self {
            segments: VecDeque::with_capacity(config.system_audio_segment_capacity),
            total_samples: 0,
            sample_capacity: config.system_audio_sample_limit(),
            segment_capacity: config.system_audio_segment_capacity,
        }
    }

    pub fn push(&mut self, segment: SystemAudioSegment) {
        self.total_samples += segment.pcm.samples.len();
        self.segments.push_back(segment);
        while self.segments.len() > self.segment_capacity
            || self.total_samples > self.sample_capacity
        {
            if let Some(removed) = self.segments.pop_front() {
                self.total_samples -= removed.pcm.samples.len();
            }
        }
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn total_samples(&self) -> usize {
        self.total_samples
    }

    pub fn recent(&self) -> Vec<&SystemAudioSegment> {
        self.segments.iter().collect()
    }

    pub fn clear(&mut self) {
        self.segments.clear();
        self.total_samples = 0;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingAppendOutcome {
    Accepted,
    LimitReached,
}

#[derive(Clone, Debug)]
pub struct MicrophoneTempStore {
    directory: PathBuf,
}

impl MicrophoneTempStore {
    pub fn prepare(cache_root: &Path) -> Result<Self, AppError> {
        let directory = cache_root.join("microphone");
        fs::create_dir_all(&directory).map_err(|_| microphone_cache_error())?;
        let store = Self { directory };
        store.cleanup_orphans()?;
        Ok(store)
    }

    pub fn cleanup_orphans(&self) -> Result<(), AppError> {
        let entries = fs::read_dir(&self.directory).map_err(|_| microphone_cache_error())?;
        for entry in entries {
            let entry = entry.map_err(|_| microphone_cache_error())?;
            let file_type = entry.file_type().map_err(|_| microphone_cache_error())?;
            if file_type.is_file() || file_type.is_symlink() {
                fs::remove_file(entry.path()).map_err(|_| microphone_cache_error())?;
            }
        }
        Ok(())
    }

    fn start(
        &self,
        request_id: u64,
        config: AudioRuntimeConfig,
    ) -> Result<MicrophoneRecording, AppError> {
        if !config.is_valid() {
            return Err(AppError::invalid_argument(
                "Microphone capture limits are invalid.",
            ));
        }
        for _ in 0..16 {
            let sequence = RECORDING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = self.directory.join(format!(
                "voice-{}-{request_id}-{sequence}.pcm",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(MicrophoneRecording {
                        request_id,
                        path,
                        file: Some(file),
                        samples_written: 0,
                        sample_limit: config.microphone_sample_limit(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(microphone_cache_error()),
            }
        }
        Err(microphone_cache_error())
    }
}

pub struct MicrophoneRecording {
    request_id: u64,
    path: PathBuf,
    file: Option<File>,
    samples_written: usize,
    sample_limit: usize,
}

impl MicrophoneRecording {
    pub fn append(&mut self, samples: &[i16]) -> Result<RecordingAppendOutcome, AppError> {
        let remaining = self.sample_limit.saturating_sub(self.samples_written);
        let accepted = remaining.min(samples.len());
        let file = self.file.as_mut().ok_or_else(microphone_cache_error)?;
        for chunk in samples[..accepted].chunks(4_096) {
            let mut bytes = Vec::with_capacity(chunk.len() * 2);
            for sample in chunk {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
            file.write_all(&bytes)
                .map_err(|_| microphone_cache_error())?;
        }
        self.samples_written += accepted;
        Ok(
            if accepted < samples.len() || self.samples_written == self.sample_limit {
                RecordingAppendOutcome::LimitReached
            } else {
                RecordingAppendOutcome::Accepted
            },
        )
    }

    pub fn finish(mut self) -> Result<UserVoiceSegment, AppError> {
        let mut file = self.file.take().ok_or_else(microphone_cache_error)?;
        file.flush().map_err(|_| microphone_cache_error())?;
        drop(file);
        let mut file = File::open(&self.path).map_err(|_| microphone_cache_error())?;
        let mut bytes = Vec::with_capacity(self.samples_written * 2);
        file.read_to_end(&mut bytes)
            .map_err(|_| microphone_cache_error())?;
        drop(file);
        if bytes.len() != self.samples_written * 2 {
            return Err(microphone_cache_error());
        }
        let samples = bytes
            .chunks_exact(2)
            .map(|sample| i16::from_le_bytes([sample[0], sample[1]]))
            .collect();
        fs::remove_file(&self.path).map_err(|_| microphone_cache_error())?;
        Ok(UserVoiceSegment {
            request_id: self.request_id,
            pcm: Pcm16Mono::new(samples)?,
        })
    }

    #[cfg(test)]
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for MicrophoneRecording {
    fn drop(&mut self) {
        self.file.take();
        let _ = fs::remove_file(&self.path);
    }
}

fn microphone_cache_error() -> AppError {
    AppError::new(
        ErrorCode::Internal,
        "The microphone cache operation failed.",
    )
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn config() -> AudioRuntimeConfig {
        AudioRuntimeConfig {
            sample_rate_hz: 16_000,
            channels: 1,
            microphone_max_seconds: 60,
            microphone_max_bytes: 2 * 1024 * 1024,
            system_audio_ring_seconds: 60,
            system_audio_segment_capacity: 3,
            user_voice_queue_capacity: 1,
            system_audio_queue_capacity: 2,
        }
    }

    #[test]
    fn rejects_audio_defaults_beyond_the_hard_contract() {
        let mut candidate = config();
        assert!(candidate.is_valid());
        candidate.sample_rate_hz = 48_000;
        assert!(!candidate.is_valid());
        candidate = config();
        candidate.channels = 2;
        assert!(!candidate.is_valid());
        candidate = config();
        candidate.microphone_max_seconds = 61;
        assert!(!candidate.is_valid());
        candidate = config();
        candidate.microphone_max_bytes = MICROPHONE_HARD_MAX_BYTES + 1;
        assert!(!candidate.is_valid());
        candidate = config();
        candidate.system_audio_ring_seconds = 61;
        assert!(!candidate.is_valid());
    }

    #[test]
    fn system_audio_ring_is_bounded_by_segments_and_duration() {
        let mut candidate = config();
        candidate.sample_rate_hz = 16_000;
        candidate.system_audio_ring_seconds = 1;
        let mut ring = SystemAudioRing::new(candidate);
        for sequence in 1..=10_000 {
            ring.push(
                SystemAudioSegment::new(
                    sequence,
                    sequence,
                    vec![sequence as i16; 8_000],
                    candidate,
                )
                .expect("segment"),
            );
            assert!(ring.len() <= 2);
            assert!(ring.total_samples() <= 16_000);
        }
        assert_eq!(
            ring.recent()
                .into_iter()
                .map(|segment| segment.sequence)
                .collect::<Vec<_>>(),
            [9_999, 10_000]
        );
    }

    #[test]
    fn microphone_recording_stops_at_duration_limit_and_deletes_on_success() {
        let directory = tempdir().expect("temporary cache");
        let mut candidate = config();
        candidate.microphone_max_seconds = 1;
        let store = MicrophoneTempStore::prepare(directory.path()).expect("store");
        let gate = UserVoiceGate::default();
        let reservation = gate.reserve().expect("reservation");
        let request_id = reservation.request_id();
        let mut recording = reservation
            .start_recording(&store, candidate)
            .expect("recording");
        let path = recording.path().to_owned();
        assert_eq!(
            recording.append(&vec![42; 20_000]).expect("append"),
            RecordingAppendOutcome::LimitReached
        );
        assert_eq!(fs::metadata(&path).expect("recording file").len(), 32_000);
        let segment = recording.finish().expect("segment");
        assert_eq!(segment.segment().request_id, request_id);
        assert_eq!(segment.segment().pcm().samples().len(), 16_000);
        assert!(!path.exists());
        assert!(gate.is_reserved());
        drop(segment);
        assert!(!gate.is_reserved());
    }

    #[test]
    fn cancellation_failure_and_startup_cleanup_leave_no_orphans() {
        let directory = tempdir().expect("temporary cache");
        let store = MicrophoneTempStore::prepare(directory.path()).expect("store");
        let gate = UserVoiceGate::default();
        let microphone = directory.path().join("microphone");

        let mut cancelled = gate
            .reserve()
            .expect("reservation")
            .start_recording(&store, config())
            .expect("cancelled recording");
        let cancelled_path = cancelled.path().to_owned();
        cancelled.append(&[1, 2, 3]).expect("append cancelled");
        drop(cancelled);
        assert!(!cancelled_path.exists());
        assert!(!gate.is_reserved());

        fs::write(microphone.join("orphan.pcm"), b"orphan").expect("orphan");
        fs::create_dir(microphone.join("preserved-directory")).expect("directory");
        MicrophoneTempStore::prepare(directory.path()).expect("cleanup");
        assert!(!microphone.join("orphan.pcm").exists());
        assert!(microphone.join("preserved-directory").is_dir());
    }

    #[test]
    fn empty_recording_is_rejected_and_still_removed() {
        let directory = tempdir().expect("temporary cache");
        let store = MicrophoneTempStore::prepare(directory.path()).expect("store");
        let gate = UserVoiceGate::default();
        let recording = gate
            .reserve()
            .expect("reservation")
            .start_recording(&store, config())
            .expect("recording");
        let path = recording.path().to_owned();
        assert!(recording.finish().is_err());
        assert!(!path.exists());
        assert!(!gate.is_reserved());
    }

    #[test]
    fn microphone_cannot_start_until_the_single_user_voice_lane_is_reserved() {
        let directory = tempdir().expect("temporary cache");
        let store = MicrophoneTempStore::prepare(directory.path()).expect("store");
        let gate = UserVoiceGate::default();
        let first = gate.reserve().expect("first reservation");
        let error = match gate.reserve() {
            Ok(_) => panic!("second reservation must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.code, ErrorCode::QueueFull);
        drop(first);

        let recording = gate
            .reserve()
            .expect("replacement reservation")
            .start_recording(&store, config())
            .expect("recording after reservation");
        assert!(gate.is_reserved());
        drop(recording);
        assert!(!gate.is_reserved());
    }
}
