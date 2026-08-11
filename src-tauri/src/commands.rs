//! Tauri IPC command boundary and account-switch transaction orchestration.
//!
//! Commands validate all frontend input before delegating to repositories or
//! platform adapters. Saved credentials are returned only by the explicit
//! single-profile reveal command.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono_tz::Tz;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, State, ipc::Channel};
use uuid::Uuid;
use yaat_contracts::{
    ActivateProviderRequest, ActivationMode, ApiError, AppSettings, BootstrapResponse,
    CaptureCredentialsRequest, CliStatus, CreateProviderRequest, DeactivateGlobalRequest,
    DeleteProviderRequest, HistoryApplyRequest, HistoryApplyResult, HistoryPreview,
    HistoryPreviewRequest, HistoryScope, HistorySyncState, HistorySyncStatus, LaunchRequest,
    LoginRequest, ModelFetchRequest, ModelFetchResponse, OperationProgress, OperationResult,
    Platform, PlatformState, ProfileStatus, ProviderCredentialRequest, ProviderCredentialResponse,
    ProviderImportCandidate, ProviderImportCommitRequest, ProviderImportCredentialState,
    ProviderImportPreview, ProviderImportPreviewRequest, ProviderImportResult,
    ProviderImportSource, ProviderKind, ProviderProfile, ReleaseUpdate, SecretKind, UpdateProgress,
    UpdateProviderRequest, UsageQueryRequest, UsageReport, UsageRescanRequest,
};
use zeroize::{Zeroize, Zeroizing};

use crate::activation::{
    ConfigFormat, OwnedPath, PatchEngine, PatchOperation, PathChange, PathState, PreparedPatch,
    RollbackOutcome, remove_atomically, replace_atomically,
};
use crate::app_state::{AppState, CliProbe};
use crate::db::{ProviderBatchInsert, ProviderBatchSensitiveRecord, SensitiveRecordKey};
use crate::error::{AppError, AppResult};
use crate::platform::{
    CredentialSnapshot, CredentialState, DiscoveredProvider, PlatformAdapter, ProfileRuntime,
    SidecarPlan, claude::ClaudeAdapter, claude_desktop::ClaudeDesktopAdapter, codex::CodexAdapter,
};
use crate::usage::service;
use crate::{history, launcher, paths, process, updates, validation};

const AUTH_SNAPSHOT_KIND: &str = "provider.auth_snapshot.v1";
const GLOBAL_BASELINE_KIND: &str = "global.baseline.v1";
const OFFICIAL_CREDENTIAL_EXPORT_FORMAT: &str = "yaat.official-credential";
const OFFICIAL_CREDENTIAL_EXPORT_VERSION: u32 = 1;
const MAX_SIDECAR_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PortableOfficialCredential {
    format: String,
    version: u32,
    platform: Platform,
    storage_kind: String,
    account_label: Option<String>,
    credential: serde_json::Value,
}

#[derive(Serialize, Deserialize, Zeroize)]
struct StoredCredentialSnapshot {
    storage_kind: String,
    opaque_payload: Vec<u8>,
    account_label: Option<String>,
}

impl From<&CredentialSnapshot> for StoredCredentialSnapshot {
    fn from(value: &CredentialSnapshot) -> Self {
        Self {
            storage_kind: value.storage_kind.clone(),
            opaque_payload: value.opaque_payload.clone(),
            account_label: value.account_label.clone(),
        }
    }
}

impl From<StoredCredentialSnapshot> for CredentialSnapshot {
    fn from(mut value: StoredCredentialSnapshot) -> Self {
        Self {
            storage_kind: std::mem::take(&mut value.storage_kind),
            opaque_payload: std::mem::take(&mut value.opaque_payload),
            account_label: value.account_label.take(),
            warning: None,
        }
    }
}

#[derive(Serialize, Deserialize, Zeroize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum StoredCredentialState {
    Present { snapshot: StoredCredentialSnapshot },
    Absent,
}

impl From<&CredentialState> for StoredCredentialState {
    fn from(value: &CredentialState) -> Self {
        match value {
            CredentialState::Present(snapshot) => Self::Present {
                snapshot: StoredCredentialSnapshot::from(snapshot),
            },
            CredentialState::Absent => Self::Absent,
        }
    }
}

