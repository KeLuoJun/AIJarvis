use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};

use serde::Serialize;

use crate::{
    asr::{AsrEngine, UserVoiceOutcome, UserVoiceOutcomeSink},
    audio::{
        AudioRuntimeConfig, MicrophoneTempStore, RecordingAppendOutcome,
        ReservedMicrophoneRecording, SystemAudioRing, SystemAudioSegment, UserVoiceGate,
        REQUIRED_SAMPLE_RATE_HZ,
    },
    error::{AppError, ErrorCode},
};

const VAD_FRAME_SAMPLES: usize = 320;
const VAD_PREROLL_FRAMES: usize = 5;
const VAD_END_SILENCE_FRAMES: usize = 30;
const VAD_MIN_SPEECH_FRAMES: usize = 5;
const VAD_MAX_SEGMENT_FRAMES: usize = 750;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum VoiceInputStatus {
    Idle,
    Recording,
    Processing,
    Submitted,
    NotHeard,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VoiceInputState {
    pub revision: u64,
    pub request_id: Option<u64>,
    pub status: VoiceInputStatus,
}

pub(crate) trait VoiceInputEventSink: Send + Sync {
    fn emit(&self, state: VoiceInputState);
}

pub(crate) struct VoiceInputStatusHub {
    state: Mutex<VoiceInputState>,
    sink: Arc<dyn VoiceInputEventSink>,
}

impl VoiceInputStatusHub {
    pub fn new(sink: Arc<dyn VoiceInputEventSink>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(VoiceInputState {
                revision: 0,
                request_id: None,
                status: VoiceInputStatus::Idle,
            }),
            sink,
        })
    }

    pub fn snapshot(&self) -> VoiceInputState {
        self.lock_state().clone()
    }

    fn publish(&self, request_id: u64, status: VoiceInputStatus) {
        let next = {
            let mut state = self.lock_state();
            state.revision = state.revision.saturating_add(1);
            state.request_id = Some(request_id);
            state.status = status;
            state.clone()
        };
        self.sink.emit(next);
    }

    fn publish_outcome(&self, request_id: u64, status: VoiceInputStatus) {
        let next = {
            let mut state = self.lock_state();
            if state.request_id != Some(request_id) || state.status != VoiceInputStatus::Processing
            {
                return;
            }
            state.revision = state.revision.saturating_add(1);
            state.status = status;
            state.clone()
        };
        self.sink.emit(next);
    }

    fn lock_state(&self) -> MutexGuard<'_, VoiceInputState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl UserVoiceOutcomeSink for VoiceInputStatusHub {
    fn emit(&self, request_id: u64, outcome: UserVoiceOutcome) {
        let status = match outcome {
            UserVoiceOutcome::Submitted => VoiceInputStatus::Submitted,
            UserVoiceOutcome::NotHeard => VoiceInputStatus::NotHeard,
            UserVoiceOutcome::Failed => VoiceInputStatus::Failed,
        };
        self.publish_outcome(request_id, status);
    }
}

trait VoiceClassifier {
    fn is_voice(&mut self, frame: &[i16]) -> bool;
}

struct VadSegmenter<V> {
    classifier: V,
    pending: Vec<i16>,
    preroll: VecDeque<Vec<i16>>,
    active: Vec<i16>,
    speech_frames: usize,
    trailing_silence_frames: usize,
}

impl<V: VoiceClassifier> VadSegmenter<V> {
    fn new(classifier: V) -> Self {
        Self {
            classifier,
            pending: Vec::with_capacity(VAD_FRAME_SAMPLES * 2),
            preroll: VecDeque::with_capacity(VAD_PREROLL_FRAMES),
            active: Vec::with_capacity(VAD_MAX_SEGMENT_FRAMES * VAD_FRAME_SAMPLES),
            speech_frames: 0,
            trailing_silence_frames: 0,
        }
    }

