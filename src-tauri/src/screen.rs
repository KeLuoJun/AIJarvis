use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

const FINGERPRINT_SIDE: usize = 8;
const FINGERPRINT_CELLS: usize = FINGERPRINT_SIDE * FINGERPRINT_SIDE;
const BLACK_LUMA_MAX: u8 = 10;
const SOLID_LUMA_SPREAD_MAX: u8 = 2;
const CELL_CHANGE_THRESHOLD: u8 = 16;
const SMALL_CHANGE_PER_MILLE: u16 = 40;
const SIGNIFICANT_CHANGE_PER_MILLE: u16 = 80;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScreenRuntimeConfig {
    pub capture_interval_seconds: u64,
    pub frame_ring_capacity: usize,
    pub request_images_min: usize,
    pub request_images_max: usize,
    pub observation_min_interval_seconds: u64,
    pub next_check_min_seconds: u64,
    pub next_check_max_seconds: u64,
    pub next_check_default_seconds: u64,
    pub game_interval_seconds: u64,
    pub media_interval_seconds: u64,
    pub default_interval_seconds: u64,
    pub pending_observation_capacity: usize,
}

impl ScreenRuntimeConfig {
    pub const fn is_valid(self) -> bool {
        self.capture_interval_seconds > 0
            && self.frame_ring_capacity > 0
            && self.frame_ring_capacity <= 8
            && self.request_images_min >= 5
            && self.request_images_min <= self.request_images_max
            && self.request_images_max <= self.frame_ring_capacity
            && self.observation_min_interval_seconds >= 5
            && self.next_check_min_seconds >= self.observation_min_interval_seconds
            && self.next_check_min_seconds <= self.next_check_default_seconds
            && self.next_check_default_seconds <= self.next_check_max_seconds
            && self.next_check_max_seconds <= 18
            && self.game_interval_seconds >= self.capture_interval_seconds
            && self.media_interval_seconds >= self.capture_interval_seconds
            && self.default_interval_seconds >= self.capture_interval_seconds
            && self.pending_observation_capacity == 1
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureSource {
    pub monitor_id: String,
    pub process_id: u32,
    pub application: String,
    pub window_title: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedFrame {
    pub sequence: u64,
    pub captured_at_ms: u64,
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
    pub encoded_image: Vec<u8>,
    pub source: CaptureSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScreenFrame {
    pub sequence: u64,
    pub captured_at_ms: u64,
    pub encoded_image: Vec<u8>,
    pub source: CaptureSource,
    fingerprint: FrameFingerprint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FrameFingerprint {
    average_hash: u64,
    cells: [u8; FINGERPRINT_CELLS],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturePolicy {
    pub target_monitor_id: String,
    pub own_process_id: u32,
    pub excluded_applications: Vec<String>,
    pub excluded_windows: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameRejection {
    WrongMonitor,
    OwnApplication,
    ExcludedApplication,
    ExcludedWindow,
    Damaged,
    Black,
    SolidColor,
    OutOfOrder,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeClassification {
    Stable,
    ContinuousMotion,
    Transition,
    NewStableState,
}

impl ChangeClassification {
    pub const fn is_significant(self) -> bool {
        matches!(self, Self::Transition | Self::NewStableState)
    }
}

pub struct FrameRing {
    capacity: usize,
    frames: VecDeque<ScreenFrame>,
}

impl FrameRing {
    pub fn new(capacity: usize) -> Option<Self> {
        (capacity > 0 && capacity <= 8).then(|| Self {
            capacity,
            frames: VecDeque::with_capacity(capacity),
        })
    }

    pub fn admit(
        &mut self,
        frame: CapturedFrame,
        policy: &CapturePolicy,
    ) -> Result<ChangeClassification, FrameRejection> {
        if self
            .frames
            .back()
            .is_some_and(|previous| frame.sequence <= previous.sequence)
        {
            return Err(FrameRejection::OutOfOrder);
        }
        let accepted = validate_and_fingerprint(frame, policy)?;
        if self.frames.len() == self.capacity {
            self.frames.pop_front();
        }
        self.frames.push_back(accepted);
        Ok(classify(&self.frames))
    }

    pub fn recent(&self, maximum: usize) -> Vec<&ScreenFrame> {
        let skip = self.frames.len().saturating_sub(maximum);
        self.frames.iter().skip(skip).collect()
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

fn validate_and_fingerprint(
    frame: CapturedFrame,
    policy: &CapturePolicy,
) -> Result<ScreenFrame, FrameRejection> {
    if !frame
        .source
        .monitor_id
        .eq_ignore_ascii_case(&policy.target_monitor_id)
    {
        return Err(FrameRejection::WrongMonitor);
    }
    if frame.source.process_id == policy.own_process_id {
        return Err(FrameRejection::OwnApplication);
    }
    if matches_any(
        &frame.source.application,
        &policy.excluded_applications,
        false,
    ) {
        return Err(FrameRejection::ExcludedApplication);
    }
    if matches_any(&frame.source.window_title, &policy.excluded_windows, true) {
        return Err(FrameRejection::ExcludedWindow);
    }

    let pixel_count = (frame.width as usize)
        .checked_mul(frame.height as usize)
        .ok_or(FrameRejection::Damaged)?;
    let expected_bytes = pixel_count.checked_mul(3).ok_or(FrameRejection::Damaged)?;
    if frame.width == 0
        || frame.height == 0
        || frame.rgb.len() != expected_bytes
        || frame.encoded_image.is_empty()
    {
        return Err(FrameRejection::Damaged);
    }
    let fingerprint = fingerprint(&frame.rgb, frame.width as usize, frame.height as usize);
    let (minimum, maximum) = fingerprint
        .cells
        .iter()
        .fold((u8::MAX, u8::MIN), |(minimum, maximum), value| {
            (minimum.min(*value), maximum.max(*value))
        });
    if maximum <= BLACK_LUMA_MAX {
        return Err(FrameRejection::Black);
    }
    if maximum.saturating_sub(minimum) <= SOLID_LUMA_SPREAD_MAX {
        return Err(FrameRejection::SolidColor);
    }

    Ok(ScreenFrame {
        sequence: frame.sequence,
        captured_at_ms: frame.captured_at_ms,
        encoded_image: frame.encoded_image,
        source: frame.source,
        fingerprint,
    })
}

fn matches_any(value: &str, patterns: &[String], substring: bool) -> bool {
    let value = value.trim().to_lowercase();
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim().to_lowercase();
        !pattern.is_empty()
            && if substring {
                value.contains(&pattern)
            } else {
                value == pattern
            }
    })
}

fn fingerprint(rgb: &[u8], width: usize, height: usize) -> FrameFingerprint {
    let mut cells = [0_u8; FINGERPRINT_CELLS];
    for cell_y in 0..FINGERPRINT_SIDE {
        for cell_x in 0..FINGERPRINT_SIDE {
            let x = ((cell_x * 2 + 1) * width / (FINGERPRINT_SIDE * 2)).min(width - 1);
            let y = ((cell_y * 2 + 1) * height / (FINGERPRINT_SIDE * 2)).min(height - 1);
            let offset = (y * width + x) * 3;
            let red = u32::from(rgb[offset]);
            let green = u32::from(rgb[offset + 1]);
            let blue = u32::from(rgb[offset + 2]);
            cells[cell_y * FINGERPRINT_SIDE + cell_x] =
                ((red * 77 + green * 150 + blue * 29) >> 8) as u8;
        }
    }
    let average =
        cells.iter().map(|value| u32::from(*value)).sum::<u32>() / FINGERPRINT_CELLS as u32;
    let average_hash = cells
        .iter()
        .enumerate()
        .fold(0_u64, |hash, (index, value)| {
            hash | (u64::from(u32::from(*value) >= average) << index)
        });
    FrameFingerprint {
        average_hash,
        cells,
    }
}

fn classify(frames: &VecDeque<ScreenFrame>) -> ChangeClassification {
    let Some(current) = frames.back() else {
        return ChangeClassification::Stable;
    };
    let Some(previous) = frames.iter().rev().nth(1) else {
        return ChangeClassification::Stable;
    };
    if current.source.application != previous.source.application
        || current.source.window_title != previous.source.window_title
    {
        return ChangeClassification::Transition;
    }

    let deltas = frames
        .iter()
        .zip(frames.iter().skip(1))
        .map(|(before, after)| change_per_mille(before.fingerprint, after.fingerprint))
        .rev()
        .take(3)
        .collect::<Vec<_>>();
    if deltas
        .first()
        .map_or(true, |delta| *delta <= SMALL_CHANGE_PER_MILLE)
    {
        if deltas.len() == 3
            && deltas[0] <= SMALL_CHANGE_PER_MILLE
            && deltas[1] <= SMALL_CHANGE_PER_MILLE
            && deltas[2] >= SIGNIFICANT_CHANGE_PER_MILLE
        {
            return ChangeClassification::NewStableState;
        }
        return ChangeClassification::Stable;
    }
    if deltas.len() == 3
        && deltas
            .iter()
            .all(|delta| *delta >= SIGNIFICANT_CHANGE_PER_MILLE)
    {
        ChangeClassification::ContinuousMotion
    } else {
        ChangeClassification::Transition
    }
}

fn change_per_mille(before: FrameFingerprint, after: FrameFingerprint) -> u16 {
    let changed_cells = before
        .cells
        .iter()
        .zip(after.cells)
        .filter(|(before, after)| before.abs_diff(*after) >= CELL_CHANGE_THRESHOLD)
        .count();
    let changed_area = changed_cells * 1_000 / FINGERPRINT_CELLS;
    let hash_distance = (before.average_hash ^ after.average_hash).count_ones() as usize;
    changed_area.max(hash_distance * 1_000 / FINGERPRINT_CELLS) as u16
}

#[cfg(windows)]
pub mod wgc {
    use std::{
        fmt,
        sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
        time::{Duration, Instant},
    };

    use windows_capture::{
        capture::{CaptureControl, GraphicsCaptureApiHandler},
        encoder::ImageEncoder,
        frame::{Frame, ImageFormat},
        graphics_capture_api::InternalCaptureControl,
        monitor::Monitor,
        settings::{ColorFormat, CursorCaptureSettings, DrawBorderSettings, Settings},
    };

    use super::{CaptureSource, CapturedFrame};

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub enum WgcCaptureError {
        MonitorUnavailable,
        StartFailed,
        FrameFailed,
        SessionClosed,
        StopFailed,
    }

    impl fmt::Display for WgcCaptureError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(match self {
                Self::MonitorUnavailable => "the target monitor is unavailable",
                Self::StartFailed => "Windows Graphics Capture could not start",
                Self::FrameFailed => "Windows Graphics Capture returned an invalid frame",
                Self::SessionClosed => "the Windows Graphics Capture session is closed",
                Self::StopFailed => "Windows Graphics Capture could not stop cleanly",
            })
        }
    }

    impl std::error::Error for WgcCaptureError {}

    struct NativeFrame {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        png: Vec<u8>,
    }

    struct CaptureFlags {
        sender: SyncSender<Result<NativeFrame, WgcCaptureError>>,
        minimum_interval: Duration,
    }

    struct WgcHandler {
        sender: SyncSender<Result<NativeFrame, WgcCaptureError>>,
        minimum_interval: Duration,
        last_sent: Option<Instant>,
    }

    impl GraphicsCaptureApiHandler for WgcHandler {
        type Flags = CaptureFlags;
        type Error = WgcCaptureError;

        fn new(flags: Self::Flags) -> Result<Self, Self::Error> {
            Ok(Self {
                sender: flags.sender,
                minimum_interval: flags.minimum_interval,
                last_sent: None,
            })
        }

        fn on_frame_arrived(
            &mut self,
            frame: &mut Frame,
            control: InternalCaptureControl,
        ) -> Result<(), Self::Error> {
            let now = Instant::now();
            if self
                .last_sent
                .is_some_and(|last| now.duration_since(last) < self.minimum_interval)
            {
                return Ok(());
            }
            let width = frame.width();
            let height = frame.height();
            let mut buffer = frame.buffer().map_err(|_| WgcCaptureError::FrameFailed)?;
            let rgba = buffer
                .as_raw_nopadding_buffer()
                .map_err(|_| WgcCaptureError::FrameFailed)?
                .to_vec();
            let png = ImageEncoder::new(ImageFormat::Png, ColorFormat::Rgba8)
                .encode(&rgba, width, height)
                .map_err(|_| WgcCaptureError::FrameFailed)?;
            match self.sender.try_send(Ok(NativeFrame {
                width,
                height,
                rgba,
                png,
            })) {
                Ok(()) => self.last_sent = Some(now),
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => control.stop(),
            }
            Ok(())
        }

        fn on_closed(&mut self) -> Result<(), Self::Error> {
            let _ = self.sender.try_send(Err(WgcCaptureError::SessionClosed));
            Ok(())
        }
    }

    pub struct WgcSession {
        monitor_id: String,
        receiver: Receiver<Result<NativeFrame, WgcCaptureError>>,
        control: Option<CaptureControl<WgcHandler, WgcCaptureError>>,
    }

    impl WgcSession {
        pub fn start_primary(capture_interval: Duration) -> Result<Self, WgcCaptureError> {
            let monitor = Monitor::primary().map_err(|_| WgcCaptureError::MonitorUnavailable)?;
            Self::start(monitor, capture_interval)
        }

        pub fn start_index(
            monitor_index: usize,
            capture_interval: Duration,
        ) -> Result<Self, WgcCaptureError> {
            let monitor = Monitor::from_index(monitor_index)
                .map_err(|_| WgcCaptureError::MonitorUnavailable)?;
            Self::start(monitor, capture_interval)
        }

        fn start(monitor: Monitor, capture_interval: Duration) -> Result<Self, WgcCaptureError> {
            if capture_interval.is_zero() {
                return Err(WgcCaptureError::StartFailed);
            }
            let monitor_id = monitor
                .device_name()
                .map_err(|_| WgcCaptureError::MonitorUnavailable)?;
            let (sender, receiver) = mpsc::sync_channel(1);
            let settings = Settings::new(
                monitor,
                CursorCaptureSettings::WithoutCursor,
                DrawBorderSettings::Default,
                ColorFormat::Rgba8,
                CaptureFlags {
                    sender,
                    minimum_interval: capture_interval,
                },
            );
            let control = WgcHandler::start_free_threaded(settings)
                .map_err(|_| WgcCaptureError::StartFailed)?;
            Ok(Self {
                monitor_id,
                receiver,
                control: Some(control),
            })
        }

        pub fn monitor_id(&self) -> &str {
            &self.monitor_id
        }

        pub fn try_latest(
            &self,
            sequence: u64,
            captured_at_ms: u64,
            process_id: u32,
            application: String,
            window_title: String,
        ) -> Result<Option<CapturedFrame>, WgcCaptureError> {
            let mut latest = None;
            loop {
                match self.receiver.try_recv() {
                    Ok(Ok(frame)) => latest = Some(frame),
                    Ok(Err(error)) => return Err(error),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Err(WgcCaptureError::SessionClosed),
                }
            }
            Ok(latest.map(|frame| CapturedFrame {
                sequence,
                captured_at_ms,
                width: frame.width,
                height: frame.height,
                rgb: rgba_to_rgb(&frame.rgba),
                encoded_image: frame.png,
                source: CaptureSource {
                    monitor_id: self.monitor_id.clone(),
                    process_id,
                    application,
                    window_title,
                },
            }))
        }

        pub fn stop(mut self) -> Result<(), WgcCaptureError> {
            let Some(control) = self.control.take() else {
                return Ok(());
            };
            control.stop().map_err(|_| WgcCaptureError::StopFailed)
        }
    }

    impl Drop for WgcSession {
        fn drop(&mut self) {
            if let Some(control) = self.control.take() {
                let _ = control.stop();
            }
        }
    }

    fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
        rgba.chunks_exact(4)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rgba_conversion_drops_only_alpha_and_preserves_order() {
            assert_eq!(rgba_to_rgb(&[1, 2, 3, 4, 5, 6, 7, 8]), [1, 2, 3, 5, 6, 7]);
        }

        #[test]
        #[ignore = "requires an interactive Windows desktop"]
        fn primary_monitor_capture_returns_an_in_memory_png() {
            let session =
                WgcSession::start_primary(Duration::from_millis(250)).expect("primary WGC session");
            let deadline = Instant::now() + Duration::from_secs(10);
            let captured = loop {
                if let Some(frame) = session
                    .try_latest(1, 0, 0, String::new(), String::new())
                    .expect("latest frame")
                {
                    break frame;
                }
                assert!(Instant::now() < deadline, "WGC frame timeout");
                std::thread::sleep(Duration::from_millis(25));
            };
            assert!(captured.width > 0 && captured.height > 0);
            assert_eq!(&captured.encoded_image[..8], b"\x89PNG\r\n\x1a\n");
            assert_eq!(
                captured.rgb.len(),
                captured.width as usize * captured.height as usize * 3
            );
            session.stop().expect("stop WGC session");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> CapturePolicy {
        CapturePolicy {
            target_monitor_id: "display-1".into(),
            own_process_id: 42,
            excluded_applications: vec!["keepass.exe".into()],
            excluded_windows: vec!["payroll".into()],
        }
    }

    fn frame(sequence: u64, cells: impl Fn(usize, usize) -> u8) -> CapturedFrame {
        let width = 16;
        let height = 16;
        let mut rgb = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            for x in 0..width {
                let value = cells(x, y);
                rgb.extend_from_slice(&[value, value, value]);
            }
        }
        CapturedFrame {
            sequence,
            captured_at_ms: sequence * 1_000,
            width: width as u32,
            height: height as u32,
            rgb,
            encoded_image: vec![sequence as u8, 1],
            source: CaptureSource {
                monitor_id: "display-1".into(),
                process_id: 7,
                application: "editor.exe".into(),
                window_title: "Code".into(),
            },
        }
    }

    fn patterned(sequence: u64, offset: usize) -> CapturedFrame {
        frame(
            sequence,
            |x, y| {
                if (x + y + offset) % 3 == 0 {
                    220
                } else {
                    30
                }
            },
        )
    }

    #[test]
    fn rejects_wrong_monitor_self_exclusions_and_invalid_pixels() {
        let mut ring = FrameRing::new(8).expect("ring");
        let mut candidate = patterned(1, 0);
        candidate.source.monitor_id = "display-2".into();
        assert_eq!(
            ring.admit(candidate, &policy()),
            Err(FrameRejection::WrongMonitor)
        );

        let mut candidate = patterned(2, 0);
        candidate.source.process_id = 42;
        assert_eq!(
            ring.admit(candidate, &policy()),
            Err(FrameRejection::OwnApplication)
        );

        let mut candidate = patterned(3, 0);
        candidate.source.application = "KeePass.exe".into();
        assert_eq!(
            ring.admit(candidate, &policy()),
            Err(FrameRejection::ExcludedApplication)
        );

        let mut candidate = patterned(4, 0);
        candidate.source.window_title = "Quarterly Payroll - Sheet".into();
        assert_eq!(
            ring.admit(candidate, &policy()),
            Err(FrameRejection::ExcludedWindow)
        );

        let mut candidate = patterned(5, 0);
        candidate.rgb.pop();
        assert_eq!(
            ring.admit(candidate, &policy()),
            Err(FrameRejection::Damaged)
        );
        assert!(ring.is_empty());
    }

    #[test]
    fn rejects_black_and_solid_frames_without_polluting_the_ring() {
        let mut ring = FrameRing::new(8).expect("ring");
        assert_eq!(
            ring.admit(frame(1, |_, _| 0), &policy()),
            Err(FrameRejection::Black)
        );
        assert_eq!(
            ring.admit(frame(2, |_, _| 180), &policy()),
            Err(FrameRejection::SolidColor)
        );
        assert!(ring.is_empty());
    }

    #[test]
    fn classifies_stable_motion_transition_and_new_stable_state() {
        let mut stable = FrameRing::new(8).expect("ring");
        for sequence in 1..=4 {
            assert_eq!(
                stable.admit(patterned(sequence, 0), &policy()),
                Ok(ChangeClassification::Stable)
            );
        }

        let mut motion = FrameRing::new(8).expect("ring");
        let mut outcome = ChangeClassification::Stable;
        for sequence in 1..=4 {
            outcome = motion
                .admit(patterned(sequence, sequence as usize), &policy())
                .expect("frame");
        }
        assert_eq!(outcome, ChangeClassification::ContinuousMotion);

        let mut transition = FrameRing::new(8).expect("ring");
        transition.admit(patterned(1, 0), &policy()).expect("first");
        assert_eq!(
            transition.admit(patterned(2, 1), &policy()),
            Ok(ChangeClassification::Transition)
        );

        let mut settled = FrameRing::new(8).expect("ring");
        settled.admit(patterned(1, 0), &policy()).expect("old");
        settled.admit(patterned(2, 1), &policy()).expect("change");
        settled.admit(patterned(3, 1), &policy()).expect("settling");
        assert_eq!(
            settled.admit(patterned(4, 1), &policy()),
            Ok(ChangeClassification::NewStableState)
        );
    }

    #[test]
    fn ring_stays_bounded_and_chronological_after_many_ticks() {
        let mut ring = FrameRing::new(8).expect("ring");
        for sequence in 1..=10_000 {
            ring.admit(patterned(sequence, sequence as usize % 2), &policy())
                .expect("valid frame");
            assert!(ring.len() <= 8);
        }
        let sequences = ring
            .recent(8)
            .iter()
            .map(|frame| frame.sequence)
            .collect::<Vec<_>>();
        assert_eq!(sequences, (9_993..=10_000).collect::<Vec<_>>());
        assert_eq!(
            ring.admit(patterned(10_000, 0), &policy()),
            Err(FrameRejection::OutOfOrder)
        );
    }
}