impl From<StoredCredentialState> for CredentialState {
    fn from(value: StoredCredentialState) -> Self {
        match value {
            StoredCredentialState::Present { snapshot } => Self::Present(snapshot.into()),
            StoredCredentialState::Absent => Self::Absent,
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum StoredConfigFormat {
    Toml,
    Json,
    Jsonc,
}

impl From<ConfigFormat> for StoredConfigFormat {
    fn from(value: ConfigFormat) -> Self {
        match value {
            ConfigFormat::Toml => Self::Toml,
            ConfigFormat::Json => Self::Json,
            ConfigFormat::Jsonc => Self::Jsonc,
        }
    }
}

impl From<StoredConfigFormat> for ConfigFormat {
    fn from(value: StoredConfigFormat) -> Self {
        match value {
            StoredConfigFormat::Toml => Self::Toml,
            StoredConfigFormat::Json => Self::Json,
            StoredConfigFormat::Jsonc => Self::Jsonc,
        }
    }
}

#[derive(Deserialize, Serialize)]
struct StoredPathChange {
    path: String,
    before_exists: bool,
    before: Option<serde_json::Value>,
    after_exists: bool,
    after: Option<serde_json::Value>,
}

impl From<&PathChange> for StoredPathChange {
    fn from(value: &PathChange) -> Self {
        Self {
            path: value.path.to_json_pointer(),
            before_exists: value.before.exists,
            before: value.before.value.clone(),
            after_exists: value.after.exists,
            after: value.after.value.clone(),
        }
    }
}

impl StoredPathChange {
    fn into_change(self) -> AppResult<PathChange> {
        Ok(PathChange {
            path: OwnedPath::from_json_pointer(&self.path)
                .map_err(|error| AppError::ConfigMalformed(error.to_string()))?,
            before: PathState {
                exists: self.before_exists,
                value: self.before,
            },
            after: PathState {
                exists: self.after_exists,
                value: self.after,
            },
        })
    }

    fn update_after(&mut self, change: &PathChange) {
        self.after_exists = change.after.exists;
        self.after.clone_from(&change.after.value);
    }
}

#[derive(Deserialize, Serialize)]
struct StoredGlobalBaseline {
    version: u32,
    config_path: String,
    config_format: StoredConfigFormat,
    config_existed: bool,
    changes: Vec<StoredPathChange>,
    /// `None` means no official credential has been switched during this
    /// global-management session. `Some(Absent)` records that the shared slot
    /// was empty before YAAT first wrote an official account.
    previous_credential: Option<StoredCredentialState>,
}

impl Drop for StoredGlobalBaseline {
    fn drop(&mut self) {
        if let Some(state) = self.previous_credential.as_mut() {
            state.zeroize();
        }
    }
}

#[tauri::command]
pub fn bootstrap(state: State<'_, AppState>) -> Result<BootstrapResponse, ApiError> {
    for platform in Platform::ALL {
        schedule_history_sync(&state, platform);
    }
    api(bootstrap_inner(&state))
}

#[tauri::command]
pub async fn cli_status_refresh(
    state: State<'_, AppState>,
) -> Result<Vec<PlatformState>, ApiError> {
    let settings = state.repository.load_settings().map_err(AppError::from)?;
    let probe_settings = settings.clone();
    let probes = finish_background(tauri::async_runtime::spawn_blocking(move || {
        probe_all_clients(&probe_settings)
    }))
    .await?;
    let mut platforms = Vec::with_capacity(probes.len());
    for probe in probes {
        if let Some(path) = probe.path.as_ref() {
            state.cache_cli_probe(CliProbe {
                platform: probe.platform,
                path: path.clone(),
                status: probe.status,
                version: probe.version.clone(),
                error: probe.error.clone(),
            });
        }
        platforms.push(platform_state_from_probe(&state, &settings, probe)?);
    }
    Ok(platforms)
}

#[tauri::command]
pub async fn app_update_check(
    app: AppHandle,
    state: State<'_, updates::UpdateState>,
) -> Result<Option<ReleaseUpdate>, ApiError> {
    api(updates::check(&app, state.inner()).await)
}

#[tauri::command]
pub async fn app_update_install(
    app: AppHandle,
    state: State<'_, updates::UpdateState>,
    on_progress: Channel<UpdateProgress>,
) -> Result<(), ApiError> {
    api(updates::install(&app, state.inner(), on_progress).await)
}

#[tauri::command]
pub fn app_update_cancel(state: State<'_, updates::UpdateState>) -> Result<(), ApiError> {
    api(updates::cancel(state.inner()))
}

#[tauri::command]
pub fn provider_create(
    state: State<'_, AppState>,
    request: CreateProviderRequest,
) -> Result<ProviderProfile, ApiError> {
    api(provider_create_inner(&state, &request))
}

#[tauri::command]
pub fn provider_update(
    state: State<'_, AppState>,
    request: UpdateProviderRequest,
) -> Result<ProviderProfile, ApiError> {
    api((|| {
        validation::validate_update(&request)?;
        let current = require_profile(&state, &request.id)?;
        validation::validate_existing_profile_update(&current, &request)?;
        let replacement_official_credential = request
            .replacement_official_credential
            .as_deref()
            .map(|value| import_official_credential(current.platform, value))
            .transpose()?;
        validate_platform_profile_shape(
            current.platform,
            current.kind,
            request.model.as_deref(),
            request.secret_kind,
        )?;
        let execution_changed = provider_execution_changed(&current, &request);
        let globally_active = execution_changed && is_globally_active_profile(&state, &current)?;
        if globally_active {
            ensure_platform_stopped(current.platform)?;
        }
        let previous_secret =
            if globally_active && current.kind != ProviderKind::OfficialSubscription {
                state
                    .repository
                    .load_provider_secret(&current.id)
                    .map_err(AppError::from)?
            } else {
                None
            };
        let previous_official_credential =
            if globally_active && replacement_official_credential.is_some() {
                load_snapshot_optional(&state, &current.id)?
            } else {
                None
            };
        let updated = state
            .repository
            .update_provider(&request)
            .map_err(AppError::from)?;
        let updated = match replacement_official_credential {
            Some(snapshot) => install_official_credential(&state, &updated, &snapshot),
            None => Ok(updated),
        };
        let updated = match updated {
            Ok(updated) => updated,
            Err(error) if globally_active => {
                let rollback = rollback_active_provider_update(
                    &state,
                    &current,
                    previous_secret.as_ref(),
                    previous_official_credential.as_ref(),
                );
                return Err(with_provider_update_rollback(error, &rollback));
            }
            Err(error) => return Err(error),
        };
        if globally_active && let Err(error) = activate_global(&state, &updated) {
            let rollback = rollback_active_provider_update(
                &state,
                &current,
                previous_secret.as_ref(),
                previous_official_credential.as_ref(),
            );
            return Err(with_provider_update_rollback(error, &rollback));
        }
        Ok(updated)
    })())
}

#[tauri::command]
pub fn provider_credential_get(
    state: State<'_, AppState>,
    request: ProviderCredentialRequest,
) -> Result<ProviderCredentialResponse, ApiError> {
    api((|| {
        let profile = require_profile(&state, &request.id)?;
        let credential = if profile.kind == ProviderKind::OfficialSubscription {
            load_snapshot_optional(&state, &profile.id)?
                .map(|snapshot| export_official_credential(profile.platform, &snapshot))
                .transpose()?
        } else {
            state
                .repository
                .load_provider_secret(&profile.id)
                .map_err(AppError::from)?
                .map(|secret| secret.expose_secret().to_owned())
        };
        Ok(ProviderCredentialResponse { credential })
    })())
}

#[tauri::command]
pub async fn provider_models_fetch(
    request: ModelFetchRequest,
) -> Result<ModelFetchResponse, ApiError> {
    api(crate::model_fetch::fetch(&request)
        .await
        .map_err(AppError::Command))
}

#[tauri::command]
pub fn provider_delete(
    state: State<'_, AppState>,
    request: DeleteProviderRequest,
) -> Result<OperationResult, ApiError> {
    api((|| {
        require_profile(&state, &request.id)?;
        state
            .repository
            .delete_provider(&request)
            .map_err(AppError::from)?;
        Ok(operation("provider deleted", None))
    })())
}

#[tauri::command]
pub fn provider_activate(
    state: State<'_, AppState>,
    request: ActivateProviderRequest,
) -> Result<OperationResult, ApiError> {
    api((|| {
        if request.mode != ActivationMode::GlobalCredential {
            return Err(AppError::Validation(
                "managed profiles are launched directly and are not globally activated".into(),
            ));
        }
        let profile = require_ready_profile(&state, &request.profile_id)?;
        if profile.platform != request.platform {
            return Err(AppError::Validation(
                "provider does not belong to the selected platform".into(),
            ));
        }
        activate_global(&state, &profile)
    })())
}

#[tauri::command]
pub fn provider_global_deactivate(
    state: State<'_, AppState>,
    request: DeactivateGlobalRequest,
) -> Result<OperationResult, ApiError> {
    api(deactivate_global(&state, request.platform))
}

#[tauri::command]
pub fn provider_login(
    state: State<'_, AppState>,
    request: LoginRequest,
) -> Result<OperationResult, ApiError> {
    api((|| {
        let profile = require_profile(&state, &request.profile_id)?;
        state.validate_profile_for_platform(&profile)?;
        let settings = state.repository.load_settings().map_err(AppError::from)?;
        let context = state.context(profile.platform, &settings);
        if profile.platform == Platform::ClaudeDesktop {
            process::ensure_claude_desktop_is_stopped()?;
        }
        let spec = state
            .adapter(profile.platform)
            .login_spec(
                &context,
                ProfileRuntime {
                    profile: &profile,
                    secret: None,
                },
                request.console,
            )
            .map_err(AppError::Command)?;
        let home = paths::managed_profile_home(profile.platform, &profile.id)?;
        if profile.platform == Platform::ClaudeDesktop {
            launcher::spawn(spec)?;
        } else {
            launcher::spawn_terminal(spec)?;
        }
        let warning = state
            .repository
            .update_provider_runtime_state(
                &profile.id,
                ProfileStatus::NeedsLogin,
                Some(path_text(&home)?),
            )
            .err()
            .map(|error| {
                format!(
                    "login started, but YAAT could not update the account status; capture credentials after login: {error}"
                )
            });
        Ok(operation("official login started", warning))
    })())
}

#[tauri::command]
pub fn provider_capture(
    state: State<'_, AppState>,
    request: CaptureCredentialsRequest,
) -> Result<OperationResult, ApiError> {
    api(provider_capture_inner(&state, &request.profile_id))
}

#[tauri::command]
pub fn provider_import_preview(
    state: State<'_, AppState>,
    request: ProviderImportPreviewRequest,
) -> Result<ProviderImportPreview, ApiError> {
    api(discover_provider_import(&state, request.platform).map(|discovery| discovery.preview))
}

#[tauri::command]
pub fn provider_import_commit(
    state: State<'_, AppState>,
    request: ProviderImportCommitRequest,
) -> Result<ProviderImportResult, ApiError> {
    api(commit_provider_import(&state, request))
}

#[tauri::command]
pub fn profile_launch(
    state: State<'_, AppState>,
    request: LaunchRequest,
) -> Result<OperationResult, ApiError> {
    api((|| {
        let profile_id = request
            .profile_id
            .as_deref()
            .ok_or_else(|| AppError::Validation("select a provider before launching it".into()))?;
        let profile = require_ready_profile(&state, profile_id)?;
        if profile.platform != request.platform {
            return Err(AppError::Validation(
                "launch profile does not belong to the selected platform".into(),
            ));
        }
        state.validate_profile_for_platform(&profile)?;
        let cwd = validate_launch_cwd(profile.platform, request.cwd.as_deref())?;
        let settings = state.repository.load_settings().map_err(AppError::from)?;
        let mut warning = None;
        let context = state.context(profile.platform, &settings);
        let stored_secret = state
            .repository
            .load_provider_secret(&profile.id)
            .map_err(AppError::from)?;
        if profile.platform == Platform::ClaudeDesktop {
            process::ensure_claude_desktop_is_stopped()?;
        }
        let spec = state
            .adapter(profile.platform)
            .launch_spec(
                &context,
                ProfileRuntime {
                    profile: &profile,
                    secret: stored_secret
                        .as_ref()
                        .map(secrecy::ExposeSecret::expose_secret),
                },
                cwd,
                Vec::new(),
            )
            .map_err(AppError::Command)?;
        if profile.platform == Platform::ClaudeDesktop {
            launcher::spawn(spec)?;
        } else {
            launcher::spawn_terminal(spec)?;
        }
        if let Err(error) = state
            .repository
            .set_last_managed_profile(profile.platform, Some(&profile.id))
        {
            let binding_warning = format!(
                "profile launched, but YAAT could not remember it as the last managed account: {error}"
            );
            warning = Some(match warning {
                Some(existing) => format!("{existing}; {binding_warning}"),
                None => binding_warning,
            });
        }
        schedule_history_sync(&state, profile.platform);
        Ok(operation("managed profile launched", warning))
    })())
}

#[tauri::command]
pub async fn usage_query(
    state: State<'_, AppState>,
    request: UsageQueryRequest,
    on_progress: Channel<OperationProgress>,
) -> Result<UsageReport, ApiError> {
    let roots = usage_roots(&state, request.platform).map_err(ApiError::from)?;
    let repository = state.repository.clone();
    let cancelled = state.begin_usage_operation();
    finish_background(tauri::async_runtime::spawn_blocking(move || {
        service::scan_cancellable(
            &repository,
            request.platform,
            &roots,
            &cancelled,
            progress_sender(on_progress),
        )?;
        service::query(&repository, &request)
    }))
    .await
}

#[tauri::command]
pub async fn usage_rescan(
    state: State<'_, AppState>,
    request: UsageRescanRequest,
    on_progress: Channel<OperationProgress>,
) -> Result<OperationResult, ApiError> {
    let roots = usage_roots(&state, request.platform).map_err(ApiError::from)?;
    let repository = state.repository.clone();
    let cancelled = state.begin_usage_operation();
    finish_background(tauri::async_runtime::spawn_blocking(move || {
        let summary = service::scan_full_cancellable(
            &repository,
            request.platform,
            &roots,
            &cancelled,
            progress_sender(on_progress),
        )?;
        Ok(operation(
            &format!(
                "indexed {} local usage events from {} files",
                summary.indexed, summary.diagnostics.files_scanned
            ),
            None,
        ))
    }))
    .await
}

#[tauri::command]
pub fn usage_cancel(state: State<'_, AppState>) {
    state.cancel_usage_operation();
}

#[tauri::command]
pub fn settings_update(
    state: State<'_, AppState>,
    request: AppSettings,
) -> Result<AppSettings, ApiError> {
    api((|| {
        validate_settings(&request)?;
        let current = state.repository.load_settings().map_err(AppError::from)?;
        if current.unify_codex_history && !request.unify_codex_history {
            return Err(AppError::Validation(
                "Codex unified history cannot be disabled after it is enabled".into(),
            ));
        }
        reject_active_root_change(&state, &current, &request)?;
        if !current.unify_codex_history && request.unify_codex_history {
            let normalized = state
                .repository
                .list_history_sync_status()
                .map_err(AppError::from)?
                .into_iter()
                .any(|status| {
                    status.scope == HistoryScope::Codex
                        && status.state == HistorySyncState::Completed
                });
            if !normalized {
                return Err(AppError::Validation(
                    "run Codex history unification successfully before enabling it".into(),
                ));
            }
            process::ensure_codex_history_clients_stopped()?;
            let binding = state
                .repository
                .get_platform_binding(Platform::Codex)
                .map_err(AppError::from)?;
            let active_kind = binding
                .global_profile_id
                .as_deref()
                .map(|id| require_profile(&state, id))
                .transpose()?
                .map(|profile| profile.kind);
            if active_kind.is_none_or(|kind| kind == ProviderKind::OfficialSubscription) {
                apply_codex_unified_shell(&state, &request)?;
            }
        }
        let saved = state
            .repository
            .save_settings(&request)
            .map_err(AppError::from)?;
        if current.unify_claude_code_history && !saved.unify_claude_code_history {
            state.cancel_queued_history(HistoryScope::ClaudeCode);
        }
        if current.unify_claude_desktop_code_history && !saved.unify_claude_desktop_code_history {
            state.cancel_queued_history(HistoryScope::ClaudeDesktopCode);
        }
        Ok(saved)
    })())
}

#[tauri::command]
pub async fn history_preview(
    state: State<'_, AppState>,
    request: HistoryPreviewRequest,
    on_progress: Channel<OperationProgress>,
) -> Result<HistoryPreview, ApiError> {
    let roots = history_roots(&state, request.scope).map_err(ApiError::from)?;
    let cancelled = state.begin_history_operation();
    finish_background(tauri::async_runtime::spawn_blocking(move || {
        history::preview_cancellable(
            request.scope,
            roots,
            request.target_group_id.as_deref(),
            &cancelled,
            progress_sender(on_progress),
        )
    }))
    .await
}

#[tauri::command]
pub async fn history_apply(
    state: State<'_, AppState>,
    request: HistoryApplyRequest,
    on_progress: Channel<OperationProgress>,
) -> Result<HistoryApplyResult, ApiError> {
    let roots = history_roots(&state, request.scope).map_err(ApiError::from)?;
    let repository = Arc::clone(&state.repository);
    let status_repository = Arc::clone(&state.repository);
    let scope = request.scope;
    let cancelled = state.begin_history_operation();
    let _ = repository.save_history_sync_status(&HistorySyncStatus {
        scope,
        state: HistorySyncState::Scanning,
        ..HistorySyncStatus::default()
    });
    let task = tauri::async_runtime::spawn_blocking(move || {
        let mut send = progress_sender(on_progress);
        let mut normalizing = false;
        history::apply_full_indexed_cancellable(
            &repository,
            scope,
            roots,
            request.target_group_id.as_deref(),
            &cancelled,
            || ensure_history_clients_stopped(scope),
            move |progress| {
                if scope == HistoryScope::Codex
                    && progress.phase == yaat_contracts::OperationPhase::Saving
                    && !normalizing
                {
                    let _ = status_repository.save_history_sync_status(&HistorySyncStatus {
                        scope,
                        state: HistorySyncState::Normalizing,
                        processed_files: progress.processed,
                        ..HistorySyncStatus::default()
                    });
                    normalizing = true;
                }
                send(progress);
            },
        )
    });
    let result = match task.await {
        Ok(result) => result,
        Err(error) => Err(AppError::Internal(format!(
            "background task failed: {error}"
        ))),
    };
    let status = match &result {
        Ok(result) => HistorySyncStatus {
            scope,
            state: HistorySyncState::Completed,
            processed_files: result
                .copied
                .saturating_add(result.metadata_updated)
                .saturating_add(result.identical_files),
            last_completed_at: Some(chrono::Utc::now().timestamp_millis()),
            error_summary: None,
        },
        Err(AppError::Cancelled) => HistorySyncStatus {
            scope,
            state: HistorySyncState::Cancelled,
            ..HistorySyncStatus::default()
        },
        Err(error) => HistorySyncStatus {
            scope,
            state: HistorySyncState::Failed,
            error_summary: Some(truncate_error(&error.to_string())),
            ..HistorySyncStatus::default()
        },
    };
    let _ = state.repository.save_history_sync_status(&status);
    api(result)
}

#[tauri::command]
pub fn history_cancel(state: State<'_, AppState>) {
    state.cancel_history_operation();
}

#[tauri::command]
pub fn history_sync_status(state: State<'_, AppState>) -> Result<Vec<HistorySyncStatus>, ApiError> {
    api(state
        .repository
        .list_history_sync_status()
        .map_err(AppError::from))
}

fn activate_global(state: &AppState, profile: &ProviderProfile) -> AppResult<OperationResult> {
    ensure_platform_stopped(profile.platform)?;
    let loaded_baseline = load_global_baseline(state, profile.platform)?;
    let settings = state.repository.load_settings().map_err(AppError::from)?;
    let context = state.context(profile.platform, &settings);
    if profile.platform == Platform::ClaudeCode
        && profile.kind == ProviderKind::OfficialSubscription
    {
        crate::platform::claude::ensure_global_credential_namespace()
            .map_err(AppError::Credential)?;
    }
    let stored_secret = state
        .repository
        .load_provider_secret(&profile.id)
        .map_err(AppError::from)?;
    let runtime = ProfileRuntime {
        profile,
        secret: stored_secret
            .as_ref()
            .map(secrecy::ExposeSecret::expose_secret),
    };
    let mut plan = state
        .adapter(profile.platform)
        .global_config_plan(&context, runtime.clone())
        .map_err(AppError::ConfigMalformed)?;
    if profile.platform == Platform::Codex
        && profile.kind == ProviderKind::OfficialSubscription
        && !settings.unify_codex_history
    {
        plan.operations = loaded_baseline
            .as_ref()
            .map(baseline_restore_operations)
            .transpose()?
            .unwrap_or_default();
    }
    let mut sidecars = prepare_sidecars(plan.sidecars)?;
    let prepared =
        PatchEngine::prepare_file(&plan.path, plan.format, plan.operations).map_err(patch_error)?;
    let config_path = path_text(prepared.path())?.to_owned();
    let config_root = prepared
        .path()
        .parent()
        .ok_or_else(|| AppError::ConfigMalformed("global config has no parent directory".into()))?
        .to_path_buf();

    let mut target_credential = if profile.kind == ProviderKind::OfficialSubscription {
        Some(load_snapshot(state, &profile.id)?)
    } else if profile.platform == Platform::ClaudeDesktop {
        Some(
            crate::platform::claude_desktop::global_credential_for_profile(
                profile,
                runtime.secret.ok_or_else(|| {
                    AppError::Credential("Claude Desktop provider credential is unavailable".into())
                })?,
            )
            .map_err(AppError::Credential)?,
        )
    } else {
        None
    };
    let current_credential = if target_credential.is_some() {
        Some(
            state
                .adapter(profile.platform)
                .capture_credential_state(&context, &config_root)
                .map_err(AppError::Credential)?,
        )
    } else {
        None
    };
    let credential_warning = current_credential
        .as_ref()
        .and_then(CredentialState::warning)
        .map(str::to_owned);
    if profile.platform == Platform::ClaudeDesktop {
        let binding = state
            .repository
            .get_platform_binding(Platform::ClaudeDesktop)
            .map_err(AppError::from)?;
        if let (Some(current_profile_id), Some(CredentialState::Present(current_snapshot))) = (
            binding.global_profile_id.as_deref(),
            current_credential.as_ref(),
        ) {
            let current_profile = require_profile(state, current_profile_id)?;
            if current_profile.kind == ProviderKind::OfficialSubscription {
                store_snapshot(state, current_profile_id, current_snapshot)?;
                if current_profile_id == profile.id {
                    target_credential = Some(current_snapshot.clone());
                }
            }
        }
    }

    let previous_baseline = loaded_baseline
        .as_ref()
        .map(encode_global_baseline)
        .transpose()?;
    let mut baseline = loaded_baseline.unwrap_or_else(|| StoredGlobalBaseline {
        version: 1,
        config_path: config_path.clone(),
        config_format: prepared.format().into(),
        config_existed: prepared.before_existed(),
        changes: Vec::new(),
        previous_credential: None,
    });
    validate_baseline(&baseline, &config_path, prepared.format())?;
    merge_baseline_changes(&mut baseline, &prepared);
    if baseline.previous_credential.is_none() {
        baseline.previous_credential = current_credential.as_ref().map(StoredCredentialState::from);
    }
    store_global_baseline(state, profile.platform, &baseline)?;

    if let Err(error) = commit_sidecars(&mut sidecars) {
        let baseline_rollback = restore_global_baseline_state(
            state,
            profile.platform,
            previous_baseline.as_ref().map(|value| value.as_slice()),
        );
        return Err(AppError::ConfigMalformed(format!(
            "sidecar update failed: {error}; baseline rollback: {baseline_rollback}"
        )));
    }

    let applied = match PatchEngine::commit(prepared) {
        Ok(applied) => applied,
        Err(error) => {
            let sidecar_rollback = rollback_sidecars(&mut sidecars);
            let baseline_rollback = restore_global_baseline_state(
                state,
                profile.platform,
                previous_baseline.as_ref().map(|value| value.as_slice()),
            );
            let error = patch_error(error);
            return Err(match error {
                AppError::ConfigConflict(message) => AppError::ConfigConflict(format!(
                    "{message}; sidecar rollback: {sidecar_rollback}; baseline rollback: {baseline_rollback}"
                )),
                AppError::ConfigMalformed(message) => AppError::ConfigMalformed(format!(
                    "{message}; sidecar rollback: {sidecar_rollback}; baseline rollback: {baseline_rollback}"
                )),
                other => other,
            });
        }
    };
    if let Some(target) = target_credential.as_ref()
        && let Err(error) =
            state
                .adapter(profile.platform)
                .restore_credentials(&context, &config_root, target)
    {
        let rollback = rollback_switch(
            state,
            profile.platform,
            &context,
            &config_root,
            current_credential.as_ref(),
            &applied,
            &mut sidecars,
        );
        let baseline_rollback = if rollback.complete {
            restore_global_baseline_state(
                state,
                profile.platform,
                previous_baseline.as_ref().map(|value| value.as_slice()),
            )
        } else {
            "kept for recovery because the external rollback was incomplete".into()
        };
        return Err(AppError::Credential(format!(
            "credential switch failed: {error}; rollback: {}; baseline rollback: {baseline_rollback}",
            rollback.summary
        )));
    }
    if let Err(error) = state
        .repository
        .set_global_profile(profile.platform, Some(&profile.id))
    {
        let rollback = rollback_switch(
            state,
            profile.platform,
            &context,
            &config_root,
            current_credential.as_ref(),
            &applied,
            &mut sidecars,
        );
        let baseline_rollback = if rollback.complete {
            restore_global_baseline_state(
                state,
                profile.platform,
                previous_baseline.as_ref().map(|value| value.as_slice()),
            )
        } else {
            "kept for recovery because the external rollback was incomplete".into()
        };
        return Err(AppError::Database(format!(
            "failed to record active provider: {error}; rollback: {}; baseline rollback: {baseline_rollback}",
            rollback.summary
        )));
    }

    schedule_history_sync(state, profile.platform);
    Ok(operation("global provider activated", credential_warning))
}

fn deactivate_global(state: &AppState, platform: Platform) -> AppResult<OperationResult> {
    ensure_platform_stopped(platform)?;
    let Some(mut baseline) = load_global_baseline(state, platform)? else {
        let settings = state.repository.load_settings().map_err(AppError::from)?;
        if platform == Platform::Codex && settings.unify_codex_history {
            apply_codex_unified_shell(state, &settings)?;
        }
        state
            .repository
            .set_global_profile(platform, None)
            .map_err(AppError::from)?;
        return Ok(operation("global management was already inactive", None));
    };
    if baseline.version != 1 {
        return Err(AppError::UnsupportedConfigVersion(format!(
            "global baseline version {} is unsupported",
            baseline.version
        )));
    }
    let settings = state.repository.load_settings().map_err(AppError::from)?;
    let context = state.context(platform, &settings);
    let config_path = PathBuf::from(&baseline.config_path);
    let config_root = config_path
        .parent()
        .ok_or_else(|| AppError::ConfigMalformed("global config has no parent directory".into()))?;
    let current_credential = if baseline.previous_credential.is_some() {
        Some(
            state
                .adapter(platform)
                .capture_credential_state(&context, config_root)
                .map_err(AppError::Credential)?,
        )
    } else {
        None
    };
    let warning = current_credential
        .as_ref()
        .and_then(CredentialState::warning)
        .map(str::to_owned);
    if let Some(previous) = baseline.previous_credential.take() {
        let previous: CredentialState = previous.into();
        state
            .adapter(platform)
            .restore_credential_state(&context, config_root, &previous)
            .map_err(AppError::Credential)?;
    }
    let changes = std::mem::take(&mut baseline.changes)
        .into_iter()
        .map(StoredPathChange::into_change)
        .collect::<AppResult<Vec<_>>>()?;
    if let Err(error) = PatchEngine::rollback_recorded(
        &config_path,
        baseline.config_format.into(),
        changes,
        baseline.config_existed,
    )
    .map_err(patch_error)
    {
        if let Some(current) = current_credential.as_ref() {
            let _ =
                state
                    .adapter(platform)
                    .restore_credential_state(&context, config_root, current);
        }
        return Err(error);
    }
    if platform == Platform::Codex && settings.unify_codex_history {
        apply_codex_unified_shell(state, &settings)?;
    }
    state
        .repository
        .clear_global_profile_and_delete_sensitive_record(
            platform,
            &global_baseline_record_id(platform),
        )
        .map_err(AppError::from)?;
    Ok(operation(
        "global management stopped; original account fields restored",
        warning,
    ))
}

fn apply_codex_unified_shell(state: &AppState, settings: &AppSettings) -> AppResult<()> {
    let profile = ProviderProfile {
        id: "unified-official-shell".into(),
        platform: Platform::Codex,
        kind: ProviderKind::OfficialSubscription,
        name: "OpenAI".into(),
        secret_kind: SecretKind::None,
        status: ProfileStatus::Ready,
        platform_config: yaat_contracts::ProviderPlatformConfig::empty_for(Platform::Codex),
        ..ProviderProfile::default()
    };
    let context = state.context(Platform::Codex, settings);
    let plan = state
        .adapter(Platform::Codex)
        .global_config_plan(
            &context,
            ProfileRuntime {
                profile: &profile,
                secret: None,
            },
        )
        .map_err(AppError::ConfigMalformed)?;
    let mut sidecars = prepare_sidecars(plan.sidecars)?;
    let prepared =
        PatchEngine::prepare_file(&plan.path, plan.format, plan.operations).map_err(patch_error)?;
    commit_sidecars(&mut sidecars).map_err(AppError::ConfigMalformed)?;
    if let Err(error) = PatchEngine::commit(prepared).map_err(patch_error) {
        let rollback = rollback_sidecars(&mut sidecars);
        return Err(AppError::ConfigMalformed(format!(
            "{error}; sidecar rollback: {rollback}"
        )));
    }
    Ok(())
}

fn merge_baseline_changes(baseline: &mut StoredGlobalBaseline, prepared: &PreparedPatch) {
    let mut existing = baseline
        .changes
        .iter()
        .enumerate()
        .map(|(index, change)| (change.path.clone(), index))
        .collect::<HashMap<_, _>>();
    for change in prepared.changes() {
        let path = change.path.to_json_pointer();
        if let Some(index) = existing.get(&path).copied() {
            baseline.changes[index].update_after(change);
        } else {
            existing.insert(path, baseline.changes.len());
            baseline.changes.push(StoredPathChange::from(change));
        }
    }
}

fn baseline_restore_operations(baseline: &StoredGlobalBaseline) -> AppResult<Vec<PatchOperation>> {
    baseline
        .changes
        .iter()
        .map(|change| {
            let path = OwnedPath::from_json_pointer(&change.path)
                .map_err(|error| AppError::ConfigMalformed(error.to_string()))?;
            if change.before_exists {
                let value = change.before.clone().ok_or_else(|| {
                    AppError::ConfigMalformed(format!(
                        "global baseline is missing the original value for {}",
                        change.path
                    ))
                })?;
                Ok(PatchOperation::set(path, value))
            } else {
                Ok(PatchOperation::remove(path))
            }
        })
        .collect()
}

fn validate_baseline(
    baseline: &StoredGlobalBaseline,
    config_path: &str,
    format: ConfigFormat,
) -> AppResult<()> {
    if baseline.version != 1 {
        return Err(AppError::UnsupportedConfigVersion(format!(
            "global baseline version {} is unsupported",
            baseline.version
        )));
    }
    if baseline.config_path != config_path {
        return Err(AppError::ConfigConflict(format!(
            "YAAT is managing {}; stop global management before changing the config root to {config_path}",
            baseline.config_path
        )));
    }
    if ConfigFormat::from(baseline.config_format) != format {
        return Err(AppError::ConfigConflict(
            "the managed global config format changed unexpectedly".into(),
        ));
    }
    Ok(())
}

struct SwitchRollback {
    summary: String,
    complete: bool,
}

fn rollback_switch(
    state: &AppState,
    platform: Platform,
    context: &crate::platform::AdapterContext,
    config_root: &Path,
    previous_credential: Option<&CredentialState>,
    applied: &crate::activation::AppliedPatch,
    sidecars: &mut [PreparedSidecar],
) -> SwitchRollback {
    let (credential, credential_complete) = previous_credential.map_or_else(
        || ("not changed".to_owned(), true),
        |snapshot| match state.adapter(platform).restore_credential_state(
            context,
            config_root,
            snapshot,
        ) {
            Ok(()) => ("restored".to_owned(), true),
            Err(error) => (error, false),
        },
    );
    let (config, config_complete) = match PatchEngine::rollback(applied) {
        Ok(outcome) => (
            rollback_message(&outcome).to_owned(),
            !matches!(outcome, RollbackOutcome::Conflict { .. }),
        ),
        Err(error) => (error.to_string(), false),
    };
    let sidecar = rollback_sidecars(sidecars);
    let sidecar_complete = sidecar == "restored" || sidecar == "not changed";
    SwitchRollback {
        summary: format!("credential {credential}; config {config}; sidecars {sidecar}"),
        complete: credential_complete && config_complete && sidecar_complete,
    }
}

struct PreparedSidecar {
    path: PathBuf,
    before: Option<Vec<u8>>,
    desired: Option<Vec<u8>>,
    committed: bool,
}

fn prepare_sidecars(plans: Vec<SidecarPlan>) -> AppResult<Vec<PreparedSidecar>> {
    let data_root = paths::app_data_dir()?;
    let mut seen = std::collections::HashSet::new();
    plans
        .into_iter()
        .map(|plan| {
            if !plan.path.is_absolute() || !plan.path.starts_with(&data_root) {
                return Err(AppError::ConfigMalformed(format!(
                    "sidecar path must stay inside {}",
                    data_root.display()
                )));
            }
            if !seen.insert(plan.path.clone()) {
                return Err(AppError::ConfigMalformed(format!(
                    "duplicate sidecar path: {}",
                    plan.path.display()
                )));
            }
            if plan
                .contents
                .as_ref()
                .is_some_and(|contents| contents.len() as u64 > MAX_SIDECAR_BYTES)
            {
                return Err(AppError::ConfigMalformed(format!(
                    "sidecar exceeds the {MAX_SIDECAR_BYTES} byte limit: {}",
                    plan.path.display()
                )));
            }
            let before = read_sidecar(&plan.path)?;
            Ok(PreparedSidecar {
                path: plan.path,
                before,
                desired: plan.contents,
                committed: false,
            })
        })
        .collect()
}

fn read_sidecar(path: &Path) -> AppResult<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AppError::Io(error.to_string())),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(AppError::ConfigMalformed(format!(
            "refusing non-regular sidecar file: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_SIDECAR_BYTES {
        return Err(AppError::ConfigMalformed(format!(
            "sidecar exceeds the {MAX_SIDECAR_BYTES} byte limit: {}",
            path.display()
        )));
    }
    fs::read(path).map(Some).map_err(AppError::from)
}

fn commit_sidecars(sidecars: &mut [PreparedSidecar]) -> Result<(), String> {
    for index in 0..sidecars.len() {
        let path = sidecars[index].path.clone();
        let current = read_sidecar(&path).map_err(|error| error.to_string())?;
        if current != sidecars[index].before {
            let rollback = rollback_sidecars(&mut sidecars[..index]);
            return Err(format!(
                "{} changed outside YAAT; rollback: {rollback}",
                path.display()
            ));
        }
        let write_result = {
            let sidecar = &sidecars[index];
            write_sidecar(&sidecar.path, sidecar.desired.as_deref())
        };
        if let Err(error) = write_result {
            let rollback = rollback_sidecars(&mut sidecars[..index]);
            return Err(format!("{error}; rollback: {rollback}"));
        }
        sidecars[index].committed = true;
    }
    Ok(())
}

fn write_sidecar(path: &Path, contents: Option<&[u8]>) -> Result<(), String> {
    match contents {
        Some(contents) => {
            let parent = path
                .parent()
                .ok_or_else(|| format!("sidecar path has no parent: {}", path.display()))?;
            paths::ensure_private_directory(parent).map_err(|error| error.to_string())?;
            replace_atomically(path, contents).map_err(|error| error.to_string())?;
            paths::ensure_private_file(path).map_err(|error| error.to_string())
        }
        None => remove_atomically(path).map_err(|error| error.to_string()),
    }
}

fn rollback_sidecars(sidecars: &mut [PreparedSidecar]) -> String {
    if !sidecars.iter().any(|sidecar| sidecar.committed) {
        return "not changed".into();
    }
    let mut errors = Vec::new();
    for sidecar in sidecars
        .iter_mut()
        .rev()
        .filter(|sidecar| sidecar.committed)
    {
        match read_sidecar(&sidecar.path) {
            Ok(current) if current == sidecar.desired => {
                if let Err(error) = write_sidecar(&sidecar.path, sidecar.before.as_deref()) {
                    errors.push(error);
                } else {
                    sidecar.committed = false;
                }
            }
            Ok(_) => errors.push(format!(
                "{} changed outside YAAT after publication",
                sidecar.path.display()
            )),
            Err(error) => errors.push(error.to_string()),
        }
    }
    if errors.is_empty() {
        "restored".into()
    } else {
        errors.join("; ")
    }
}

fn rollback_message(outcome: &RollbackOutcome) -> &'static str {
    match outcome {
        RollbackOutcome::Restored { .. } => "restored",
        RollbackOutcome::RemovedFileCreatedByPatch => "removed",
        RollbackOutcome::AlreadyRolledBack => "unchanged",
        RollbackOutcome::Conflict { .. } => "conflicted",
    }
}

fn load_global_baseline(
    state: &AppState,
    platform: Platform,
) -> AppResult<Option<StoredGlobalBaseline>> {
    let record_id = global_baseline_record_id(platform);
    let stored = state
        .repository
        .load_sensitive_record(SensitiveRecordKey {
            profile_id: platform.as_str(),
            record_id: &record_id,
            kind: GLOBAL_BASELINE_KIND,
            provider_id: None,
        })
        .map_err(AppError::from)?;
    stored
        .map(|bytes| {
            serde_json::from_slice(bytes.expose())
                .map_err(|_| AppError::Credential("global baseline is malformed".into()))
        })
        .transpose()
}

fn store_global_baseline(
    state: &AppState,
    platform: Platform,
    baseline: &StoredGlobalBaseline,
) -> AppResult<()> {
    let encoded = encode_global_baseline(baseline)?;
    store_global_baseline_bytes(state, platform, &encoded)
}

fn encode_global_baseline(baseline: &StoredGlobalBaseline) -> AppResult<Zeroizing<Vec<u8>>> {
    Ok(Zeroizing::new(
        serde_json::to_vec(baseline).map_err(|error| AppError::Internal(error.to_string()))?,
    ))
}

fn store_global_baseline_bytes(
    state: &AppState,
    platform: Platform,
    encoded: &[u8],
) -> AppResult<()> {
    let record_id = global_baseline_record_id(platform);
    state
        .repository
        .store_sensitive_record(
            SensitiveRecordKey {
                profile_id: platform.as_str(),
                record_id: &record_id,
                kind: GLOBAL_BASELINE_KIND,
                provider_id: None,
            },
            encoded,
        )
        .map_err(AppError::from)
}

fn restore_global_baseline_state(
    state: &AppState,
    platform: Platform,
    previous: Option<&[u8]>,
) -> String {
    match previous {
        Some(encoded) => store_global_baseline_bytes(state, platform, encoded).map_or_else(
            |error| format!("failed: {error}"),
            |()| "restored".to_owned(),
        ),
        None => state
            .repository
            .delete_sensitive_record(&global_baseline_record_id(platform))
            .map_or_else(
                |error| format!("failed: {error}"),
                |deleted| {
                    if deleted {
                        "removed".to_owned()
                    } else {
                        "already absent".to_owned()
                    }
                },
            ),
    }
}

fn global_baseline_record_id(platform: Platform) -> String {
    format!("global/{}/baseline", platform.as_str())
}

#[derive(Clone, Debug)]
struct CliProbeResult {
    platform: Platform,
    path: Option<PathBuf>,
    status: CliStatus,
    version: Option<String>,
    error: Option<String>,
}

fn bootstrap_inner(state: &AppState) -> AppResult<BootstrapResponse> {
    let profiles = state
        .repository
        .list_providers(None)
        .map_err(AppError::from)?;
    let settings = state.repository.load_settings().map_err(AppError::from)?;
    let mut platforms = Vec::with_capacity(Platform::ALL.len());
    for platform in Platform::ALL {
        let context = state.context(platform, &settings);
        let probe = match state.adapter(platform).resolve_cli(&context) {
            Ok(path) => state.cached_cli_probe(platform, &path).map_or(
                CliProbeResult {
                    platform,
                    path: Some(path),
                    status: CliStatus::VersionUnknown,
                    version: None,
                    error: None,
                },
                |cached| CliProbeResult {
                    platform,
                    path: Some(cached.path),
                    status: cached.status,
                    version: cached.version,
                    error: cached.error,
                },
            ),
            Err(error) => CliProbeResult {
                platform,
                path: context.explicit_cli_path.clone(),
                status: if context.explicit_cli_path.is_some() {
                    CliStatus::Invalid
                } else {
                    CliStatus::Missing
                },
                version: None,
                error: Some(cli_error_summary(&error)),
            },
        };
        platforms.push(platform_state_from_probe(state, &settings, probe)?);
    }
    Ok(BootstrapResponse {
        profiles,
        platforms,
        settings,
        history_sync: state
            .repository
            .list_history_sync_status()
            .map_err(AppError::from)?,
    })
}

fn probe_all_clients(settings: &AppSettings) -> AppResult<Vec<CliProbeResult>> {
    std::thread::scope(|scope| {
        let handles =
            Platform::ALL.map(|platform| scope.spawn(move || probe_client(settings, platform)));
        let mut probes = Vec::with_capacity(handles.len());
        for handle in handles {
            probes.push(
                handle.join().map_err(|_| {
                    AppError::Internal("CLI version probe thread panicked".into())
                })??,
            );
        }
        Ok(probes)
    })
}

fn probe_client(settings: &AppSettings, platform: Platform) -> AppResult<CliProbeResult> {
    let context = AppState::context_for(platform, settings)?;
    match platform {
        Platform::Codex => probe_client_with(&CodexAdapter::new(), platform, &context),
        Platform::ClaudeCode => probe_client_with(&ClaudeAdapter::new(), platform, &context),
        Platform::ClaudeDesktop => {
            probe_client_with(&ClaudeDesktopAdapter::new(), platform, &context)
        }
    }
}

fn probe_client_with(
    adapter: &dyn PlatformAdapter,
    platform: Platform,
    context: &crate::platform::AdapterContext,
) -> AppResult<CliProbeResult> {
    let path = match adapter.resolve_cli(context) {
        Ok(path) => path,
        Err(error) => {
            return Ok(CliProbeResult {
                platform,
                path: context.explicit_cli_path.clone(),
                status: if context.explicit_cli_path.is_some() {
                    CliStatus::Invalid
                } else {
                    CliStatus::Missing
                },
                version: None,
                error: Some(cli_error_summary(&error)),
            });
        }
    };

    #[cfg(windows)]
    if platform == Platform::ClaudeDesktop {
        return Ok(CliProbeResult {
            platform,
            path: Some(path),
            status: CliStatus::Ready,
            version: None,
            error: None,
        });
    }

    Ok(match adapter.cli_version(&path) {
        Ok(version) => CliProbeResult {
            platform,
            path: Some(path),
            status: CliStatus::Ready,
            version: Some(version),
            error: None,
        },
        Err(error) => CliProbeResult {
            platform,
            path: Some(path),
            status: CliStatus::VersionUnknown,
            version: None,
            error: Some(cli_error_summary(&error)),
        },
    })
}

fn platform_state_from_probe(
    state: &AppState,
    settings: &AppSettings,
    probe: CliProbeResult,
) -> AppResult<PlatformState> {
    Ok(PlatformState {
        platform: probe.platform,
        cli_status: probe.status,
        cli_path: probe.path.map(|path| path.to_string_lossy().into_owned()),
        cli_version: probe.version,
        cli_error: probe.error,
        config_root: state
            .config_root(probe.platform, settings)?
            .to_string_lossy()
            .into_owned(),
        binding: state
            .repository
            .get_platform_binding(probe.platform)
            .map_err(AppError::from)?,
    })
}

fn cli_error_summary(error: &str) -> String {
    error
        .chars()
        .filter(|character| !character.is_control())
        .take(512)
        .collect()
}

fn provider_create_inner(
    state: &AppState,
    request: &CreateProviderRequest,
) -> AppResult<ProviderProfile> {
    validation::validate_create(request)?;
    let official_credential = request
        .official_credential
        .as_deref()
        .map(|value| import_official_credential(request.platform, value))
        .transpose()?;
    validate_platform_profile_shape(
        request.platform,
        request.kind,
        request.model.as_deref(),
        request.secret_kind,
    )?;
    let mut stored_request = request.clone();
    if stored_request.account_label.is_none() {
        stored_request.account_label = official_credential
            .as_ref()
            .and_then(|snapshot| snapshot.account_label.clone());
    }
    let profile = state
        .repository
        .create_provider(&stored_request)
        .map_err(AppError::from)?;
    if let Some(snapshot) = official_credential {
        return match install_official_credential(state, &profile, &snapshot) {
            Ok(profile) => Ok(profile),
            Err(error) => match state.repository.delete_provider(&DeleteProviderRequest {
                id: profile.id.clone(),
            }) {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(AppError::Internal(format!(
                    "{error}; failed to remove the incomplete account: {cleanup_error}"
                ))),
            },
        };
    }
    let home = paths::managed_profile_home(profile.platform, &profile.id)?;
    state
        .repository
        .update_provider_runtime_state(&profile.id, profile.status, Some(path_text(&home)?))
        .map_err(AppError::from)
}

fn install_official_credential(
    state: &AppState,
    profile: &ProviderProfile,
    snapshot: &CredentialSnapshot,
) -> AppResult<ProviderProfile> {
    if profile.kind != ProviderKind::OfficialSubscription {
        return Err(AppError::Validation(
            "official credential JSON can only be used by subscription accounts".into(),
        ));
    }
    let settings = state.repository.load_settings().map_err(AppError::from)?;
    let context = state.context(profile.platform, &settings);
    let home = state
        .adapter(profile.platform)
        .prepare_profile(
            &context,
            ProfileRuntime {
                profile,
                secret: None,
            },
        )
        .map_err(AppError::ConfigMalformed)?;
    state
        .adapter(profile.platform)
        .restore_credentials(&context, &home, snapshot)
        .map_err(AppError::Credential)?;
    store_snapshot(state, &profile.id, snapshot)?;
    state
        .repository
        .update_provider_runtime_state(&profile.id, ProfileStatus::Ready, Some(path_text(&home)?))
        .map_err(AppError::from)
}

fn validate_platform_profile_shape(
    platform: Platform,
    kind: ProviderKind,
    model: Option<&str>,
    secret_kind: SecretKind,
) -> AppResult<()> {
    if matches!(platform, Platform::ClaudeCode | Platform::ClaudeDesktop)
        && kind != ProviderKind::OfficialSubscription
        && !matches!(secret_kind, SecretKind::ApiKey | SecretKind::BearerToken)
    {
        return Err(AppError::Validation(
            "Claude API profiles require an API key or bearer token".into(),
        ));
    }
    if platform == Platform::ClaudeDesktop && kind != ProviderKind::OfficialSubscription {
        crate::platform::claude_desktop::validate_direct_model(
            model,
            kind == ProviderKind::ThirdParty,
        )
        .map_err(AppError::Validation)?;
    }
    Ok(())
}

fn provider_capture_inner(state: &AppState, profile_id: &str) -> AppResult<OperationResult> {
    let profile = require_profile(state, profile_id)?;
    if profile.kind != ProviderKind::OfficialSubscription {
        return Err(AppError::Validation(
            "only official subscription profiles capture private CLI credentials".into(),
        ));
    }
    let globally_active = is_globally_active_profile(state, &profile)?;
    let previous_snapshot = if globally_active {
        load_snapshot_optional(state, &profile.id)?
    } else {
        None
    };
    let settings = state.repository.load_settings().map_err(AppError::from)?;
    let context = state.context(profile.platform, &settings);
    let home = paths::managed_profile_home(profile.platform, &profile.id)?;
    let global_root = state.config_root(profile.platform, &settings)?;
    let capture_root = credential_capture_root(globally_active, &global_root, &home);
    if profile.platform == Platform::ClaudeDesktop {
        process::ensure_claude_desktop_is_stopped()?;
    }
    let snapshot = state
        .adapter(profile.platform)
        .capture_credentials(&context, capture_root)
        .map_err(AppError::Credential)?;
    let warning = snapshot.warning.clone();
    store_snapshot(state, &profile.id, &snapshot)?;
    let updated = state
        .repository
        .update_provider_runtime_state(&profile.id, ProfileStatus::Ready, Some(path_text(&home)?))
        .map_err(AppError::from)?;
    if globally_active && let Err(error) = activate_global(state, &updated) {
        let rollback = match previous_snapshot.as_ref() {
            Some(previous) => match install_official_credential(state, &profile, previous) {
                Ok(_) => "previous credential restored".into(),
                Err(rollback_error) => {
                    format!("failed to restore previous credential: {rollback_error}")
                }
            },
            None => "no previous credential snapshot was available".into(),
        };
        return Err(with_provider_update_rollback(error, &rollback));
    }
    Ok(operation("credentials captured", warning))
}

fn credential_capture_root<'a>(
    globally_active: bool,
    global_root: &'a Path,
    profile_home: &'a Path,
) -> &'a Path {
    if globally_active {
        global_root
    } else {
        profile_home
    }
}

