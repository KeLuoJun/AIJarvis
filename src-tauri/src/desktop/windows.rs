use tauri::{
    AppHandle, Manager, PhysicalPosition, Runtime, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};

use crate::commands::AppWindowKind;

// Keep the transparent pet surface close to the rendered character. Chat and
// input controls are positioned inside this compact surface by the frontend.
const PET_WINDOW_WIDTH: u32 = 180;
const PET_WINDOW_HEIGHT: u32 = 320;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WindowSpec {
    pub kind: AppWindowKind,
    pub title: &'static str,
    pub width: u32,
    pub height: u32,
    pub transparent: bool,
    pub decorations: bool,
    pub always_on_top: bool,
    pub focusable: bool,
    pub resizable: bool,
    pub skip_taskbar: bool,
    pub click_through: bool,
    pub covers_monitor: bool,
}

impl WindowSpec {
    pub const fn capture_excluded(self) -> bool {
        // Only the fullscreen overlay needs OS exclusion to avoid masking the desktop.
        // Regular windows must retain taskbar previews and remote-desktop visibility.
        matches!(self.kind, AppWindowKind::Danmaku)
    }

    pub const fn for_kind(kind: AppWindowKind) -> Self {
        match kind {
            AppWindowKind::Launcher => Self {
                kind,
                title: "",
                width: 760,
                height: 560,
                transparent: false,
                decorations: true,
                always_on_top: false,
                focusable: true,
                resizable: false,
                skip_taskbar: false,
                click_through: false,
                covers_monitor: false,
            },
            AppWindowKind::Control => Self {
                kind,
                title: "AI Jarvis",
                width: 960,
                height: 720,
                transparent: false,
                decorations: true,
                always_on_top: false,
                focusable: true,
                resizable: true,
                skip_taskbar: false,
                click_through: false,
                covers_monitor: false,
            },
            AppWindowKind::Pet => Self {
                kind,
                title: "AI Jarvis Pet",
                width: PET_WINDOW_WIDTH,
                height: PET_WINDOW_HEIGHT,
                transparent: true,
                decorations: false,
                always_on_top: true,
                focusable: true,
                resizable: false,
                skip_taskbar: true,
                click_through: false,
                covers_monitor: false,
            },
            AppWindowKind::Danmaku => Self {
                kind,
                title: "AI Jarvis Danmaku",
                width: 1280,
                height: 720,
                transparent: true,
                decorations: false,
                always_on_top: true,
                focusable: false,
                resizable: false,
                skip_taskbar: true,
                click_through: true,
                covers_monitor: true,
            },
        }
    }
}

pub(crate) fn should_exit_after_window_destroyed<I, S>(
    destroyed_label: &str,
    remaining_labels: I,
) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    is_recovery_window(destroyed_label)
        && !remaining_labels
            .into_iter()
            .any(|label| is_recovery_window(label.as_ref()))
}

fn is_recovery_window(label: &str) -> bool {
    matches!(
        AppWindowKind::try_from(label),
        Ok(AppWindowKind::Launcher | AppWindowKind::Control | AppWindowKind::Pet)
    )
}

pub(crate) fn show_app_window<R: Runtime>(
    app: &AppHandle<R>,
    kind: AppWindowKind,
) -> tauri::Result<()> {
    let window = match app.get_webview_window(kind.as_str()) {
        Some(window) => window,
        None => create_window(app, kind)?,
    };

    if kind == AppWindowKind::Danmaku {
        configure_danmaku_overlay(app, &window)?;
    }
    window.show()?;
    if kind == AppWindowKind::Danmaku {
        raise_overlay_without_activation(&window)?;
    }
    if matches!(kind, AppWindowKind::Launcher | AppWindowKind::Control) {
        window.unminimize()?;
        window.set_focus()?;
    }
    crate::dev_event!("window.shown", "kind" => kind.as_str(),
        "visible" => window.is_visible().ok(), "minimized" => window.is_minimized().ok());
    Ok(())
}

pub(crate) fn hide_app_window<R: Runtime>(
    app: &AppHandle<R>,
    kind: AppWindowKind,
) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window(kind.as_str()) {
        window.hide()?;
    }
    Ok(())
}

