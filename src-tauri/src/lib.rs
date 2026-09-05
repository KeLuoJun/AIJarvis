mod commands;
mod windows;

use std::{thread, time::Duration};

use commands::{AppWindowKind, ResourceLayoutStatus};
use tauri::{Manager, RunEvent, WindowEvent};

pub(crate) struct AppState {
    resources: ResourceLayoutStatus,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            let resource_dir = app.path().resource_dir()?;
            let resources = commands::validate_resource_layout(&resource_dir);
            app.manage(AppState {
                resources: resources.clone(),
            });

            if smoke_test_enabled() {
                for kind in AppWindowKind::ALL {
                    windows::show_app_window(app.handle(), kind)?;
                }
                eprintln!(
                    "AI_JARVIS_SMOKE windows=control,pet,danmaku resources:model_config={},defaults_config={},qwen_preflight={},pet_manifest={}",
                    resources.api.model_config,
                    resources.api.defaults_config,
                    resources.qwen_preflight,
                    resources.api.pet_manifest
                );

                let app_handle = app.handle().clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(750));
                    app_handle.exit(0);
                });
            } else {
                windows::show_app_window(app.handle(), AppWindowKind::Pet)?;
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_bootstrap_state,
            commands::show_app_window,
            commands::close_app_window,
            commands::quit_app,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build AI Jarvis");

    app.run(|app, event| {
        if let RunEvent::WindowEvent {
            label,
            event: WindowEvent::Destroyed,
            ..
        } = event
        {
            // Tauri removes this label from its manager before emitting Destroyed.
            let remaining_windows = app.webview_windows();
            if windows::should_exit_after_window_destroyed(&label, remaining_windows.keys()) {
                app.exit(0);
            }
        }
    });
}

fn smoke_test_enabled() -> bool {
    std::env::var("AI_JARVIS_SMOKE_TEST").as_deref() == Ok("1")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    #[test]
    fn bundle_maps_only_the_declared_external_resources() {
        let config: Value = serde_json::from_str(include_str!("../tauri.conf.json"))
            .expect("tauri.conf.json must be valid JSON");
        let resources = config["bundle"]["resources"]
            .as_object()
            .expect("bundle.resources must be a source-to-target map");

        assert_eq!(resources.len(), 3);
        assert_eq!(resources["../resources/config/"], "resources/config/");
        assert_eq!(resources["../resources/prompts/"], "resources/prompts/");
        assert_eq!(resources["../public/assets/pet/"], "assets/pet/");
        assert_eq!(config["app"]["windows"], Value::Array(Vec::new()));
    }

    #[test]
    fn production_and_development_csp_are_explicit_and_minimal() {
        let config: Value = serde_json::from_str(include_str!("../tauri.conf.json"))
            .expect("tauri.conf.json must be valid JSON");
        let security = &config["app"]["security"];

        assert_eq!(
            security["csp"],
            "default-src 'self'; script-src 'self'; connect-src ipc: http://ipc.localhost; img-src 'self' asset: http://asset.localhost; style-src 'self' 'unsafe-inline'; object-src 'none'"
        );
        assert_eq!(
            security["devCsp"],
            "default-src 'self'; script-src 'self' 'unsafe-inline'; connect-src ipc: http://ipc.localhost ws://localhost:1420 ws://localhost:1421; img-src 'self' asset: http://asset.localhost; style-src 'self' 'unsafe-inline'; object-src 'none'"
        );

        for policy in [
            security["csp"].as_str().expect("production CSP string"),
            security["devCsp"].as_str().expect("development CSP string"),
        ] {
            assert!(!policy.contains('*'));
            assert!(!policy.contains("'unsafe-eval'"));
            assert!(!policy.contains("https:"));
        }
    }

    #[test]
    fn capability_has_only_drag_and_application_permissions() {
        let capability: Value = serde_json::from_str(include_str!("../capabilities/default.json"))
            .expect("capability must be valid JSON");
        let permissions = capability["permissions"]
            .as_array()
            .expect("permissions must be an array")
            .iter()
            .map(|permission| permission.as_str().expect("permission identifier"))
            .collect::<BTreeSet<_>>();
        let expected = BTreeSet::from([
            "allow-close-app-window",
            "allow-get-bootstrap-state",
            "allow-quit-app",
            "allow-show-app-window",
            "core:window:allow-start-dragging",
        ]);

        assert_eq!(permissions, expected);
        assert!(permissions.iter().all(|permission| {
            !permission.contains("shell")
                && !permission.contains("fs")
                && !permission.contains("http")
        }));
    }

    #[test]
    fn custom_permissions_expose_exactly_the_registered_commands() {
        let permissions: toml::Value = toml::from_str(include_str!("../permissions/app.toml"))
            .expect("custom permissions must be valid TOML");
        let commands = permissions["permission"]
            .as_array()
            .expect("permission entries")
            .iter()
            .flat_map(|permission| {
                permission["commands"]["allow"]
                    .as_array()
                    .expect("allowed commands")
            })
            .map(|command| command.as_str().expect("command name"))
            .collect::<BTreeSet<_>>();

        assert_eq!(
            commands,
            BTreeSet::from([
                "close_app_window",
                "get_bootstrap_state",
                "quit_app",
                "show_app_window",
            ])
        );
    }
}