enum ImportCredential {
    Direct(Option<Zeroizing<String>>),
    Official(CredentialSnapshot),
}

struct InternalImportCandidate {
    public: ProviderImportCandidate,
    credential: ImportCredential,
}

struct InternalImportDiscovery {
    preview: ProviderImportPreview,
    candidates: Vec<InternalImportCandidate>,
}

fn discover_provider_import(
    state: &AppState,
    platform: Platform,
) -> AppResult<InternalImportDiscovery> {
    if platform == Platform::ClaudeDesktop {
        process::ensure_claude_desktop_is_stopped()?;
    }
    let settings = state.repository.load_settings().map_err(AppError::from)?;
    let context = state.context(platform, &settings);
    let source_root = state.config_root(platform, &settings)?;
    let adapter = state.adapter(platform);
    let mut warnings = Vec::new();
    let mut direct = match adapter.discover_import_provider(&context, &source_root) {
        Ok(value) => value,
        Err(error) => {
            warnings.push(format!(
                "Unable to inspect the active provider configuration: {error}"
            ));
            None
        }
    };
    let official = match adapter.capture_import_official_credentials(&context, &source_root) {
        Ok(value) => Some(value),
        Err(error) => {
            warnings.push(format!(
                "Unable to inspect the official credential slot: {error}"
            ));
            None
        }
    };

    let mut candidates = Vec::new();
    if let Some(provider) = direct.as_mut() {
        if let Some(CredentialState::Present(snapshot)) = official.as_ref()
            && platform == Platform::Codex
            && provider.kind == ProviderKind::OfficialApi
            && provider.secret.is_none()
            && let Some(api_key) =
                crate::platform::codex::CodexAdapter::extract_api_key(&snapshot.opaque_payload)
                    .map_err(AppError::Credential)?
        {
            provider.secret_kind = SecretKind::ApiKey;
            provider.secret = Some(api_key);
            provider.credential_state = ProviderImportCredentialState::Ready;
        }
        candidates.push(direct_candidate(provider));
    }

    if let Some(CredentialState::Present(snapshot)) = official {
        let native_api_key = if platform == Platform::Codex {
            crate::platform::codex::CodexAdapter::extract_api_key(&snapshot.opaque_payload)
                .map_err(AppError::Credential)?
        } else {
            None
        };
        let merge_with_active_codex_api = native_api_key.as_ref().is_some_and(|native| {
            direct.as_ref().is_some_and(|provider| {
                provider.kind == ProviderKind::OfficialApi
                    && provider.secret.as_ref().is_some_and(|active| {
                        constant_time_equal(active.as_bytes(), native.as_bytes())
                    })
            })
        });
        if !merge_with_active_codex_api {
            candidates.push(official_candidate(platform, snapshot, direct.is_none())?);
        }
    }

    let source_revision = import_source_revision(platform, &candidates, &warnings)?;
    for candidate in &mut candidates {
        candidate.public.already_imported_provider_id =
            find_existing_import(state, platform, candidate)?;
    }
    let public = candidates
        .iter()
        .map(|candidate| candidate.public.clone())
        .collect();
    Ok(InternalImportDiscovery {
        preview: ProviderImportPreview {
            platform,
            source_revision,
            candidates: public,
            warnings,
        },
        candidates,
    })
}