    fn push(&mut self, samples: &[i16]) -> Vec<Vec<i16>> {
        self.pending.extend_from_slice(samples);
        let complete_len = self.pending.len() / VAD_FRAME_SAMPLES * VAD_FRAME_SAMPLES;
        let complete = self.pending[..complete_len].to_vec();
        self.pending.drain(..complete_len);

        let mut segments = Vec::new();
        for frame in complete.chunks_exact(VAD_FRAME_SAMPLES) {
            if let Some(segment) = self.push_frame(frame) {
                segments.push(segment);
            }
        }
        segments
    }

    fn finish(&mut self) -> Option<Vec<i16>> {
        self.pending.clear();
        self.preroll.clear();
        self.take_active()
    }

    fn push_frame(&mut self, frame: &[i16]) -> Option<Vec<i16>> {
        let is_voice = self.classifier.is_voice(frame);
        if self.active.is_empty() {
            self.preroll.push_back(frame.to_vec());
            while self.preroll.len() > VAD_PREROLL_FRAMES {
                self.preroll.pop_front();
            }
            if !is_voice {
                return None;
            }
            for buffered in self.preroll.drain(..) {
                self.active.extend_from_slice(&buffered);
            }
            self.speech_frames = 1;
            return None;
        }

        self.active.extend_from_slice(frame);
        if is_voice {
            self.speech_frames += 1;
            self.trailing_silence_frames = 0;
        } else {
            self.trailing_silence_frames += 1;
        }

        if self.active.len() >= VAD_MAX_SEGMENT_FRAMES * VAD_FRAME_SAMPLES
            || self.trailing_silence_frames >= VAD_END_SILENCE_FRAMES
        {
            return self.take_active();
        }
        None
    }

    fn take_active(&mut self) -> Option<Vec<i16>> {
        let keep = self.speech_frames >= VAD_MIN_SPEECH_FRAMES;
        self.speech_frames = 0;
        self.trailing_silence_frames = 0;
        let active = std::mem::take(&mut self.active);
        keep.then_some(active)
    }
}

pub(crate) struct SystemAudioCaptureController {
    async_runtime: tokio::runtime::Handle,
    #[cfg(windows)]
    worker: Mutex<Option<CaptureWorker>>,
    ring: Arc<Mutex<Option<SystemAudioRing>>>,
}

impl SystemAudioCaptureController {
    pub fn new(async_runtime: tokio::runtime::Handle) -> Self {
        Self {
            async_runtime,
            #[cfg(windows)]
            worker: Mutex::new(None),
            ring: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(windows)]
    pub fn start(&self, asr: Arc<AsrEngine>, config: AudioRuntimeConfig) -> Result<(), AppError> {
        use std::{
            sync::{
                atomic::{AtomicBool, Ordering},
                mpsc,
            },
            thread,
            time::Duration,
        };

        let mut worker = self.lock_worker();
        if worker.is_some() {
            return Ok(());
        }
        *self.lock_ring() = Some(SystemAudioRing::new(config));
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = Arc::clone(&cancel);
        let ring = Arc::clone(&self.ring);
        let async_runtime = self.async_runtime.clone();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let handle = match thread::Builder::new()
            .name("ai-jarvis-system-audio".into())
            .spawn(move || {
                let result = run_system_audio_capture(
                    thread_cancel,
                    asr,
                    ring,
                    config,
                    async_runtime,
                    ready_sender,
                );
                if result.is_err() {
                    // Audio contents and transcripts are intentionally never logged.
                    eprintln!("system audio capture stopped unexpectedly");
                }
            }) {
            Ok(handle) => handle,
            Err(_) => {
                self.lock_ring().take();
                return Err(audio_capture_error(
                    "System audio capture could not be started.",
                ));
            }
        };
        let started = match ready_receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(started) => started,
            Err(_) => {
                cancel.store(true, Ordering::Release);
                let _ = handle.join();
                self.lock_ring().take();
                return Err(audio_capture_error(
                    "System audio capture did not initialize in time.",
                ));
            }
        };
        if let Err(error) = started {
            cancel.store(true, Ordering::Release);
            let _ = handle.join();
            self.lock_ring().take();
            return Err(error);
        }
        *worker = Some(CaptureWorker {
            cancel,
            handle: Some(handle),
        });
        Ok(())
    }

