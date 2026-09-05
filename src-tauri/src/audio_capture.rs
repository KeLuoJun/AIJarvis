use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
};

use crate::{
    asr::AsrEngine,
    audio::{AudioRuntimeConfig, SystemAudioRing, SystemAudioSegment, REQUIRED_SAMPLE_RATE_HZ},
    error::{AppError, ErrorCode},
};

const VAD_FRAME_SAMPLES: usize = 320;
const VAD_PREROLL_FRAMES: usize = 5;
const VAD_END_SILENCE_FRAMES: usize = 30;
const VAD_MIN_SPEECH_FRAMES: usize = 5;
const VAD_MAX_SEGMENT_FRAMES: usize = 750;

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

#[derive(Default)]
pub(crate) struct SystemAudioCaptureController {
    #[cfg(windows)]
    worker: Mutex<Option<CaptureWorker>>,
    ring: Arc<Mutex<Option<SystemAudioRing>>>,
}

impl SystemAudioCaptureController {
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
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let handle = match thread::Builder::new()
            .name("ai-jarvis-system-audio".into())
            .spawn(move || {
                let result =
                    run_system_audio_capture(thread_cancel, asr, ring, config, ready_sender);
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
fn run_system_audio_capture(
    cancel: Arc<std::sync::atomic::AtomicBool>,
    asr: Arc<AsrEngine>,
    ring: Arc<Mutex<Option<SystemAudioRing>>>,
    config: AudioRuntimeConfig,
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
                submit_system_segment(&asr, &ring, config, sequence, samples);
            }
        },
    )?;
    if let Some(samples) = segmenter.finish() {
        sequence = sequence.saturating_add(1);
        submit_system_segment(&asr, &ring, config, sequence, samples);
    }
    Ok(())
}

#[cfg(windows)]
fn submit_system_segment(
    asr: &Arc<AsrEngine>,
    ring: &Arc<Mutex<Option<SystemAudioRing>>>,
    config: AudioRuntimeConfig,
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
    let _ = asr.submit_system(segment);
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum CaptureSource {
    SystemAudio,
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
}