fn create_window<R: Runtime>(
    app: &AppHandle<R>,
    kind: AppWindowKind,
) -> tauri::Result<WebviewWindow<R>> {
    let spec = WindowSpec::for_kind(kind);
    let window = WebviewWindowBuilder::new(
        app,
        spec.kind.as_str(),
        WebviewUrl::App("index.html".into()),
    )
    .title(spec.title)
    .inner_size(spec.width as f64, spec.height as f64)
    .visible(false)
    .focused(false)
    .focusable(spec.focusable)
    .transparent(spec.transparent)
    .decorations(spec.decorations)
    .always_on_top(spec.always_on_top)
    .resizable(spec.resizable)
    .skip_taskbar(spec.skip_taskbar)
    .shadow(!spec.transparent)
    .content_protected(spec.capture_excluded())
    .build()?;

    if !spec.covers_monitor {
        position_window(app, &window, spec)?;
    }

    Ok(window)
}

fn configure_danmaku_overlay<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
) -> tauri::Result<()> {
    let spec = WindowSpec::for_kind(AppWindowKind::Danmaku);

    // Like the legacy Electron overlay, use a monitor-sized ordinary window.
    // Fullscreen transitions can change z-order and disrupt game presentation.
    if window.is_fullscreen()? {
        window.set_fullscreen(false)?;
    }
    position_window(app, window, spec)?;
    window.set_decorations(false)?;
    window.set_resizable(false)?;
    window.set_skip_taskbar(true)?;
    window.set_ignore_cursor_events(true)?;
    window.set_always_on_top(true)?;
    crate::dev_event!(
        "window.overlay_configured",
        "fullscreen" => window.is_fullscreen().ok(),
        "always_on_top" => spec.always_on_top,
        "click_through" => spec.click_through,
        "skip_taskbar" => spec.skip_taskbar
    );
    Ok(())
}

fn raise_overlay_without_activation<R: Runtime>(window: &WebviewWindow<R>) -> tauri::Result<()> {
    #[cfg(windows)]
    {
        use windows::Win32::{
            Foundation::HWND,
            UI::WindowsAndMessaging::{
                SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
            },
        };
        // Reassert z-order after showing, including when already topmost.
        // Never activate the overlay or steal keyboard input from the game.
        unsafe {
            SetWindowPos(
                HWND(window.hwnd()?.0 as isize),
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            )
        }
        .map_err(|error| tauri::Error::Io(std::io::Error::other(error.to_string())))?;
    }
    Ok(())
}

fn foreground_monitor<R: Runtime>(app: &AppHandle<R>) -> Option<tauri::Monitor> {
    #[cfg(windows)]
    {
        use windows::Win32::{
            Foundation::RECT,
            UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect},
        };
        let mut rect = RECT::default();
        unsafe { GetWindowRect(GetForegroundWindow(), &mut rect) }.ok()?;
        app.monitor_from_point(
            (f64::from(rect.left) + f64::from(rect.right)) / 2.0,
            (f64::from(rect.top) + f64::from(rect.bottom)) / 2.0,
        )
        .ok()
        .flatten()
    }
    #[cfg(not(windows))]
    None
}

