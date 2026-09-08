use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    App,
};

use crate::{commands::AppWindowKind, windows};

const OPEN_CONTROL_ID: &str = "tray-open-control";
const QUIT_ID: &str = "tray-quit";

pub(crate) fn setup(app: &App) -> tauri::Result<()> {
    let open_control = MenuItem::with_id(app, OPEN_CONTROL_ID, "打开控制台", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, QUIT_ID, "退出 AI Jarvis", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open_control, &quit])?;

    let mut tray = TrayIconBuilder::with_id("ai-jarvis")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("AI Jarvis")
        .on_menu_event(|app, event| match event.id().as_ref() {
            OPEN_CONTROL_ID => {
                if let Err(error) = windows::show_app_window(app, AppWindowKind::Control) {
                    eprintln!("control window could not be opened from the tray: {error}");
                }
            }
            QUIT_ID => crate::request_shutdown_and_exit(app),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}
