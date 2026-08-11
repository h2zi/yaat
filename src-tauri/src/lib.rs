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
mod model_fetch;
mod paths;
mod platform;
mod process;
mod updates;
mod usage;
mod validation;

use tauri::Manager;

use crate::app_state::AppState;

/// Starts YAAT.
///
/// # Panics
///
/// Panics when the Tauri runtime cannot be initialized. Recoverable
/// application-state failures are returned through Tauri's setup error path.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
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
            let state = AppState::open()
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
            commands::provider_models_fetch,
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
            commands::history_sync_status,
        ])
        .run(tauri::generate_context!())
        .expect("error while running YAAT");
}
