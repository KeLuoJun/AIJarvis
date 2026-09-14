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
            && self.request_images_min == 1
            && self.request_images_min <= self.request_images_max
            && self.request_images_max <= self.frame_ring_capacity
            && self.request_images_max <= 3
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

pub fn application_key(application: &str, process_id: u32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut identity = std::collections::hash_map::DefaultHasher::new();
    application.hash(&mut identity);
    process_id.hash(&mut identity);
    identity.finish()
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

    /// Retain chronology, omit redundant frames, always include the current screen.
    pub fn observation_frames(
        &self,
        maximum: usize,
        change: ChangeClassification,
    ) -> Vec<&ScreenFrame> {
        let Some(latest) = self.frames.back() else {
            return Vec::new();
        };
        if maximum == 0 {
            return Vec::new();
        }
        if change == ChangeClassification::Stable {
            return vec![latest];
        }
        let mut selected = vec![latest];
        for frame in self.frames.iter().rev().skip(1) {
            if selected.len() >= maximum.min(3) {
                break;
            }
            if latest.captured_at_ms.saturating_sub(frame.captured_at_ms) > 30_000 {
                break;
            }
            if frame.source.process_id != latest.source.process_id
                || frame.source.application != latest.source.application
            {
                break;
            }
            if change_per_mille(
                frame.fingerprint,
                selected.last().expect("latest frame").fingerprint,
            ) > SMALL_CHANGE_PER_MILLE
            {
                selected.push(frame);
            }
        }
        selected.reverse();
        selected
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn clear(&mut self) {
        self.frames.clear();
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
        || current.source.process_id != previous.source.process_id
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
        sync::{
            atomic::{AtomicU64, Ordering},
            mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
            Arc,
        },
        time::{Duration, Instant},
    };

    use windows::{
        core::PWSTR,
        Win32::{
            Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT},
            Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO},
            System::Threading::{
                OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION,
            },
            UI::WindowsAndMessaging::{
                EnumWindows, GetForegroundWindow, GetWindowRect, GetWindowTextLengthW,
                GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
            },
        },
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

    const MODEL_IMAGE_WIDTH: u32 = 1920;
    const MODEL_IMAGE_HEIGHT: u32 = 1080;

    pub fn environment_signal(privacy: &WgcPrivacyRules) -> crate::activity::EnvironmentSignal {
        use windows::Win32::{
            Foundation::HANDLE,
            System::{
                StationsAndDesktops::{
                    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop,
                    DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, UOI_NAME,
                },
                SystemInformation::GetTickCount,
            },
            UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
        };
        // Query the input desktop's name; never activate or switch desktops.
        let desktop_available = unsafe {
            OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS).is_ok_and(
                |desktop| {
                    let mut name = [0_u16; 64];
                    let result = GetUserObjectInformationW(
                        HANDLE(desktop.0),
                        UOI_NAME,
                        Some(name.as_mut_ptr().cast()),
                        std::mem::size_of_val(&name) as u32,
                        None,
                    );
                    let _ = CloseDesktop(desktop);
                    result.is_ok()
                        && String::from_utf16_lossy(
                            &name[..name.iter().position(|c| *c == 0).unwrap_or(name.len())],
                        )
                        .eq_ignore_ascii_case("default")
                },
            )
        };
        if !desktop_available {
            return crate::activity::EnvironmentSignal {
                protected: true,
                ..Default::default()
            };
        }
        let window = describe_window(unsafe { GetForegroundWindow() });
        let own_application = window
            .as_ref()
            .is_some_and(|w| w.process_id == privacy.own_process_id);
        let protected = window
            .as_ref()
            .is_some_and(|w| privacy.excludes(w) && !own_application);
        let application_key = window.filter(|_| !protected).map_or(0, |window| {
            super::application_key(&window.application, window.process_id)
        });
        let mut input = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            ..Default::default()
        };
        let input_idle_seconds = unsafe { GetLastInputInfo(&mut input) }
            .as_bool()
            .then(|| u64::from(unsafe { GetTickCount() }.wrapping_sub(input.dwTime)) / 1000);
        crate::activity::EnvironmentSignal {
            application_key,
            input_idle_seconds,
            protected,
            own_application,
        }
    }

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
        captured_at: Instant,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        png: Vec<u8>,
        process_id: u32,
        application: String,
        window_title: String,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct WgcPrivacyRules {
        pub own_process_id: u32,
        pub excluded_applications: Vec<String>,
        pub excluded_windows: Vec<String>,
        include_own_application: bool,
    }

    impl WgcPrivacyRules {
        pub fn for_current_process(
            excluded_applications: Vec<String>,
            excluded_windows: Vec<String>,
        ) -> Self {
            Self {
                own_process_id: std::process::id(),
                excluded_applications,
                excluded_windows,
                include_own_application: false,
            }
        }

        pub fn for_direct_conversation(
            excluded_applications: Vec<String>,
            excluded_windows: Vec<String>,
        ) -> Self {
            Self {
                own_process_id: std::process::id(),
                excluded_applications,
                excluded_windows,
                include_own_application: true,
            }
        }
    }

    struct CaptureFlags {
        sender: SyncSender<Result<NativeFrame, WgcCaptureError>>,
        interval_ms: Arc<AtomicU64>,
        privacy: WgcPrivacyRules,
        monitor_rect: RECT,
    }

    struct WgcHandler {
        sender: SyncSender<Result<NativeFrame, WgcCaptureError>>,
        interval_ms: Arc<AtomicU64>,
        last_sent: Option<Instant>,
        privacy: WgcPrivacyRules,
        monitor_rect: RECT,
    }

    impl GraphicsCaptureApiHandler for WgcHandler {
        type Flags = CaptureFlags;
        type Error = WgcCaptureError;

        fn new(flags: Self::Flags) -> Result<Self, Self::Error> {
            Ok(Self {
                sender: flags.sender,
                interval_ms: flags.interval_ms,
                last_sent: None,
                privacy: flags.privacy,
                monitor_rect: flags.monitor_rect,
            })
        }

        fn on_frame_arrived(
            &mut self,
            frame: &mut Frame,
            control: InternalCaptureControl,
        ) -> Result<(), Self::Error> {
            let now = Instant::now();
            let interval_ms = self.interval_ms.load(Ordering::Acquire);
            if interval_ms == 0 {
                return Ok(());
            }
            if self
                .last_sent
                .is_some_and(|last| now.duration_since(last) < Duration::from_millis(interval_ms))
            {
                return Ok(());
            }
            self.last_sent = Some(now);
            let signal = environment_signal(&self.privacy);
            if signal.protected || (signal.own_application && !self.privacy.include_own_application)
            {
                return Ok(());
            }
            let width = frame.width();
            let height = frame.height();
            let mut buffer = frame.buffer().map_err(|_| WgcCaptureError::FrameFailed)?;
            let mut rgba = buffer
                .as_raw_nopadding_buffer()
                .map_err(|_| WgcCaptureError::FrameFailed)?
                .to_vec();
            let windows = visible_windows();
            let foreground = foreground_window(&windows);
            if foreground
                .as_ref()
                .is_some_and(|window| self.privacy.excludes(window))
            {
                return Ok(());
            }
            for window in windows
                .iter()
                .filter(|window| self.privacy.excludes(window))
            {
                mask_window(&mut rgba, width, height, self.monitor_rect, window.rect);
            }
            let model_rgba = resize_rgba_letterboxed(
                &rgba,
                width,
                height,
                MODEL_IMAGE_WIDTH,
                MODEL_IMAGE_HEIGHT,
            )
            .ok_or(WgcCaptureError::FrameFailed)?;
            let png = ImageEncoder::new(ImageFormat::Png, ColorFormat::Rgba8)
                .encode(&model_rgba, MODEL_IMAGE_WIDTH, MODEL_IMAGE_HEIGHT)
                .map_err(|_| WgcCaptureError::FrameFailed)?;
            match self.sender.try_send(Ok(NativeFrame {
                captured_at: now,
                width,
                height,
                rgba,
                png,
                process_id: foreground.as_ref().map_or(0, |window| window.process_id),
                application: foreground
                    .as_ref()
                    .map_or_else(String::new, |window| window.application.clone()),
                window_title: foreground.map_or_else(String::new, |window| window.title),
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

    fn resize_rgba_letterboxed(
        source: &[u8],
        source_width: u32,
        source_height: u32,
        target_width: u32,
        target_height: u32,
    ) -> Option<Vec<u8>> {
        if source_width == 0 || source_height == 0 || target_width == 0 || target_height == 0 {
            return None;
        }
        let source_len = usize::try_from(source_width)
            .ok()?
            .checked_mul(usize::try_from(source_height).ok()?)?
            .checked_mul(4)?;
        if source.len() != source_len {
            return None;
        }
        let target_len = usize::try_from(target_width)
            .ok()?
            .checked_mul(usize::try_from(target_height).ok()?)?
            .checked_mul(4)?;
        let mut output = vec![0_u8; target_len];
        for alpha in output.iter_mut().skip(3).step_by(4) {
            *alpha = u8::MAX;
        }

        let width_limited = u64::from(target_width) * u64::from(source_height)
            <= u64::from(target_height) * u64::from(source_width);
        let (scaled_width, scaled_height) = if width_limited {
            (
                target_width,
                ((u64::from(source_height) * u64::from(target_width) / u64::from(source_width))
                    as u32)
                    .max(1),
            )
        } else {
            (
                ((u64::from(source_width) * u64::from(target_height) / u64::from(source_height))
                    as u32)
                    .max(1),
                target_height,
            )
        };
        let offset_x = (target_width - scaled_width) / 2;
        let offset_y = (target_height - scaled_height) / 2;
        for y in 0..scaled_height {
            let source_y =
                (u64::from(y) * u64::from(source_height) / u64::from(scaled_height)) as u32;
            for x in 0..scaled_width {
                let source_x =
                    (u64::from(x) * u64::from(source_width) / u64::from(scaled_width)) as u32;
                let source_offset = ((source_y * source_width + source_x) * 4) as usize;
                let target_offset = (((y + offset_y) * target_width + x + offset_x) * 4) as usize;
                output[target_offset..target_offset + 4]
                    .copy_from_slice(&source[source_offset..source_offset + 4]);
            }
        }
        Some(output)
    }

    pub struct WgcSession {
        monitor_id: String,
        receiver: Receiver<Result<NativeFrame, WgcCaptureError>>,
        control: Option<CaptureControl<WgcHandler, WgcCaptureError>>,
        interval_ms: Arc<AtomicU64>,
    }

    impl WgcSession {
        pub fn start_primary(capture_interval: Duration) -> Result<Self, WgcCaptureError> {
            Self::start_primary_with_privacy(
                capture_interval,
                WgcPrivacyRules::for_current_process(Vec::new(), Vec::new()),
            )
        }

        pub fn start_primary_with_privacy(
            capture_interval: Duration,
            privacy: WgcPrivacyRules,
        ) -> Result<Self, WgcCaptureError> {
            let monitor = Monitor::primary().map_err(|_| WgcCaptureError::MonitorUnavailable)?;
            Self::start(monitor, capture_interval, privacy)
        }

        pub fn start_index(
            monitor_index: usize,
            capture_interval: Duration,
            privacy: WgcPrivacyRules,
        ) -> Result<Self, WgcCaptureError> {
            let monitor = Monitor::from_index(monitor_index)
                .map_err(|_| WgcCaptureError::MonitorUnavailable)?;
            Self::start(monitor, capture_interval, privacy)
        }

        fn start(
            monitor: Monitor,
            capture_interval: Duration,
            privacy: WgcPrivacyRules,
        ) -> Result<Self, WgcCaptureError> {
            if capture_interval.is_zero() {
                return Err(WgcCaptureError::StartFailed);
            }
            let monitor_id = monitor
                .device_name()
                .map_err(|_| WgcCaptureError::MonitorUnavailable)?;
            let monitor_rect = monitor_rect(monitor)?;
            let (sender, receiver) = mpsc::sync_channel(1);
            let interval_ms = Arc::new(AtomicU64::new(
                capture_interval.as_millis().min(u128::from(u64::MAX)) as u64,
            ));
            let settings = Settings::new(
                monitor,
                CursorCaptureSettings::WithoutCursor,
                DrawBorderSettings::Default,
                ColorFormat::Rgba8,
                CaptureFlags {
                    sender,
                    interval_ms: Arc::clone(&interval_ms),
                    privacy,
                    monitor_rect,
                },
            );
            let control = WgcHandler::start_free_threaded(settings)
                .map_err(|_| WgcCaptureError::StartFailed)?;
            Ok(Self {
                monitor_id,
                receiver,
                control: Some(control),
                interval_ms,
            })
        }

        pub fn monitor_id(&self) -> &str {
            &self.monitor_id
        }

        pub fn set_interval(&self, interval: Option<Duration>) {
            self.interval_ms.store(
                interval.map_or(0, |value| {
                    value.as_millis().min(u128::from(u64::MAX)) as u64
                }),
                Ordering::Release,
            );
        }

        pub fn try_latest(
            &self,
            sequence: u64,
            captured_at_ms: u64,
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
                captured_at_ms: captured_at_ms.saturating_sub(
                    frame
                        .captured_at
                        .elapsed()
                        .as_millis()
                        .min(u128::from(u64::MAX)) as u64,
                ),
                width: frame.width,
                height: frame.height,
                rgb: rgba_to_rgb(&frame.rgba),
                encoded_image: frame.png,
                source: CaptureSource {
                    monitor_id: self.monitor_id.clone(),
                    process_id: frame.process_id,
                    application: frame.application,
                    window_title: frame.window_title,
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

    pub async fn capture_primary_for_direct_conversation(
        privacy: WgcPrivacyRules,
        timeout: Duration,
    ) -> Result<CapturedFrame, WgcCaptureError> {
        let session = WgcSession::start_primary_with_privacy(Duration::from_millis(50), privacy)?;
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(frame) = session.try_latest(1, 0)? {
                let _ = session.stop();
                return Ok(frame);
            }
            if Instant::now() >= deadline {
                let _ = session.stop();
                return Err(WgcCaptureError::FrameFailed);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
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

    #[derive(Clone, Debug)]
    struct VisibleWindow {
        handle: HWND,
        process_id: u32,
        application: String,
        title: String,
        rect: RECT,
    }

    impl WgcPrivacyRules {
        fn excludes(&self, window: &VisibleWindow) -> bool {
            if window.process_id == self.own_process_id && !self.include_own_application {
                let mut affinity = 0;
                if unsafe {
                    windows::Win32::UI::WindowsAndMessaging::GetWindowDisplayAffinity(
                        window.handle,
                        &mut affinity,
                    )
                }
                .is_ok()
                    && affinity == windows::Win32::UI::WindowsAndMessaging::WDA_EXCLUDEFROMCAPTURE.0
                {
                    // Windows already omits this window, including transparent overlays.
                    return false;
                }
            }
            (window.process_id == self.own_process_id && !self.include_own_application)
                || super::matches_any(&window.application, &self.excluded_applications, false)
                || super::matches_any(&window.title, &self.excluded_windows, true)
        }
    }

    fn monitor_rect(monitor: Monitor) -> Result<RECT, WgcCaptureError> {
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe {
            GetMonitorInfoW(
                HMONITOR(monitor.as_raw_hmonitor()),
                std::ptr::addr_of_mut!(info),
            )
        }
        .as_bool()
        {
            Ok(info.rcMonitor)
        } else {
            Err(WgcCaptureError::MonitorUnavailable)
        }
    }

    fn visible_windows() -> Vec<VisibleWindow> {
        unsafe extern "system" fn callback(handle: HWND, parameter: LPARAM) -> BOOL {
            if !unsafe { IsWindowVisible(handle) }.as_bool() {
                return BOOL(1);
            }
            let windows = unsafe { &mut *(parameter.0 as *mut Vec<VisibleWindow>) };
            if let Some(window) = describe_window(handle) {
                windows.push(window);
            }
            BOOL(1)
        }

        let mut windows = Vec::new();
        let parameter = LPARAM(std::ptr::addr_of_mut!(windows) as isize);
        let _ = unsafe { EnumWindows(Some(callback), parameter) };
        windows
    }

    fn describe_window(handle: HWND) -> Option<VisibleWindow> {
        let mut rect = RECT::default();
        if !unsafe { GetWindowRect(handle, &mut rect) }.is_ok()
            || rect.right <= rect.left
            || rect.bottom <= rect.top
        {
            return None;
        }
        let mut process_id = 0;
        unsafe { GetWindowThreadProcessId(handle, Some(&mut process_id)) };
        Some(VisibleWindow {
            handle,
            process_id,
            application: process_name(process_id),
            title: window_title(handle),
            rect,
        })
    }

    fn foreground_window(windows: &[VisibleWindow]) -> Option<VisibleWindow> {
        let foreground = unsafe { GetForegroundWindow() };
        windows
            .iter()
            .find(|window| window.handle == foreground)
            .cloned()
            .or_else(|| describe_window(foreground))
    }

    fn window_title(handle: HWND) -> String {
        let length = unsafe { GetWindowTextLengthW(handle) };
        if length <= 0 {
            return String::new();
        }
        let mut buffer = vec![0_u16; length as usize + 1];
        let copied = unsafe { GetWindowTextW(handle, &mut buffer) };
        String::from_utf16_lossy(&buffer[..copied.max(0) as usize])
    }

    fn process_name(process_id: u32) -> String {
        let Ok(process) =
            (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) })
        else {
            return String::new();
        };
        let mut buffer = vec![0_u16; 32_768];
        let mut length = buffer.len() as u32;
        let result = unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut length,
            )
        };
        let _ = unsafe { CloseHandle(process) };
        if result.is_err() {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buffer[..length as usize]);
        std::path::Path::new(&path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned()
    }

    fn mask_window(rgba: &mut [u8], width: u32, height: u32, monitor: RECT, window: RECT) {
        let left = (window.left - monitor.left).clamp(0, width as i32) as usize;
        let right = (window.right - monitor.left).clamp(0, width as i32) as usize;
        let top = (window.top - monitor.top).clamp(0, height as i32) as usize;
        let bottom = (window.bottom - monitor.top).clamp(0, height as i32) as usize;
        for y in top..bottom {
            for x in left..right {
                let offset = (y * width as usize + x) * 4;
                rgba[offset..offset + 4].copy_from_slice(&[0, 0, 0, 255]);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows::{
            core::w,
            Win32::{
                Foundation::{HINSTANCE, HWND},
                Graphics::Gdi::UpdateWindow,
                UI::WindowsAndMessaging::{
                    CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW,
                    TranslateMessage, HMENU, MSG, PM_REMOVE, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW,
                    WS_VISIBLE,
                },
            },
        };

        const PRIVACY_PROBE_TITLE: &str = "AI Jarvis WGC Privacy Probe";

        #[test]
        fn failed_capture_affinity_query_keeps_own_windows_masked() {
            let window = VisibleWindow {
                handle: HWND::default(),
                process_id: std::process::id(),
                application: String::new(),
                title: String::new(),
                rect: RECT {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1080,
                },
            };
            let privacy = WgcPrivacyRules::for_current_process(Vec::new(), Vec::new());
            assert!(privacy.excludes(&window));
        }

        #[test]
        fn direct_context_includes_own_window_but_keeps_explicit_exclusions() {
            let window = VisibleWindow {
                handle: HWND::default(),
                process_id: std::process::id(),
                application: "ai-jarvis.exe".into(),
                title: "AI Jarvis".into(),
                rect: RECT {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1080,
                },
            };
            let direct = WgcPrivacyRules::for_direct_conversation(Vec::new(), Vec::new());
            assert!(!direct.excludes(&window));

            let excluded =
                WgcPrivacyRules::for_direct_conversation(Vec::new(), vec!["AI Jarvis".into()]);
            assert!(excluded.excludes(&window));
        }

        #[test]
        #[ignore = "requires an interactive Windows desktop and briefly shows a probe window"]
        fn capture_excluded_own_window_does_not_black_out_its_rectangle() {
            use windows::Win32::UI::WindowsAndMessaging::{
                SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
            };
            let monitor = Monitor::primary().expect("primary monitor");
            let probe = create_privacy_probe(monitor_rect(monitor).unwrap());
            let window = describe_window(probe.handle).unwrap();
            let privacy = WgcPrivacyRules::for_current_process(Vec::new(), Vec::new());
            assert!(privacy.excludes(&window));
            unsafe { SetWindowDisplayAffinity(probe.handle, WDA_EXCLUDEFROMCAPTURE) }.unwrap();
            assert!(!privacy.excludes(&window));
        }

        struct TestWindow {
            handle: HWND,
            shutdown: mpsc::SyncSender<()>,
            thread: Option<std::thread::JoinHandle<()>>,
        }

        impl Drop for TestWindow {
            fn drop(&mut self) {
                let _ = self.shutdown.send(());
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
            }
        }

        fn create_privacy_probe(monitor: RECT) -> TestWindow {
            let (handle_sender, handle_receiver) = mpsc::sync_channel(1);
            let (shutdown, shutdown_receiver) = mpsc::sync_channel(1);
            let thread = std::thread::spawn(move || {
                let width = (monitor.right - monitor.left).clamp(160, 360);
                let height = (monitor.bottom - monitor.top).clamp(120, 240);
                let handle = unsafe {
                    CreateWindowExW(
                        WS_EX_TOPMOST,
                        w!("STATIC"),
                        w!("AI Jarvis WGC Privacy Probe"),
                        WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                        monitor.left + 32,
                        monitor.top + 32,
                        width,
                        height,
                        HWND::default(),
                        HMENU::default(),
                        HINSTANCE::default(),
                        None,
                    )
                };
                assert_ne!(handle.0, 0, "privacy probe window");
                let _ = unsafe { UpdateWindow(handle) };
                handle_sender.send(handle.0).expect("publish probe handle");
                loop {
                    let mut message = MSG::default();
                    while unsafe { PeekMessageW(&mut message, HWND::default(), 0, 0, PM_REMOVE) }
                        .as_bool()
                    {
                        let _ = unsafe { TranslateMessage(&message) };
                        unsafe { DispatchMessageW(&message) };
                    }
                    if shutdown_receiver.try_recv().is_ok() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                let _ = unsafe { DestroyWindow(handle) };
            });
            let handle = handle_receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("probe handle");
            TestWindow {
                handle: HWND(handle),
                shutdown,
                thread: Some(thread),
            }
        }

        fn capture_frame(privacy: WgcPrivacyRules) -> (CapturedFrame, RECT) {
            let monitor = Monitor::primary().expect("primary monitor");
            let bounds = monitor_rect(monitor).expect("primary monitor bounds");
            let session =
                WgcSession::start_primary_with_privacy(Duration::from_millis(100), privacy)
                    .expect("primary WGC session");
            let deadline = Instant::now() + Duration::from_secs(10);
            let frame = loop {
                if let Some(frame) = session.try_latest(1, 0).expect("latest frame") {
                    break frame;
                }
                assert!(Instant::now() < deadline, "WGC frame timeout");
                std::thread::sleep(Duration::from_millis(25));
            };
            session.stop().expect("stop WGC session");
            (frame, bounds)
        }

        fn region_pixels(
            frame: &CapturedFrame,
            monitor: RECT,
            window: RECT,
        ) -> impl Iterator<Item = &[u8]> {
            let left = (window.left - monitor.left + 8).clamp(0, frame.width as i32) as usize;
            let right = (window.right - monitor.left - 8).clamp(0, frame.width as i32) as usize;
            let top = (window.top - monitor.top + 8).clamp(0, frame.height as i32) as usize;
            let bottom = (window.bottom - monitor.top - 8).clamp(0, frame.height as i32) as usize;
            assert!(
                left < right && top < bottom,
                "probe window intersects capture"
            );
            (top..bottom).flat_map(move |y| {
                (left..right).map(move |x| {
                    let offset = (y * frame.width as usize + x) * 3;
                    &frame.rgb[offset..offset + 3]
                })
            })
        }

        #[test]
        fn rgba_conversion_drops_only_alpha_and_preserves_order() {
            assert_eq!(rgba_to_rgb(&[1, 2, 3, 4, 5, 6, 7, 8]), [1, 2, 3, 5, 6, 7]);
        }

        #[test]
        fn model_image_resize_has_fixed_dimensions_and_opaque_letterbox() {
            let source = [255, 0, 0, 255, 0, 255, 0, 128];
            let resized =
                resize_rgba_letterboxed(&source, 2, 1, MODEL_IMAGE_WIDTH, MODEL_IMAGE_HEIGHT)
                    .expect("resize");
            assert_eq!(
                resized.len(),
                MODEL_IMAGE_WIDTH as usize * MODEL_IMAGE_HEIGHT as usize * 4
            );

            let top_center = (MODEL_IMAGE_WIDTH as usize / 2) * 4;
            assert_eq!(&resized[top_center..top_center + 4], [0, 0, 0, 255]);
            let content_y = 60_usize;
            let left = (content_y * MODEL_IMAGE_WIDTH as usize) * 4;
            let right =
                (content_y * MODEL_IMAGE_WIDTH as usize + MODEL_IMAGE_WIDTH as usize - 1) * 4;
            assert_eq!(&resized[left..left + 4], [255, 0, 0, 255]);
            assert_eq!(&resized[right..right + 4], [0, 255, 0, 128]);
        }

        #[test]
        fn model_image_resize_rejects_invalid_dimensions_and_pixel_length() {
            assert!(resize_rgba_letterboxed(&[], 0, 1, 1280, 720).is_none());
            assert!(resize_rgba_letterboxed(&[0; 7], 2, 1, 1280, 720).is_none());
            assert!(resize_rgba_letterboxed(&[0; 8], 2, 1, 0, 720).is_none());
        }

        #[test]
        #[ignore = "requires an interactive Windows desktop"]
        fn primary_monitor_capture_returns_an_in_memory_png() {
            let session =
                WgcSession::start_primary(Duration::from_millis(250)).expect("primary WGC session");
            let deadline = Instant::now() + Duration::from_secs(10);
            let captured = loop {
                if let Some(frame) = session.try_latest(1, 0).expect("latest frame") {
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

        #[tokio::test]
        #[ignore = "requires an interactive Windows desktop"]
        async fn direct_conversation_capture_returns_an_in_memory_png() {
            let frame = capture_primary_for_direct_conversation(
                WgcPrivacyRules::for_direct_conversation(Vec::new(), Vec::new()),
                Duration::from_secs(2),
            )
            .await
            .expect("direct conversation capture");

            assert!(frame.width > 0 && frame.height > 0);
            assert_eq!(&frame.encoded_image[..8], b"\x89PNG\r\n\x1a\n");
        }

        #[test]
        #[ignore = "requires an interactive Windows desktop and briefly shows a probe window"]
        fn real_capture_masks_configured_and_own_process_windows() {
            let monitor = Monitor::primary().expect("primary monitor");
            let monitor_bounds = monitor_rect(monitor).expect("primary monitor bounds");
            let probe = create_privacy_probe(monitor_bounds);
            let probe_bounds = describe_window(probe.handle)
                .expect("probe description")
                .rect;

            let (unmasked, unmasked_monitor) = capture_frame(WgcPrivacyRules {
                own_process_id: u32::MAX,
                excluded_applications: Vec::new(),
                excluded_windows: Vec::new(),
                include_own_application: false,
            });
            assert!(
                region_pixels(&unmasked, unmasked_monitor, probe_bounds)
                    .any(|pixel| pixel != [0, 0, 0]),
                "unmasked probe must be visible before testing privacy rules"
            );

            let (title_masked, title_monitor) = capture_frame(WgcPrivacyRules {
                own_process_id: u32::MAX,
                excluded_applications: Vec::new(),
                excluded_windows: vec![PRIVACY_PROBE_TITLE.into()],
                include_own_application: false,
            });
            assert!(
                region_pixels(&title_masked, title_monitor, probe_bounds)
                    .all(|pixel| pixel == [0, 0, 0]),
                "configured excluded window must be blacked out"
            );

            let (process_masked, process_monitor) =
                capture_frame(WgcPrivacyRules::for_current_process(Vec::new(), Vec::new()));
            assert!(
                region_pixels(&process_masked, process_monitor, probe_bounds)
                    .all(|pixel| pixel == [0, 0, 0]),
                "own-process window must be blacked out"
            );
        }

        #[test]
        fn masking_clamps_window_rectangles_to_the_monitor() {
            let mut rgba = vec![255; 4 * 4 * 4];
            mask_window(
                &mut rgba,
                4,
                4,
                RECT {
                    left: 100,
                    top: 200,
                    right: 104,
                    bottom: 204,
                },
                RECT {
                    left: 98,
                    top: 201,
                    right: 102,
                    bottom: 203,
                },
            );
            for y in 0..4 {
                for x in 0..4 {
                    let pixel = &rgba[(y * 4 + x) * 4..(y * 4 + x + 1) * 4];
                    if (1..3).contains(&y) && x < 2 {
                        assert_eq!(pixel, [0, 0, 0, 255]);
                    } else {
                        assert_eq!(pixel, [255, 255, 255, 255]);
                    }
                }
            }
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

    #[test]
    fn title_churn_does_not_turn_identical_pixels_into_significant_events() {
        let mut ring = FrameRing::new(8).unwrap();
        for sequence in 1..=20 {
            let mut frame = patterned(sequence, 0);
            frame.source.window_title = format!("Editor - counter {sequence}");
            assert_eq!(
                ring.admit(frame, &policy()),
                Ok(ChangeClassification::Stable)
            );
        }
        ring.clear();
        assert!(ring.is_empty());
    }

    #[test]
    fn keyframes_are_sparse_fresh_ordered_and_never_padded() {
        let mut ring = FrameRing::new(8).unwrap();
        ring.admit(patterned(1, 0), &policy()).unwrap();
        assert_eq!(
            ring.observation_frames(3, ChangeClassification::Transition)
                .len(),
            1
        );
        for (seq, pattern) in [(2, 0), (3, 1), (4, 1), (5, 2), (6, 2)] {
            ring.admit(patterned(seq, pattern), &policy()).unwrap();
        }
        let selected = ring.observation_frames(3, ChangeClassification::ContinuousMotion);
        let ids: Vec<_> = selected.iter().map(|frame| frame.sequence).collect();
        assert_eq!(ids, vec![2, 4, 6]);
        assert_eq!(
            ring.observation_frames(3, ChangeClassification::Stable)[0].sequence,
            6
        );
        let mut fresh = patterned(7, 3);
        fresh.captured_at_ms = 100_000;
        ring.admit(fresh, &policy()).unwrap();
        assert_eq!(
            ring.observation_frames(3, ChangeClassification::Transition)
                .len(),
            1
        );
    }
}