fn direct_candidate(provider: &DiscoveredProvider) -> InternalImportCandidate {
    InternalImportCandidate {
        public: ProviderImportCandidate {
            candidate_id: provider.candidate_id.clone(),
            source: ProviderImportSource::ActiveConfig,
            active: true,
            kind: provider.kind,
            name: provider.name.clone(),
            account_label: provider.account_label.clone(),
            base_url: provider.base_url.clone(),
            model: provider.model.clone(),
            custom_headers: provider.custom_headers.clone(),
            user_agent: provider.user_agent.clone(),
            platform_config: provider.platform_config.clone(),
            secret_kind: provider.secret_kind,
            credential_state: provider.credential_state,
            already_imported_provider_id: None,
            warnings: provider.warnings.clone(),
        },
        credential: ImportCredential::Direct(provider.secret.clone()),
    }
}

fn official_candidate(
    platform: Platform,
    snapshot: CredentialSnapshot,
    active: bool,
) -> AppResult<InternalImportCandidate> {
    if platform == Platform::Codex
        && let Some(secret) =
            crate::platform::codex::CodexAdapter::extract_api_key(&snapshot.opaque_payload)
                .map_err(AppError::Credential)?
    {
        return Ok(InternalImportCandidate {
            public: ProviderImportCandidate {
                candidate_id: "official_credential".into(),
                source: ProviderImportSource::OfficialCredential,
                active,
                kind: ProviderKind::OfficialApi,
                name: "OpenAI API".into(),
                account_label: snapshot.account_label.clone(),
                base_url: None,
                model: None,
                custom_headers: Vec::new(),
                user_agent: None,
                platform_config: yaat_contracts::ProviderPlatformConfig::empty_for(platform),
                secret_kind: SecretKind::ApiKey,
                credential_state: ProviderImportCredentialState::Ready,
                already_imported_provider_id: None,
                warnings: snapshot.warning.clone().into_iter().collect(),
            },
            credential: ImportCredential::Direct(Some(secret)),
        });
    }
    let name = match platform {
        Platform::Codex => "OpenAI",
        Platform::ClaudeCode => "Claude",
        Platform::ClaudeDesktop => "Claude Desktop",
    };
    let warning = snapshot.warning.clone().into_iter().collect();
    Ok(InternalImportCandidate {
        public: ProviderImportCandidate {
            candidate_id: "official_credential".into(),
            source: ProviderImportSource::OfficialCredential,
            active,
            kind: ProviderKind::OfficialSubscription,
            name: name.into(),
            account_label: snapshot.account_label.clone(),
            base_url: None,
            model: None,
            custom_headers: Vec::new(),
            user_agent: None,
            platform_config: yaat_contracts::ProviderPlatformConfig::empty_for(platform),
            secret_kind: SecretKind::None,
            credential_state: ProviderImportCredentialState::Ready,
            already_imported_provider_id: None,
            warnings: warning,
        },
        credential: ImportCredential::Official(snapshot),
    })
}

