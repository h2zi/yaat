//! Native backend for the YAAT desktop application.
//!
//! The frontend can reach this crate only through the commands registered in
//! [`run`]. Filesystem access, credential storage, configuration patching, and
//! local usage indexing remain behind that IPC boundary.

pub mod activation;
mod app_state;
mod commands;
mod db;
mod error;
mod history;
mod launcher;
mod paths;
mod platform;
mod process;
mod updates;
mod usage;
mod validation;

use std::io::Write;

use secrecy::ExposeSecret;
use tauri::Manager;
use yaat_contracts::Platform;

use crate::app_state::AppState;

/// Starts YAAT or serves an internal credential-helper invocation.
///
/// # Panics
///
/// Panics when the current executable cannot be resolved or the Tauri runtime
/// cannot be initialized. Recoverable application-state failures are returned
/// through Tauri's setup error path.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    if let Some(exit_code) = run_credential_helper() {
        std::process::exit(exit_code);
    }
    let helper_executable = std::env::current_exe().expect("unable to resolve YAAT executable");
    tauri::Builder::default()
        // Tauri requires single-instance to run before plugins that may perform
        // startup work or interfere with secondary-instance delivery.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(move |app| {
            let state = AppState::open(helper_executable)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            app.manage(state);
            app.manage(updates::UpdateState::default());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::bootstrap,
            commands::app_update_check,
            commands::app_update_install,
            commands::app_update_cancel,
            commands::provider_create,
            commands::provider_update,
            commands::provider_credential_get,
            commands::provider_delete,
            commands::provider_activate,
            commands::provider_global_deactivate,
            commands::provider_login,
            commands::provider_capture,
            commands::provider_import_current,
            commands::profile_launch,
            commands::usage_query,
            commands::usage_rescan,
            commands::usage_cancel,
            commands::settings_update,
            commands::history_preview,
            commands::history_apply,
            commands::history_cancel,
        ])
        .run(tauri::generate_context!())
        .expect("error while running YAAT");
}

fn run_credential_helper() -> Option<i32> {
    let arguments = std::env::args().collect::<Vec<_>>();
    let marker = arguments
        .iter()
        .position(|value| value == "--yaat-credential-helper")?;
    let platform = match arguments.get(marker + 1).map(String::as_str) {
        Some("codex") => Platform::Codex,
        Some("claude_code") => Platform::ClaudeCode,
        Some("claude_desktop") => Platform::ClaudeDesktop,
        _ => return Some(2),
    };
    let Some(profile_id) = arguments.get(marker + 2) else {
        return Some(2);
    };
    let profile_id = profile_id.clone();
    if paths::validate_identifier(&profile_id).is_err() {
        return Some(2);
    }

    let Ok(database_path) = paths::database_path() else {
        return Some(3);
    };
    let Ok(repository) = db::Repository::open(database_path) else {
        return Some(3);
    };
    let profile = match repository.get_provider(&profile_id) {
        Ok(Some(profile)) if profile.platform == platform && profile.has_secret => profile,
        _ => return Some(4),
    };
    let Ok(Some(secret)) = repository.load_provider_secret(&profile.id) else {
        return Some(5);
    };

    let mut stdout = std::io::stdout().lock();
    if stdout
        .write_all(secret.expose_secret().as_bytes())
        .and_then(|_| stdout.write_all(b"\n"))
        .and_then(|_| stdout.flush())
        .is_err()
    {
        return Some(6);
    }
    Some(0)
}