    #[cfg(not(windows))]
    pub fn start(&self, _asr: Arc<AsrEngine>, _config: AudioRuntimeConfig) -> Result<(), AppError> {
        Err(AppError::invalid_argument(
            "System audio capture requires Windows.",
        ))
    }

    pub fn stop(&self) {
        #[cfg(windows)]
        if let Some(mut worker) = self.lock_worker().take() {
            worker.stop();
        }
        self.lock_ring().take();
    }

    fn lock_ring(&self) -> MutexGuard<'_, Option<SystemAudioRing>> {
        self.ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(windows)]
    fn lock_worker(&self) -> MutexGuard<'_, Option<CaptureWorker>> {
        self.worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for SystemAudioCaptureController {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(crate) struct MicrophoneCaptureController {
    gate: UserVoiceGate,
    store: MicrophoneTempStore,
    config: AudioRuntimeConfig,
    status: Arc<VoiceInputStatusHub>,
    async_runtime: tokio::runtime::Handle,
    #[cfg(windows)]
    worker: Mutex<Option<MicrophoneWorker>>,
}

impl MicrophoneCaptureController {
    pub fn new(
        cache_root: &Path,
        config: AudioRuntimeConfig,
        status: Arc<VoiceInputStatusHub>,
        async_runtime: tokio::runtime::Handle,
    ) -> Result<Self, AppError> {
        Ok(Self {
            gate: UserVoiceGate::default(),
            store: MicrophoneTempStore::prepare(cache_root)?,
            config,
            status,
            async_runtime,
            #[cfg(windows)]
            worker: Mutex::new(None),
        })
    }

    #[cfg(windows)]
    pub fn start(&self, asr: Arc<AsrEngine>) -> Result<VoiceInputState, AppError> {
        use std::{
            sync::{
                atomic::{AtomicBool, Ordering},
                mpsc,
            },
            thread,
            time::Duration,
        };

        let mut worker = self.lock_worker();
        reap_microphone_worker(&mut worker);
        if worker.is_some() {
            return Err(AppError::new(
                ErrorCode::QueueFull,
                "A microphone recording is already active.",
            ));
        }
        let reservation = self.gate.reserve()?;
        let request_id = reservation.request_id();
        let recording = reservation.start_recording(&self.store, self.config)?;
        self.status.publish(request_id, VoiceInputStatus::Recording);

        let cancel = Arc::new(AtomicBool::new(false));
        let submit = Arc::new(AtomicBool::new(false));
        let thread_cancel = Arc::clone(&cancel);
        let thread_submit = Arc::clone(&submit);
        let status = Arc::clone(&self.status);
        let async_runtime = self.async_runtime.clone();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let handle = match thread::Builder::new()
            .name("ai-jarvis-microphone".into())
            .spawn(move || {
                run_microphone_capture(
                    recording,
                    thread_cancel,
                    thread_submit,
                    asr,
                    status,
                    async_runtime,
                    ready_sender,
                );
            }) {
            Ok(handle) => handle,
            Err(_) => {
                self.status.publish(request_id, VoiceInputStatus::Failed);
                return Err(audio_capture_error(
                    "Microphone capture could not be started.",
                ));
            }
        };

        match ready_receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {
                *worker = Some(MicrophoneWorker {
                    cancel,
                    submit,
                    handle: Some(handle),
                });
                Ok(self.status.snapshot())
            }
            Ok(Err(error)) => {
                cancel.store(true, Ordering::Release);
                let _ = handle.join();
                Err(error)
            }
            Err(_) => {
                cancel.store(true, Ordering::Release);
                let _ = handle.join();
                self.status.publish(request_id, VoiceInputStatus::Failed);
                Err(audio_capture_error(
                    "Microphone capture did not initialize in time.",
                ))
            }
        }
    }

    #[cfg(not(windows))]
    pub fn start(&self, _asr: Arc<AsrEngine>) -> Result<VoiceInputState, AppError> {
        Err(AppError::invalid_argument(
            "Microphone capture requires Windows.",
        ))
    }

    pub fn stop_and_submit(&self) -> VoiceInputState {
        #[cfg(windows)]
        {
            let worker = self.lock_worker().take();
            if let Some(mut worker) = worker {
                worker.stop(true);
            }
        }
        self.status.snapshot()
    }

    pub fn cancel(&self) -> VoiceInputState {
        #[cfg(windows)]
        {
            let worker = self.lock_worker().take();
            if let Some(mut worker) = worker {
                worker.stop(false);
            }
        }
        let current = self.status.snapshot();
        if matches!(
            current.status,
            VoiceInputStatus::Recording | VoiceInputStatus::Processing
        ) {
            if let Some(request_id) = current.request_id {
                self.status.publish(request_id, VoiceInputStatus::Cancelled);
            }
        }
        self.status.snapshot()
    }

    pub fn status(&self) -> VoiceInputState {
        #[cfg(windows)]
        {
            let mut worker = self.lock_worker();
            reap_microphone_worker(&mut worker);
        }
        self.status.snapshot()
    }

    #[cfg(windows)]
    fn lock_worker(&self) -> MutexGuard<'_, Option<MicrophoneWorker>> {
        self.worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for MicrophoneCaptureController {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(windows)]
struct CaptureWorker {
    cancel: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl CaptureWorker {
    fn stop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(windows)]
struct MicrophoneWorker {
    cancel: Arc<std::sync::atomic::AtomicBool>,
    submit: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl MicrophoneWorker {
    fn stop(&mut self, submit: bool) {
        self.submit
            .store(submit, std::sync::atomic::Ordering::Release);
        self.cancel
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(windows)]
fn reap_microphone_worker(worker: &mut Option<MicrophoneWorker>) {
    if worker
        .as_ref()
        .and_then(|worker| worker.handle.as_ref())
        .is_some_and(std::thread::JoinHandle::is_finished)
    {
        if let Some(mut finished) = worker.take() {
            if let Some(handle) = finished.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

#[cfg(windows)]
struct WebRtcVoiceClassifier(webrtc_vad::Vad);

#[cfg(windows)]
impl WebRtcVoiceClassifier {
    fn new() -> Self {
        Self(webrtc_vad::Vad::new_with_rate_and_mode(
            webrtc_vad::SampleRate::Rate16kHz,
            webrtc_vad::VadMode::Aggressive,
        ))
    }
}

#[cfg(windows)]
impl VoiceClassifier for WebRtcVoiceClassifier {
    fn is_voice(&mut self, frame: &[i16]) -> bool {
        self.0.is_voice_segment(frame).unwrap_or(false)
    }
}

#[cfg(windows)]
fn run_microphone_capture(
    mut recording: ReservedMicrophoneRecording,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    submit: Arc<std::sync::atomic::AtomicBool>,
    asr: Arc<AsrEngine>,
    status: Arc<VoiceInputStatusHub>,
    async_runtime: tokio::runtime::Handle,
    ready: std::sync::mpsc::SyncSender<Result<(), AppError>>,
) {
    use std::sync::atomic::Ordering;

    let request_id = recording.request_id();
    let write_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let callback_write_failed = Arc::clone(&write_failed);
    let capture_result = capture_pcm16_mono(
        CaptureSource::Microphone,
        Arc::clone(&cancel),
        ready,
        |samples| match recording.append(samples) {
            Ok(RecordingAppendOutcome::Accepted) => {}
            Ok(RecordingAppendOutcome::LimitReached) => {
                submit.store(true, Ordering::Release);
                cancel.store(true, Ordering::Release);
            }
            Err(_) => {
                callback_write_failed.store(true, Ordering::Release);
                cancel.store(true, Ordering::Release);
            }
        },
    );
    if capture_result.is_err() || write_failed.load(Ordering::Acquire) {
        status.publish(request_id, VoiceInputStatus::Failed);
        return;
    }
    if !submit.load(Ordering::Acquire) {
        status.publish(request_id, VoiceInputStatus::Cancelled);
        return;
    }

    status.publish(request_id, VoiceInputStatus::Processing);
    match recording.finish() {
        Ok(segment) => {
            let _runtime_guard = async_runtime.enter();
            if let Err(error) = asr.submit_user(segment) {
                eprintln!(
                    "microphone audio could not be scheduled for speech recognition: {:?}",
                    error.code
                );
                status.publish_outcome(request_id, VoiceInputStatus::Failed);
            }
        }
        Err(error) if error.code == ErrorCode::InvalidArgument => {
            status.publish_outcome(request_id, VoiceInputStatus::NotHeard);
        }
        Err(_) => {
            eprintln!("microphone audio could not be finalized");
            status.publish_outcome(request_id, VoiceInputStatus::Failed);
        }
    }
}

#[cfg(windows)]
fn run_system_audio_capture(
    cancel: Arc<std::sync::atomic::AtomicBool>,
    asr: Arc<AsrEngine>,
    ring: Arc<Mutex<Option<SystemAudioRing>>>,
    config: AudioRuntimeConfig,
    async_runtime: tokio::runtime::Handle,
    ready: std::sync::mpsc::SyncSender<Result<(), AppError>>,
) -> Result<(), AppError> {
    let mut segmenter = VadSegmenter::new(WebRtcVoiceClassifier::new());
    let mut sequence = 0_u64;
    capture_pcm16_mono(
        CaptureSource::SystemAudio,
        Arc::clone(&cancel),
        ready,
        |samples| {
            for samples in segmenter.push(samples) {
                sequence = sequence.saturating_add(1);
                submit_system_segment(&asr, &ring, config, &async_runtime, sequence, samples);
            }
        },
    )?;
    if let Some(samples) = segmenter.finish() {
        sequence = sequence.saturating_add(1);
        submit_system_segment(&asr, &ring, config, &async_runtime, sequence, samples);
    }
    Ok(())
}

#[cfg(windows)]
fn submit_system_segment(
    asr: &Arc<AsrEngine>,
    ring: &Arc<Mutex<Option<SystemAudioRing>>>,
    config: AudioRuntimeConfig,
    async_runtime: &tokio::runtime::Handle,
    sequence: u64,
    samples: Vec<i16>,
) {
    use std::time::{SystemTime, UNIX_EPOCH};

    let captured_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let Ok(segment) = SystemAudioSegment::new(sequence, captured_at_ms, samples, config) else {
        return;
    };
    if let Some(ring) = ring
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
    {
        ring.push(segment.clone());
    }
    let _runtime_guard = async_runtime.enter();
    let _ = asr.submit_system(segment);
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum CaptureSource {
    SystemAudio,
    Microphone,
}

#[cfg(windows)]
fn capture_pcm16_mono(
    source: CaptureSource,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    ready: std::sync::mpsc::SyncSender<Result<(), AppError>>,
    mut on_samples: impl FnMut(&[i16]),
) -> Result<(), AppError> {
    use std::{sync::atomic::Ordering, thread, time::Duration};
    use wasapi::{initialize_mta, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

    let initialized: Result<_, AppError> = (|| {
        initialize_mta()
            .ok()
            .map_err(|_| audio_capture_error("Windows audio COM initialization failed."))?;
        let enumerator = DeviceEnumerator::new()
            .map_err(|_| audio_capture_error("Windows audio device enumeration failed."))?;
        let device_direction = match source {
            CaptureSource::SystemAudio => Direction::Render,
            CaptureSource::Microphone => Direction::Capture,
        };
        let device = enumerator
            .get_default_device(&device_direction)
            .map_err(|_| audio_capture_error("The default audio endpoint is unavailable."))?;
        let mut client = device
            .get_iaudioclient()
            .map_err(|_| audio_capture_error("The default audio endpoint could not be opened."))?;
        let format = WaveFormat::new(
            16,
            16,
            &SampleType::Int,
            REQUIRED_SAMPLE_RATE_HZ as usize,
            1,
            None,
        );
        let (default_period, _) = client
            .get_device_period()
            .map_err(|_| audio_capture_error("The audio endpoint period is unavailable."))?;
        client
            .initialize_client(
                &format,
                &Direction::Capture,
                &StreamMode::PollingShared {
                    autoconvert: true,
                    buffer_duration_hns: default_period,
                },
            )
            .map_err(|_| audio_capture_error("The audio endpoint format is unsupported."))?;
        let capture = client
            .get_audiocaptureclient()
            .map_err(|_| audio_capture_error("The audio capture client is unavailable."))?;
        client
            .start_stream()
            .map_err(|_| audio_capture_error("The audio stream could not be started."))?;
        Ok((client, capture))
    })();

    let (client, capture) = match initialized {
        Ok(values) => {
            let _ = ready.send(Ok(()));
            values
        }
        Err(error) => {
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };

    while !cancel.load(Ordering::Acquire) {
        loop {
            let frames = capture
                .get_next_packet_size()
                .map_err(|_| audio_capture_error("The audio packet size could not be read."))?
                .unwrap_or(0);
            if frames == 0 {
                break;
            }
            let mut bytes = vec![0_u8; frames as usize * std::mem::size_of::<i16>()];
            let (read_frames, info) = capture
                .read_from_device(&mut bytes)
                .map_err(|_| audio_capture_error("The audio packet could not be read."))?;
            let samples = if info.flags.silent {
                vec![0_i16; read_frames as usize]
            } else {
                bytes[..read_frames as usize * 2]
                    .chunks_exact(2)
                    .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
                    .collect()
            };
            on_samples(&samples);
        }
        thread::sleep(Duration::from_millis(10));
    }
    client
        .stop_stream()
        .map_err(|_| audio_capture_error("The audio stream could not be stopped."))?;
    Ok(())
}

fn audio_capture_error(message: &'static str) -> AppError {
    AppError::new(ErrorCode::Internal, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeVoiceEventSink {
        values: Mutex<Vec<VoiceInputState>>,
    }

    impl VoiceInputEventSink for FakeVoiceEventSink {
        fn emit(&self, state: VoiceInputState) {
            self.values.lock().unwrap().push(state);
        }
    }

    struct SilentAsrModel;

    impl crate::asr::AsrModel for SilentAsrModel {
        fn transcribe<'a>(
            &'a self,
            _input: crate::asr::AsrModelInput,
        ) -> crate::asr::AsrModelFuture<'a> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    struct NoopObservationSink;

    impl crate::asr::ObservationTranscriptSink for NoopObservationSink {
        fn emit(&self, _transcript: crate::asr::ObservationTranscript) {}
    }

    struct NoopDirectSink;

    impl crate::asr::DirectUserMessageSink for NoopDirectSink {
        fn emit(&self, _message: crate::asr::DirectUserMessage) -> Result<(), AppError> {
            Ok(())
        }
    }

    struct FakeClassifier {
        values: VecDeque<bool>,
    }

    impl VoiceClassifier for FakeClassifier {
        fn is_voice(&mut self, _frame: &[i16]) -> bool {
            self.values.pop_front().unwrap_or(false)
        }
    }

    fn samples(frames: usize) -> Vec<i16> {
        vec![42; frames * VAD_FRAME_SAMPLES]
    }

    #[test]
    fn voice_status_is_bounded_and_ignores_stale_asr_outcomes() {
        let sink = Arc::new(FakeVoiceEventSink::default());
        let hub = VoiceInputStatusHub::new(sink.clone());
        hub.publish(4, VoiceInputStatus::Recording);
        UserVoiceOutcomeSink::emit(&*hub, 4, UserVoiceOutcome::Submitted);
        assert_eq!(hub.snapshot().status, VoiceInputStatus::Recording);

        hub.publish(4, VoiceInputStatus::Processing);
        UserVoiceOutcomeSink::emit(&*hub, 3, UserVoiceOutcome::Failed);
        UserVoiceOutcomeSink::emit(&*hub, 4, UserVoiceOutcome::NotHeard);
        assert_eq!(
            hub.snapshot(),
            VoiceInputState {
                revision: 3,
                request_id: Some(4),
                status: VoiceInputStatus::NotHeard,
            }
        );
        assert_eq!(sink.values.lock().unwrap().len(), 3);
    }

    #[test]
    fn voice_status_serialization_never_contains_transcript_text() {
        let state = VoiceInputState {
            revision: 7,
            request_id: Some(9),
            status: VoiceInputStatus::Processing,
        };
        assert_eq!(
            serde_json::to_value(state).expect("voice state"),
            serde_json::json!({
                "revision": 7,
                "requestId": 9,
                "status": "processing"
            })
        );
    }

    #[test]
    fn lifecycle_cancel_invalidates_processing_and_late_outcome() {
        let directory = tempfile::tempdir().expect("cache");
        let sink = Arc::new(FakeVoiceEventSink::default());
        let hub = VoiceInputStatusHub::new(sink);
        let config = crate::state::RuntimeConfig::parse(include_str!(
            "../../resources/config/defaults.toml"
        ))
        .expect("runtime config")
        .audio;
        let controller = MicrophoneCaptureController::new(
            directory.path(),
            config,
            Arc::clone(&hub),
            tauri::async_runtime::handle().inner().clone(),
        )
        .expect("microphone controller");
        hub.publish(8, VoiceInputStatus::Processing);

        assert_eq!(controller.cancel().status, VoiceInputStatus::Cancelled);
        UserVoiceOutcomeSink::emit(&*hub, 8, UserVoiceOutcome::Submitted);
        assert_eq!(hub.snapshot().status, VoiceInputStatus::Cancelled);
    }

    #[test]
    fn vad_discards_silence_and_short_noise() {
        let decisions = [
            vec![false; 10],
            vec![true; VAD_MIN_SPEECH_FRAMES - 1],
            vec![false; VAD_END_SILENCE_FRAMES],
        ]
        .concat();
        let count = decisions.len();
        let mut segmenter = VadSegmenter::new(FakeClassifier {
            values: decisions.into(),
        });
        assert!(segmenter.push(&samples(count)).is_empty());
        assert!(segmenter.finish().is_none());
    }

    #[test]
    fn vad_emits_bounded_segment_after_trailing_silence() {
        let decisions = [
            vec![false; VAD_PREROLL_FRAMES],
            vec![true; VAD_MIN_SPEECH_FRAMES],
            vec![false; VAD_END_SILENCE_FRAMES],
        ]
        .concat();
        let count = decisions.len();
        let mut segmenter = VadSegmenter::new(FakeClassifier {
            values: decisions.into(),
        });
        let segments = segmenter.push(&samples(count));
        assert_eq!(segments.len(), 1);
        assert!(segments[0].len() <= VAD_MAX_SEGMENT_FRAMES * VAD_FRAME_SAMPLES);
        assert_eq!(
            segments[0].len(),
            (VAD_PREROLL_FRAMES + VAD_MIN_SPEECH_FRAMES + VAD_END_SILENCE_FRAMES - 1)
                * VAD_FRAME_SAMPLES
        );
    }

    #[test]
    fn vad_buffers_partial_frames_and_caps_continuous_speech() {
        let mut segmenter = VadSegmenter::new(FakeClassifier {
            values: vec![true; VAD_MAX_SEGMENT_FRAMES + 1].into(),
        });
        assert!(segmenter.push(&samples(1)[..100]).is_empty());
        let mut remainder = samples(VAD_MAX_SEGMENT_FRAMES);
        remainder.drain(..100);
        let segments = segmenter.push(&remainder);
        assert_eq!(segments.len(), 1);
        assert_eq!(
            segments[0].len(),
            VAD_MAX_SEGMENT_FRAMES * VAD_FRAME_SAMPLES
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires a real default Windows render endpoint"]
    fn default_wasapi_loopback_opens_and_stops() {
        use std::{
            sync::{
                atomic::{AtomicBool, Ordering},
                mpsc,
            },
            thread,
            time::Duration,
        };

        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = Arc::clone(&cancel);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let handle = thread::spawn(move || {
            capture_pcm16_mono(
                CaptureSource::SystemAudio,
                thread_cancel,
                ready_sender,
                |_| {},
            )
        });
        ready_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("loopback initialization response")
            .expect("default loopback endpoint");
        cancel.store(true, Ordering::Release);
        handle.join().expect("capture thread").expect("clean stop");
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires a real default Windows microphone endpoint and desktop permission"]
    fn default_wasapi_microphone_opens_and_stops() {
        use std::{
            sync::{
                atomic::{AtomicBool, Ordering},
                mpsc,
            },
            thread,
            time::Duration,
        };

        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = Arc::clone(&cancel);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let handle = thread::spawn(move || {
            capture_pcm16_mono(
                CaptureSource::Microphone,
                thread_cancel,
                ready_sender,
                |_| {},
            )
        });
        ready_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("microphone initialization response")
            .expect("default microphone endpoint");
        cancel.store(true, Ordering::Release);
        handle.join().expect("capture thread").expect("clean stop");
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "requires a real default Windows microphone endpoint and desktop permission"]
    async fn microphone_controller_cancels_and_submits_without_orphan_files() {
        use crate::{
            settings::{MvpSettings, SettingsStore},
            state::{RuntimeConfig, RuntimeState},
            types::{FeatureSettings, PrivacyState},
        };
        use std::time::Duration;

        let directory = tempfile::tempdir().expect("runtime");
        let store = SettingsStore::new(directory.path());
        store
            .save_startup_state(PrivacyState::Active)
            .expect("active state");
        store
            .save_settings(MvpSettings {
                features: FeatureSettings {
                    screen_enabled: true,
                    system_audio_enabled: true,
                    proactive_response_enabled: true,
                },
                excluded_applications: Vec::new(),
                excluded_windows: Vec::new(),
            })
            .expect("settings");
        let config = RuntimeConfig::parse(include_str!("../../resources/config/defaults.toml"))
            .expect("runtime config");
        let runtime = Arc::new(RuntimeState::load(directory.path(), config).expect("runtime"));
        let voice_sink = Arc::new(FakeVoiceEventSink::default());
        let hub = VoiceInputStatusHub::new(voice_sink.clone());
        let outcome_sink: Arc<dyn UserVoiceOutcomeSink> = hub.clone();
        let asr = AsrEngine::new_with_user_voice_outcomes(
            runtime,
            Arc::new(SilentAsrModel),
            Arc::new(NoopObservationSink),
            Arc::new(NoopDirectSink),
            outcome_sink,
            config.audio.system_audio_queue_capacity,
        )
        .expect("ASR");
        let controller = MicrophoneCaptureController::new(
            directory.path(),
            config.audio,
            Arc::clone(&hub),
            tokio::runtime::Handle::current(),
        )
        .expect("microphone");
        let cache = directory.path().join("microphone");

        assert_eq!(
            controller.start(Arc::clone(&asr)).expect("start").status,
            VoiceInputStatus::Recording
        );
        std::thread::sleep(Duration::from_millis(75));
        assert_eq!(controller.cancel().status, VoiceInputStatus::Cancelled);
        assert_eq!(std::fs::read_dir(&cache).expect("cache").count(), 0);

        controller.start(asr).expect("restart");
        std::thread::sleep(Duration::from_millis(75));
        controller.stop_and_submit();
        tokio::time::timeout(Duration::from_secs(1), async {
            while hub.snapshot().status == VoiceInputStatus::Processing {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("ASR outcome");
        assert_eq!(
            hub.snapshot().status,
            VoiceInputStatus::NotHeard,
            "events: {:?}",
            voice_sink.values.lock().unwrap().as_slice()
        );
        assert_eq!(std::fs::read_dir(&cache).expect("cache").count(), 0);
    }
}