fn import_source_revision(
    platform: Platform,
    candidates: &[InternalImportCandidate],
    warnings: &[String],
) -> AppResult<String> {
    let mut digest = Sha256::new();
    digest.update(platform.as_str().as_bytes());
    digest.update(
        serde_json::to_vec(warnings).map_err(|error| AppError::Internal(error.to_string()))?,
    );
    for candidate in candidates {
        let mut public = candidate.public.clone();
        public.already_imported_provider_id = None;
        digest.update(
            serde_json::to_vec(&public).map_err(|error| AppError::Internal(error.to_string()))?,
        );
        match &candidate.credential {
            ImportCredential::Direct(secret) => {
                digest.update(b"direct");
                if let Some(secret) = secret {
                    digest.update(secret.as_bytes());
                }
            }
            ImportCredential::Official(snapshot) => {
                digest.update(b"official");
                digest.update(snapshot.storage_kind.as_bytes());
                digest.update(&snapshot.opaque_payload);
            }
        }
    }
    Ok(hex::encode(digest.finalize()))
}

fn find_existing_import(
    state: &AppState,
    platform: Platform,
    candidate: &InternalImportCandidate,
) -> AppResult<Option<String>> {
    let profiles = state
        .repository
        .list_providers(Some(platform))
        .map_err(AppError::from)?;
    for profile in profiles {
        if profile.kind != candidate.public.kind
            || normalized_url(profile.base_url.as_deref())
                != normalized_url(candidate.public.base_url.as_deref())
            || profile.model != candidate.public.model
            || !headers_equal(&profile.custom_headers, &candidate.public.custom_headers)
            || profile.user_agent != candidate.public.user_agent
            || profile.platform_config != candidate.public.platform_config
        {
            continue;
        }
        let credentials_match = match &candidate.credential {
            ImportCredential::Direct(Some(candidate_secret)) => state
                .repository
                .load_provider_secret(&profile.id)
                .map_err(AppError::from)?
                .is_some_and(|saved| {
                    constant_time_equal(
                        saved.expose_secret().as_bytes(),
                        candidate_secret.as_bytes(),
                    )
                }),
            ImportCredential::Direct(None) => false,
            ImportCredential::Official(candidate_snapshot) => {
                load_snapshot_optional(state, &profile.id)?.is_some_and(|saved| {
                    saved.storage_kind == candidate_snapshot.storage_kind
                        && ((candidate_snapshot.account_label.is_some()
                            && saved.account_label == candidate_snapshot.account_label)
                            || constant_time_equal(
                                &saved.opaque_payload,
                                &candidate_snapshot.opaque_payload,
                            ))
                })
            }
        };
        if credentials_match {
            return Ok(Some(profile.id));
        }
    }
    Ok(None)
}

