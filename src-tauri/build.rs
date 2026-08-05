fn main() {
    // Keep this manifest synchronized with `generate_handler!` in `src/lib.rs`
    // and the permissions granted by `capabilities/default.json`.
    const COMMANDS: &[&str] = &[
        "bootstrap",
        "app_update_check",
        "app_update_install",
        "app_update_cancel",
        "provider_create",
        "provider_update",
        "provider_credential_get",
        "provider_delete",
        "provider_activate",
        "provider_global_deactivate",
        "provider_login",
        "provider_capture",
        "provider_import_current",
        "profile_launch",
        "usage_query",
        "usage_rescan",
        "usage_cancel",
        "settings_update",
        "history_preview",
        "history_apply",
        "history_cancel",
    ];

    let attributes = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS));

    tauri_build::try_build(attributes).expect("failed to build YAAT Tauri context");
}