fn position_window<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    spec: WindowSpec,
) -> tauri::Result<()> {
    let monitor = (spec.kind == AppWindowKind::Danmaku)
        .then(|| foreground_monitor(app))
        .flatten()
        .or_else(|| {
            app.get_webview_window(AppWindowKind::Pet.as_str())
                .and_then(|pet| pet.current_monitor().ok().flatten())
        })
        .or(app.primary_monitor()?);
    let Some(monitor) = monitor else {
        return Ok(());
    };

    match spec.kind {
        AppWindowKind::Danmaku => {
            if window.outer_position()? != *monitor.position() {
                window.set_position(*monitor.position())?;
            }
            if window.inner_size()? != *monitor.size() {
                window.set_size(*monitor.size())?;
            }
        }
        AppWindowKind::Pet => {
            let scale = monitor.scale_factor();
            let width = (spec.width as f64 * scale).round() as i32;
            let height = (spec.height as f64 * scale).round() as i32;
            let margin = (24.0 * scale).round() as i32;
            let x = monitor.position().x + monitor.size().width as i32 - width - margin;
            let y = monitor.position().y + monitor.size().height as i32 - height - margin;
            window.set_position(PhysicalPosition::new(x, y))?;
        }
        AppWindowKind::Launcher | AppWindowKind::Control => window.center()?,
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_exclusion_is_limited_to_the_fullscreen_overlay() {
        for kind in [
            AppWindowKind::Launcher,
            AppWindowKind::Control,
            AppWindowKind::Pet,
        ] {
            assert!(!WindowSpec::for_kind(kind).capture_excluded());
        }
        assert!(WindowSpec::for_kind(AppWindowKind::Danmaku).capture_excluded());
    }

    #[test]
    fn transparent_helper_windows_have_intentional_focus_behavior() {
        let pet = WindowSpec::for_kind(AppWindowKind::Pet);
        assert!(pet.transparent);
        assert!(pet.focusable);
        assert!(!pet.decorations);
        assert!(pet.always_on_top);
        assert!(pet.skip_taskbar);

        let danmaku = WindowSpec::for_kind(AppWindowKind::Danmaku);
        assert!(danmaku.transparent);
        assert!(!danmaku.focusable);
        assert!(!danmaku.decorations);
        assert!(danmaku.always_on_top);
        assert!(danmaku.skip_taskbar);
    }

    #[test]
    fn pet_window_reserves_space_for_chat_without_changing_the_asset_contract() {
        let pet = WindowSpec::for_kind(AppWindowKind::Pet);

        assert_eq!((pet.width, pet.height), (180, 320));
        assert_eq!(crate::commands::PET_CONTAINER_WIDTH, 320);
        assert_eq!(crate::commands::PET_CONTAINER_HEIGHT, 480);
    }

    #[test]
    fn only_danmaku_is_click_through() {
        assert!(!WindowSpec::for_kind(AppWindowKind::Control).click_through);
        assert!(!WindowSpec::for_kind(AppWindowKind::Pet).click_through);
        assert!(WindowSpec::for_kind(AppWindowKind::Danmaku).click_through);
    }

    #[test]
    fn only_danmaku_uses_the_full_monitor_surface() {
        assert!(!WindowSpec::for_kind(AppWindowKind::Launcher).covers_monitor);
        assert!(!WindowSpec::for_kind(AppWindowKind::Control).covers_monitor);
        assert!(!WindowSpec::for_kind(AppWindowKind::Pet).covers_monitor);
        assert!(WindowSpec::for_kind(AppWindowKind::Danmaku).covers_monitor);
    }

    #[test]
    fn control_window_is_a_regular_focusable_window() {
        let spec = WindowSpec::for_kind(AppWindowKind::Control);
        assert!(!spec.transparent);
        assert!(spec.decorations);
        assert!(spec.focusable);
        assert!(spec.resizable);
        assert!(!spec.always_on_top);
    }

    #[test]
    fn launcher_window_keeps_native_title_controls_without_a_title() {
        let spec = WindowSpec::for_kind(AppWindowKind::Launcher);
        assert_eq!(spec.title, "");
        assert!(spec.decorations);
        assert!(!spec.resizable);
    }

    #[test]
    fn closing_one_of_two_recovery_windows_keeps_the_app_reachable() {
        assert!(!should_exit_after_window_destroyed(
            "control",
            ["pet", "danmaku"]
        ));
        assert!(!should_exit_after_window_destroyed(
            "pet",
            ["control", "danmaku"]
        ));
    }

    #[test]
    fn closing_the_last_recovery_window_exits_even_with_danmaku_open() {
        assert!(should_exit_after_window_destroyed("control", ["danmaku"]));
        assert!(should_exit_after_window_destroyed("pet", ["danmaku"]));
        assert!(should_exit_after_window_destroyed("pet", ["internal"]));
    }

    #[test]
    fn closing_a_non_recovery_window_does_not_control_the_app_lifecycle() {
        assert!(!should_exit_after_window_destroyed("danmaku", ["pet"]));
        assert!(!should_exit_after_window_destroyed(
            "unknown",
            std::iter::empty::<&str>()
        ));
    }
}