fn normalized_url(value: Option<&str>) -> Option<String> {
    value.map(|value| value.trim().trim_end_matches('/').to_ascii_lowercase())
}

fn headers_equal(
    left: &[yaat_contracts::HeaderEntry],
    right: &[yaat_contracts::HeaderEntry],
) -> bool {
    let normalize = |headers: &[yaat_contracts::HeaderEntry]| {
        let mut headers = headers
            .iter()
            .map(|header| {
                (
                    header.name.trim().to_ascii_lowercase(),
                    header.value.clone(),
                )
            })
            .collect::<Vec<_>>();
        headers.sort();
        headers
    };
    normalize(left) == normalize(right)
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let maximum = left.len().max(right.len());
    for index in 0..maximum {
        let left = left.get(index).copied().unwrap_or_default();
        let right = right.get(index).copied().unwrap_or_default();
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

fn commit_provider_import(
    state: &AppState,
    request: ProviderImportCommitRequest,
) -> AppResult<ProviderImportResult> {
    if request.selections.is_empty() || request.selections.len() > 8 {
        return Err(AppError::Validation(
            "select between one and eight discovered accounts".into(),
        ));
    }
    let discovery = discover_provider_import(state, request.platform)?;
    if !constant_time_equal(
        discovery.preview.source_revision.as_bytes(),
        request.source_revision.as_bytes(),
    ) {
        return Err(AppError::ConfigConflict(
            "the client configuration changed after the import preview; scan it again".into(),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    let mut inserts = Vec::with_capacity(request.selections.len());
    for selection in request.selections {
        if !seen.insert(selection.candidate_id.clone()) {
            return Err(AppError::Validation(
                "an import candidate was selected more than once".into(),
            ));
        }
        let candidate = discovery
            .candidates
            .iter()
            .find(|candidate| candidate.public.candidate_id == selection.candidate_id)
            .ok_or_else(|| {
                AppError::Validation("an import candidate is no longer available".into())
            })?;
        if candidate.public.already_imported_provider_id.is_some() {
            return Err(AppError::Validation(format!(
                "{} is already imported",
                candidate.public.name
            )));
        }
        let mut provider = selection.provider;
        if provider.platform != request.platform || provider.kind != candidate.public.kind {
            return Err(AppError::Validation(
                "an imported account cannot change platform or provider type".into(),
            ));
        }
        if provider.official_credential.is_some() {
            return Err(AppError::Validation(
                "imported credentials must come from the scanned source".into(),
            ));
        }
        let id = Uuid::new_v4().to_string();
        let mut sensitive_records = Vec::new();
        match &candidate.credential {
            ImportCredential::Direct(discovered_secret) => {
                if provider.secret.is_none() {
                    provider.secret = discovered_secret
                        .as_ref()
                        .map(|secret| secret.as_str().to_owned());
                }
                if provider.secret.is_none() {
                    return Err(AppError::Validation(format!(
                        "{} requires a direct credential",
                        candidate.public.name
                    )));
                }
            }
            ImportCredential::Official(snapshot) => {
                if provider.kind != ProviderKind::OfficialSubscription
                    || provider.secret_kind != SecretKind::None
                    || provider.secret.is_some()
                {
                    return Err(AppError::Validation(
                        "an official subscription import cannot contain an API credential".into(),
                    ));
                }
                if provider.account_label.is_none() {
                    provider.account_label = snapshot.account_label.clone();
                }
                let stored = StoredCredentialSnapshot::from(snapshot);
                let encoded = serde_json::to_vec(&stored)
                    .map_err(|error| AppError::Internal(error.to_string()))?;
                sensitive_records.push(ProviderBatchSensitiveRecord {
                    record_id: auth_snapshot_record_id(&id),
                    kind: AUTH_SNAPSHOT_KIND.into(),
                    value: encoded,
                });
            }
        }
        validation::validate_create(&provider)?;
        validate_platform_profile_shape(
            provider.platform,
            provider.kind,
            provider.model.as_deref(),
            provider.secret_kind,
        )?;
        if let yaat_contracts::ProviderPlatformConfig::ClaudeDesktop { models } =
            &provider.platform_config
        {
            crate::platform::claude_desktop::validate_direct_models(models, false)
                .map_err(AppError::Validation)?;
        }
        inserts.push(ProviderBatchInsert {
            id,
            request: provider,
            status: ProfileStatus::Ready,
            sensitive_records,
        });
    }
    let profiles = state
        .repository
        .import_providers(inserts)
        .map_err(AppError::from)?;
    Ok(ProviderImportResult { profiles })
}

fn store_snapshot(
    state: &AppState,
    profile_id: &str,
    snapshot: &CredentialSnapshot,
) -> AppResult<()> {
    let record_id = auth_snapshot_record_id(profile_id);
    let stored = StoredCredentialSnapshot::from(snapshot);
    let mut encoded = Zeroizing::new(
        serde_json::to_vec(&stored).map_err(|error| AppError::Internal(error.to_string()))?,
    );
    let result = state.repository.store_sensitive_record(
        SensitiveRecordKey {
            profile_id,
            record_id: &record_id,
            kind: AUTH_SNAPSHOT_KIND,
            provider_id: Some(profile_id),
        },
        &encoded,
    );
    encoded.zeroize();
    result.map_err(AppError::from)
}

pub(crate) fn load_snapshot(state: &AppState, profile_id: &str) -> AppResult<CredentialSnapshot> {
    load_snapshot_optional(state, profile_id)?
        .ok_or_else(|| AppError::NotFound("captured credential snapshot".into()))
}

fn load_snapshot_optional(
    state: &AppState,
    profile_id: &str,
) -> AppResult<Option<CredentialSnapshot>> {
    let record_id = auth_snapshot_record_id(profile_id);
    let Some(secret) = state
        .repository
        .load_sensitive_record(SensitiveRecordKey {
            profile_id,
            record_id: &record_id,
            kind: AUTH_SNAPSHOT_KIND,
            provider_id: Some(profile_id),
        })
        .map_err(AppError::from)?
    else {
        return Ok(None);
    };
    let stored: StoredCredentialSnapshot = serde_json::from_slice(secret.expose())
        .map_err(|_| AppError::Credential("credential snapshot is malformed".into()))?;
    Ok(Some(stored.into()))
}

fn export_official_credential(
    platform: Platform,
    snapshot: &CredentialSnapshot,
) -> AppResult<String> {
    let credential = serde_json::from_slice(&snapshot.opaque_payload)
        .map_err(|_| AppError::Credential("saved official credential is malformed".into()))?;
    serde_json::to_string_pretty(&PortableOfficialCredential {
        format: OFFICIAL_CREDENTIAL_EXPORT_FORMAT.into(),
        version: OFFICIAL_CREDENTIAL_EXPORT_VERSION,
        platform,
        storage_kind: snapshot.storage_kind.clone(),
        account_label: snapshot.account_label.clone(),
        credential,
    })
    .map_err(|error| AppError::Internal(error.to_string()))
}

fn import_official_credential(platform: Platform, value: &str) -> AppResult<CredentialSnapshot> {
    let document: serde_json::Value = serde_json::from_str(value)
        .map_err(|_| AppError::Validation("official credential must be valid JSON".into()))?;
    let portable = if document.get("format").and_then(serde_json::Value::as_str)
        == Some(OFFICIAL_CREDENTIAL_EXPORT_FORMAT)
    {
        let portable: PortableOfficialCredential = serde_json::from_value(document)
            .map_err(|_| AppError::Validation("official credential export is malformed".into()))?;
        if portable.version != OFFICIAL_CREDENTIAL_EXPORT_VERSION {
            return Err(AppError::Validation(format!(
                "official credential export version {} is unsupported",
                portable.version
            )));
        }
        if portable.platform != platform {
            return Err(AppError::Validation(format!(
                "official credential belongs to {}, not {}",
                portable.platform.as_str(),
                platform.as_str()
            )));
        }
        portable
    } else {
        PortableOfficialCredential {
            format: OFFICIAL_CREDENTIAL_EXPORT_FORMAT.into(),
            version: OFFICIAL_CREDENTIAL_EXPORT_VERSION,
            platform,
            storage_kind: official_credential_storage_kind(platform).into(),
            account_label: None,
            credential: document,
        }
    };
    let opaque_payload = serde_json::to_vec(&portable.credential)
        .map_err(|error| AppError::Internal(error.to_string()))?;
    Ok(CredentialSnapshot {
        storage_kind: portable.storage_kind,
        opaque_payload,
        account_label: portable.account_label,
        warning: None,
    })
}

const fn official_credential_storage_kind(platform: Platform) -> &'static str {
    match platform {
        Platform::Codex => "codex.auth-json.v1",
        Platform::ClaudeCode => "claude_code_account_fields_v1",
        Platform::ClaudeDesktop => "claude_desktop_credential_v1",
    }
}

fn history_roots(state: &AppState, scope: HistoryScope) -> AppResult<Vec<history::HistoryRoot>> {
    match scope {
        HistoryScope::Codex => codex_history_roots(state),
        HistoryScope::ClaudeCode => claude_code_history_roots(state),
        HistoryScope::ClaudeDesktopCode => claude_desktop_history_roots(state),
    }
}

fn codex_history_roots(state: &AppState) -> AppResult<Vec<history::HistoryRoot>> {
    standard_history_roots(state, Platform::Codex, "Codex global")
}

fn claude_code_history_roots(state: &AppState) -> AppResult<Vec<history::HistoryRoot>> {
    standard_history_roots(state, Platform::ClaudeCode, "Claude Code global")
}

fn standard_history_roots(
    state: &AppState,
    platform: Platform,
    global_label: &str,
) -> AppResult<Vec<history::HistoryRoot>> {
    let settings = state.repository.load_settings().map_err(AppError::from)?;
    let binding = state
        .repository
        .get_platform_binding(platform)
        .map_err(AppError::from)?;
    let mut roots = vec![history::HistoryRoot {
        id: "global".into(),
        label: global_label.into(),
        root_kind: "global".into(),
        path: state.config_root(platform, &settings)?,
        is_current: binding.global_profile_id.is_some(),
    }];
    for profile in state
        .repository
        .list_providers(Some(platform))
        .map_err(AppError::from)?
    {
        let path = paths::managed_profile_home(platform, &profile.id)?;
        if roots.iter().any(|root| root.path == path) {
            continue;
        }
        roots.push(history::HistoryRoot {
            id: format!("profile:{}", profile.id),
            label: profile.name,
            root_kind: "managed".into(),
            path,
            is_current: binding.last_managed_profile_id.as_deref() == Some(profile.id.as_str()),
        });
    }
    Ok(roots)
}

fn claude_desktop_history_roots(state: &AppState) -> AppResult<Vec<history::HistoryRoot>> {
    let binding = state
        .repository
        .get_platform_binding(Platform::ClaudeDesktop)
        .map_err(AppError::from)?;
    state
        .repository
        .list_providers(Some(Platform::ClaudeDesktop))
        .map_err(AppError::from)?
        .into_iter()
        .map(|profile| {
            Ok(history::HistoryRoot {
                id: format!("profile:{}", profile.id),
                label: profile.name,
                root_kind: "managed".into(),
                path: paths::managed_profile_home(Platform::ClaudeDesktop, &profile.id)?,
                is_current: binding.last_managed_profile_id.as_deref() == Some(profile.id.as_str()),
            })
        })
        .collect()
}

fn schedule_history_sync(state: &AppState, platform: Platform) {
    let scope = history_scope(platform);
    let settings = match state.repository.load_settings() {
        Ok(settings) => settings,
        Err(_) => return,
    };
    let enabled = match platform {
        Platform::Codex => settings.unify_codex_history,
        Platform::ClaudeCode => settings.unify_claude_code_history,
        Platform::ClaudeDesktop => settings.unify_claude_desktop_code_history,
    };
    if !enabled {
        return;
    }
    let Some(task) = state.begin_queued_history(scope) else {
        return;
    };
    let roots = match history_roots(state, scope) {
        Ok(roots) => roots,
        Err(error) => {
            let _ = state
                .repository
                .save_history_sync_status(&HistorySyncStatus {
                    scope,
                    state: HistorySyncState::Failed,
                    error_summary: Some(truncate_error(&error.to_string())),
                    ..HistorySyncStatus::default()
                });
            return;
        }
    };
    let target = (platform == Platform::ClaudeDesktop)
        .then_some(settings.claude_desktop_history_target)
        .flatten();
    let repository = Arc::clone(&state.repository);
    let waiting_repository = Arc::clone(&state.repository);
    let cancelled = task.cancelled();
    let _ = repository.save_history_sync_status(&HistorySyncStatus {
        scope,
        state: HistorySyncState::Queued,
        ..HistorySyncStatus::default()
    });
    tauri::async_runtime::spawn_blocking(move || {
        let result = if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            Err(AppError::Cancelled)
        } else {
            let current = repository.load_settings().map_err(AppError::from);
            current.and_then(|current| {
                if !history_sync_enabled(&current, scope) {
                    return Err(AppError::Cancelled);
                }
                let _ = repository.save_history_sync_status(&HistorySyncStatus {
                    scope,
                    state: HistorySyncState::Scanning,
                    ..HistorySyncStatus::default()
                });
                let mut normalizing = false;
                history::apply_incremental_cancellable(
                    &repository,
                    scope,
                    roots,
                    target.as_deref(),
                    &cancelled,
                    || {
                        let _ = waiting_repository.save_history_sync_status(&HistorySyncStatus {
                            scope,
                            state: HistorySyncState::Queued,
                            ..HistorySyncStatus::default()
                        });
                        wait_for_history_write_access(scope, &cancelled)?;
                        let current = repository.load_settings().map_err(AppError::from)?;
                        if history_sync_enabled(&current, scope) {
                            Ok(())
                        } else {
                            Err(AppError::Cancelled)
                        }
                    },
                    |progress| {
                        if scope == HistoryScope::Codex
                            && progress.phase == yaat_contracts::OperationPhase::Saving
                            && !normalizing
                        {
                            let _ = repository.save_history_sync_status(&HistorySyncStatus {
                                scope,
                                state: HistorySyncState::Normalizing,
                                processed_files: progress.processed,
                                ..HistorySyncStatus::default()
                            });
                            normalizing = true;
                        }
                    },
                )
            })
        };
        let status = match result {
            Ok(result) => HistorySyncStatus {
                scope,
                state: HistorySyncState::Completed,
                processed_files: result
                    .copied
                    .saturating_add(result.metadata_updated)
                    .saturating_add(result.identical_files),
                last_completed_at: Some(chrono::Utc::now().timestamp_millis()),
                error_summary: None,
            },
            Err(AppError::Cancelled) => HistorySyncStatus {
                scope,
                state: HistorySyncState::Cancelled,
                ..HistorySyncStatus::default()
            },
            Err(error) => HistorySyncStatus {
                scope,
                state: HistorySyncState::Failed,
                error_summary: Some(truncate_error(&error.to_string())),
                ..HistorySyncStatus::default()
            },
        };
        let _ = repository.save_history_sync_status(&status);
        drop(task);
    });
}

fn history_sync_enabled(settings: &AppSettings, scope: HistoryScope) -> bool {
    match scope {
        HistoryScope::Codex => settings.unify_codex_history,
        HistoryScope::ClaudeCode => settings.unify_claude_code_history,
        HistoryScope::ClaudeDesktopCode => settings.unify_claude_desktop_code_history,
    }
}

fn wait_for_history_write_access(
    scope: HistoryScope,
    cancelled: &std::sync::atomic::AtomicBool,
) -> AppResult<()> {
    loop {
        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(AppError::Cancelled);
        }
        match ensure_history_clients_stopped(scope) {
            Ok(()) => return Ok(()),
            Err(AppError::ConfigConflict(_)) => {
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(error) => return Err(error),
        }
    }
}

fn truncate_error(value: &str) -> String {
    value.chars().take(512).collect()
}

fn history_scope(platform: Platform) -> HistoryScope {
    match platform {
        Platform::Codex => HistoryScope::Codex,
        Platform::ClaudeCode => HistoryScope::ClaudeCode,
        Platform::ClaudeDesktop => HistoryScope::ClaudeDesktopCode,
    }
}

fn ensure_history_clients_stopped(scope: HistoryScope) -> AppResult<()> {
    match scope {
        HistoryScope::Codex => process::ensure_codex_history_clients_stopped(),
        HistoryScope::ClaudeCode => process::ensure_client_is_stopped(Platform::ClaudeCode),
        HistoryScope::ClaudeDesktopCode => process::ensure_claude_desktop_is_stopped(),
    }
}

fn ensure_platform_stopped(platform: Platform) -> AppResult<()> {
    if platform == Platform::Codex {
        process::ensure_codex_history_clients_stopped()
    } else if platform == Platform::ClaudeDesktop {
        process::ensure_claude_desktop_is_stopped()
    } else {
        process::ensure_client_is_stopped(platform)
    }
}

fn usage_roots(state: &AppState, platform: Platform) -> AppResult<Vec<service::UsageRoot>> {
    let profiles = state
        .repository
        .list_providers(Some(platform))
        .map_err(AppError::from)?;
    let settings = state.repository.load_settings().map_err(AppError::from)?;
    state.usage_roots(platform, &settings, &profiles)
}

fn reject_active_root_change(
    state: &AppState,
    current: &AppSettings,
    next: &AppSettings,
) -> AppResult<()> {
    for (platform, root_changed, cli_changed) in [
        (
            Platform::Codex,
            current.codex_home != next.codex_home,
            current.codex_path != next.codex_path,
        ),
        (
            Platform::ClaudeCode,
            current.claude_config_dir != next.claude_config_dir,
            current.claude_path != next.claude_path,
        ),
    ] {
        let path_changed = root_changed || cli_changed;
        let binding_active = path_changed
            && state
                .repository
                .get_platform_binding(platform)
                .map_err(AppError::from)?
                .global_profile_id
                .is_some();
        if binding_active {
            return Err(AppError::ConfigConflict(format!(
                "stop global management for {} before changing its CLI or config path",
                platform.as_str()
            )));
        }
    }
    Ok(())
}

fn validate_settings(settings: &AppSettings) -> AppResult<()> {
    if !matches!(settings.language.as_str(), "system" | "zh" | "en") {
        return Err(AppError::Validation("unsupported language".into()));
    }
    if !matches!(settings.theme.as_str(), "auto" | "light" | "dark") {
        return Err(AppError::Validation("unsupported theme".into()));
    }
    Tz::from_str(&settings.timezone)
        .map_err(|_| AppError::Validation("unknown IANA timezone".into()))?;
    if !matches!(
        settings.usage_refresh_interval_seconds,
        0 | 5 | 10 | 30 | 60
    ) {
        return Err(AppError::Validation(
            "usage refresh interval must be off, 5, 10, 30, or 60 seconds".into(),
        ));
    }
    if settings.unify_claude_desktop_code_history
        && settings.claude_desktop_history_target.is_none()
    {
        return Err(AppError::Validation(
            "choose a Claude Desktop history target before enabling automatic sync".into(),
        ));
    }
    for path in [
        settings.codex_path.as_deref(),
        settings.claude_path.as_deref(),
        settings.claude_desktop_path.as_deref(),
        settings.codex_home.as_deref(),
        settings.claude_config_dir.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if !Path::new(path).is_absolute() || path.chars().any(char::is_control) {
            return Err(AppError::Validation(
                "configured CLI and config paths must be absolute".into(),
            ));
        }
    }
    Ok(())
}

fn validate_launch_cwd(platform: Platform, value: Option<&str>) -> AppResult<Option<PathBuf>> {
    if platform == Platform::ClaudeDesktop {
        return Ok(None);
    }
    let value = value.ok_or_else(|| {
        AppError::Validation("choose an existing absolute project directory".into())
    })?;
    let path = PathBuf::from(value);
    if !path.is_absolute() || !path.is_dir() {
        return Err(AppError::Validation(
            "launch working directory must be an existing absolute directory".into(),
        ));
    }
    Ok(Some(path))
}

fn require_profile(state: &AppState, id: &str) -> AppResult<ProviderProfile> {
    paths::validate_identifier(id)?;
    state
        .repository
        .get_provider(id)
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound(format!("provider '{id}'")))
}

fn require_ready_profile(state: &AppState, id: &str) -> AppResult<ProviderProfile> {
    let profile = require_profile(state, id)?;
    if profile.status != ProfileStatus::Ready {
        return Err(AppError::Validation(
            "provider is not ready; complete login or credential capture first".into(),
        ));
    }
    Ok(profile)
}

fn provider_execution_changed(current: &ProviderProfile, request: &UpdateProviderRequest) -> bool {
    (current.platform == Platform::Codex
        && current.kind == ProviderKind::ThirdParty
        && current.name.trim() != request.name.trim())
        || current.base_url != request.base_url
        || current.model != request.model
        || current.custom_headers != request.custom_headers
        || current.user_agent != request.user_agent
        || current.platform_config != request.platform_config
        || current.secret_kind != request.secret_kind
        || request.replacement_secret.is_some()
        || request.replacement_official_credential.is_some()
}

fn is_globally_active_profile(state: &AppState, profile: &ProviderProfile) -> AppResult<bool> {
    let binding = state
        .repository
        .get_platform_binding(profile.platform)
        .map_err(AppError::from)?;
    Ok(binding.global_profile_id.as_deref() == Some(profile.id.as_str()))
}

fn rollback_active_provider_update(
    state: &AppState,
    previous: &ProviderProfile,
    previous_secret: Option<&secrecy::SecretString>,
    previous_official_credential: Option<&CredentialSnapshot>,
) -> String {
    let mut rollback_request = UpdateProviderRequest {
        id: previous.id.clone(),
        name: previous.name.clone(),
        account_label: previous.account_label.clone(),
        base_url: previous.base_url.clone(),
        model: previous.model.clone(),
        custom_headers: previous.custom_headers.clone(),
        user_agent: previous.user_agent.clone(),
        platform_config: previous.platform_config.clone(),
        secret_kind: previous.secret_kind,
        replacement_secret: previous_secret.map(|secret| secret.expose_secret().to_owned()),
        replacement_official_credential: None,
    };
    let provider_rollback = state.repository.update_provider(&rollback_request);
    if let Some(secret) = rollback_request.replacement_secret.as_mut() {
        secret.zeroize();
    }
    if let Err(error) = provider_rollback {
        return format!("failed to restore saved provider: {error}");
    }

    if let Some(snapshot) = previous_official_credential
        && let Err(error) = install_official_credential(state, previous, snapshot)
    {
        return format!("saved provider restored, but credential rollback failed: {error}");
    }
    "saved provider and credential restored".into()
}

fn with_provider_update_rollback(error: AppError, rollback: &str) -> AppError {
    match error {
        AppError::ConfigConflict(message) => {
            AppError::ConfigConflict(format!("{message}; provider rollback: {rollback}"))
        }
        AppError::ConfigMalformed(message) => {
            AppError::ConfigMalformed(format!("{message}; provider rollback: {rollback}"))
        }
        AppError::Credential(message) => {
            AppError::Credential(format!("{message}; provider rollback: {rollback}"))
        }
        AppError::Database(message) => {
            AppError::Database(format!("{message}; provider rollback: {rollback}"))
        }
        other => AppError::Internal(format!("{other}; provider rollback: {rollback}")),
    }
}

fn path_text(path: &Path) -> AppResult<&str> {
    path.to_str()
        .ok_or_else(|| AppError::Validation("path is not valid UTF-8".into()))
}

fn auth_snapshot_record_id(profile_id: &str) -> String {
    format!("provider/{profile_id}/auth-snapshot")
}

fn patch_error(error: crate::activation::PatchError) -> AppError {
    if error.is_external_conflict() {
        AppError::ConfigConflict(error.to_string())
    } else {
        AppError::ConfigMalformed(error.to_string())
    }
}

fn operation(message: &str, warning: Option<String>) -> OperationResult {
    OperationResult {
        message: message.into(),
        warning,
    }
}

async fn finish_background<T: Send + 'static>(
    task: tauri::async_runtime::JoinHandle<AppResult<T>>,
) -> Result<T, ApiError> {
    match task.await {
        Ok(result) => api(result),
        Err(error) => api(Err(AppError::Internal(format!(
            "background task failed: {error}"
        )))),
    }
}

fn progress_sender(
    channel: Channel<OperationProgress>,
) -> impl FnMut(OperationProgress) + Send + 'static {
    let mut last_phase = None;
    let mut last_processed = 0;
    let mut last_sent = Instant::now();
    move |progress| {
        let phase_changed = last_phase != Some(progress.phase);
        let finished = progress.total == Some(progress.processed);
        let advanced = progress.processed.saturating_sub(last_processed) >= 25;
        if phase_changed
            || finished
            || advanced
            || last_sent.elapsed() >= Duration::from_millis(100)
        {
            let _ = channel.send(progress);
            last_phase = Some(progress.phase);
            last_processed = progress.processed;
            last_sent = Instant::now();
        }
    }
}

