use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

fn main() {
    // Keep this manifest synchronized with `generate_handler!` in `src/lib.rs`
    // and the permissions granted by `capabilities/default.json`.
    const COMMANDS: &[&str] = &[
        "bootstrap",
        "cli_status_refresh",
        "app_update_check",
        "app_update_install",
        "app_update_cancel",
        "provider_create",
        "provider_update",
        "provider_credential_get",
        "provider_models_fetch",
        "provider_delete",
        "provider_activate",
        "provider_global_deactivate",
        "provider_login",
        "provider_capture",
        "provider_import_preview",
        "provider_import_commit",
        "profile_launch",
        "usage_query",
        "usage_rescan",
        "usage_cancel",
        "settings_update",
        "history_preview",
        "history_apply",
        "history_cancel",
        "history_sync_status",
    ];

    verify_command_permissions(COMMANDS, Path::new("capabilities/default.json"));

    let attributes = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS));

    tauri_build::try_build(attributes).expect("failed to build YAAT Tauri context");
}

fn verify_command_permissions(commands: &[&str], capability_path: &Path) {
    println!("cargo:rerun-if-changed={}", capability_path.display());
    let contents = fs::read_to_string(capability_path).unwrap_or_else(|error| {
        panic!(
            "failed to read Tauri capability {}: {error}",
            capability_path.display()
        )
    });
    let capability: serde_json::Value = serde_json::from_str(&contents).unwrap_or_else(|error| {
        panic!(
            "failed to parse Tauri capability {}: {error}",
            capability_path.display()
        )
    });
    let permissions = capability["permissions"]
        .as_array()
        .unwrap_or_else(|| panic!("Tauri capability must contain a permissions array"))
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect::<BTreeSet<_>>();
    let missing = commands
        .iter()
        .map(|command| format!("allow-{}", command.replace('_', "-")))
        .filter(|permission| !permissions.contains(permission.as_str()))
        .collect::<Vec<_>>();

    assert!(
        missing.is_empty(),
        "Tauri commands missing from capabilities/default.json: {}",
        missing.join(", ")
    );
}