fn api<T>(result: AppResult<T>) -> Result<T, ApiError> {
    result.map_err(ApiError::from)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn active_account_refresh_reads_the_global_credential_slot() {
        let global = Path::new("/Users/example/.codex");
        let profile = Path::new("/Users/example/.yaat/profiles/codex/account/home");

        assert_eq!(credential_capture_root(true, global, profile), global);
        assert_eq!(credential_capture_root(false, global, profile), profile);
    }

    #[test]
    fn import_preview_redacts_discovered_credentials_but_revision_tracks_them() {
        let discovered = |secret: &str| DiscoveredProvider {
            candidate_id: "active_config".into(),
            kind: ProviderKind::ThirdParty,
            name: "Gateway".into(),
            account_label: None,
            base_url: Some("https://gateway.example.com/v1".into()),
            model: Some("gpt-test".into()),
            custom_headers: Vec::new(),
            user_agent: None,
            platform_config: yaat_contracts::ProviderPlatformConfig::Codex {
                default_model: Some("gpt-test".into()),
                catalog: Vec::new(),
            },
            secret_kind: SecretKind::BearerToken,
            secret: Some(Zeroizing::new(secret.into())),
            credential_state: ProviderImportCredentialState::Ready,
            warnings: Vec::new(),
        };
        let first = vec![direct_candidate(&discovered("private-one"))];
        let second = vec![direct_candidate(&discovered("private-two"))];

        let serialized = serde_json::to_string(&first[0].public).unwrap();
        assert!(!serialized.contains("private-one"));
        assert_ne!(
            import_source_revision(Platform::Codex, &first, &[]).unwrap(),
            import_source_revision(Platform::Codex, &second, &[]).unwrap()
        );
    }

    fn change(path: &str, before: serde_json::Value, after: serde_json::Value) -> PathChange {
        PathChange {
            path: OwnedPath::from_json_pointer(path).unwrap(),
            before: PathState {
                exists: true,
                value: Some(before),
            },
            after: PathState {
                exists: true,
                value: Some(after),
            },
        }
    }

    #[test]
    fn stored_change_preserves_the_original_before_value_when_after_changes() {
        let first = change("/model", json!("original"), json!("a"));
        let second = change("/model", json!("a"), json!("b"));
        let mut stored = StoredPathChange::from(&first);
        stored.update_after(&second);
        let merged = stored.into_change().unwrap();
        assert_eq!(merged.before.value, Some(json!("original")));
        assert_eq!(merged.after.value, Some(json!("b")));
    }

    #[test]
    fn provider_display_edits_do_not_count_as_execution_changes() {
        let current = ProviderProfile {
            platform: Platform::ClaudeCode,
            kind: ProviderKind::ThirdParty,
            base_url: Some("https://api.example.com".into()),
            model: Some("model-a".into()),
            secret_kind: SecretKind::ApiKey,
            ..ProviderProfile::default()
        };
        let display_only = UpdateProviderRequest {
            id: "profile-a".into(),
            name: "Renamed".into(),
            account_label: Some("New label".into()),
            base_url: current.base_url.clone(),
            model: current.model.clone(),
            custom_headers: current.custom_headers.clone(),
            user_agent: current.user_agent.clone(),
            platform_config: current.platform_config.clone(),
            secret_kind: current.secret_kind,
            replacement_secret: None,
            replacement_official_credential: None,
        };
        assert!(!provider_execution_changed(&current, &display_only));

        let mut credential_change = display_only;
        credential_change.replacement_secret = Some("new-secret".into());
        assert!(provider_execution_changed(&current, &credential_change));
    }

    #[test]
    fn active_codex_third_party_name_change_regenerates_provider_config() {
        let current = ProviderProfile {
            platform: Platform::Codex,
            kind: ProviderKind::ThirdParty,
            name: "Old gateway".into(),
            base_url: Some("https://api.example.com".into()),
            model: Some("model-a".into()),
            secret_kind: SecretKind::ApiKey,
            ..ProviderProfile::default()
        };
        let request = UpdateProviderRequest {
            id: "profile-a".into(),
            name: "New gateway".into(),
            account_label: current.account_label.clone(),
            base_url: current.base_url.clone(),
            model: current.model.clone(),
            custom_headers: current.custom_headers.clone(),
            user_agent: current.user_agent.clone(),
            platform_config: current.platform_config.clone(),
            secret_kind: current.secret_kind,
            replacement_secret: None,
            replacement_official_credential: None,
        };

        assert!(provider_execution_changed(&current, &request));
    }

    #[test]
    fn official_credential_export_round_trips() {
        let snapshot = CredentialSnapshot {
            storage_kind: official_credential_storage_kind(Platform::Codex).into(),
            opaque_payload: serde_json::to_vec(&json!({
                "auth_mode": "chatgpt",
                "tokens": { "access_token": "access", "refresh_token": "refresh" }
            }))
            .unwrap(),
            account_label: Some("person@example.com".into()),
            warning: None,
        };

        let exported = export_official_credential(Platform::Codex, &snapshot).unwrap();
        let imported = import_official_credential(Platform::Codex, &exported).unwrap();

        assert_eq!(imported.storage_kind, snapshot.storage_kind);
        assert_eq!(imported.opaque_payload, snapshot.opaque_payload);
        assert_eq!(imported.account_label, snapshot.account_label);
        assert!(exported.contains("yaat.official-credential"));
    }

    #[test]
    fn official_credential_export_cannot_cross_platforms() {
        let snapshot = CredentialSnapshot {
            storage_kind: official_credential_storage_kind(Platform::Codex).into(),
            opaque_payload: br#"{"tokens":{"access_token":"access"}}"#.to_vec(),
            account_label: None,
            warning: None,
        };
        let exported = export_official_credential(Platform::Codex, &snapshot).unwrap();

        let error = import_official_credential(Platform::ClaudeCode, &exported).unwrap_err();
        assert!(error.to_string().contains("belongs to codex"));
    }

    #[test]
    fn stored_credential_state_distinguishes_absent_from_unmanaged() {
        let encoded = serde_json::to_vec(&StoredCredentialState::Absent).unwrap();
        let decoded: StoredCredentialState = serde_json::from_slice(&encoded).unwrap();
        assert!(matches!(decoded, StoredCredentialState::Absent));

        let baseline = StoredGlobalBaseline {
            version: 1,
            config_path: "/tmp/settings.json".into(),
            config_format: StoredConfigFormat::Json,
            config_existed: true,
            changes: Vec::new(),
            previous_credential: None,
        };
        assert!(baseline.previous_credential.is_none());
    }
}
