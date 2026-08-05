//! Serializable contracts shared by the Tauri backend and its IPC boundary.
//!
//! Serde uses `snake_case` for enum values and `camelCase` for object fields so
//! these Rust definitions match the TypeScript contracts in `src/types.ts`.

use serde::{Deserialize, Serialize};

/// Client whose accounts and local data are managed by YAAT.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    #[default]
    Codex,
    ClaudeCode,
    ClaudeDesktop,
}

impl Platform {
    /// Every platform supported by this release.
    pub const ALL: [Self; 3] = [Self::Codex, Self::ClaudeCode, Self::ClaudeDesktop];

    /// Returns the stable value used in storage and IPC payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude_code",
            Self::ClaudeDesktop => "claude_desktop",
        }
    }
}

/// Authentication and endpoint model represented by a provider profile.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OfficialSubscription,
    OfficialApi,
    #[default]
    ThirdParty,
}

/// Way in which a profile is applied to a client.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationMode {
    #[default]
    ManagedLaunch,
    GlobalCredential,
}

/// Kind of reusable secret stored for a provider profile.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    None,
    #[default]
    ApiKey,
    BearerToken,
}

/// Readiness of a profile for launching or activation.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileStatus {
    #[default]
    Ready,
    NeedsLogin,
}

/// Non-secret provider metadata returned to the frontend.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProfile {
    pub id: String,
    pub platform: Platform,
    pub kind: ProviderKind,
    pub name: String,
    pub account_label: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub secret_kind: SecretKind,
    pub has_secret: bool,
    pub profile_home: Option<String>,
    pub status: ProfileStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Request to create a provider profile and optionally store its secret.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProviderRequest {
    pub platform: Platform,
    pub kind: ProviderKind,
    pub name: String,
    pub account_label: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub secret_kind: SecretKind,
    pub secret: Option<String>,
    pub official_credential: Option<String>,
}

/// Request to edit a provider profile and optionally replace its secret.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProviderRequest {
    pub id: String,
    pub name: String,
    pub account_label: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub secret_kind: SecretKind,
    pub replacement_secret: Option<String>,
    pub replacement_official_credential: Option<String>,
}

/// Request to reveal the saved credential for one provider profile.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCredentialRequest {
    pub id: String,
}

/// A saved credential returned only by an explicit reveal request.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCredentialResponse {
    pub credential: Option<String>,
}

/// Request to apply a provider using the selected activation mode.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivateProviderRequest {
    pub platform: Platform,
    pub profile_id: String,
    pub mode: ActivationMode,
}

/// Request to remove one provider profile.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteProviderRequest {
    pub id: String,
}

/// Active global and most recently launched managed profiles for a platform.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformBinding {
    pub platform: Platform,
    pub global_profile_id: Option<String>,
    pub last_managed_profile_id: Option<String>,
}

/// Runtime discovery state and binding information for one platform.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformState {
    pub platform: Platform,
    pub cli_found: bool,
    pub cli_path: Option<String>,
    pub cli_version: Option<String>,
    pub config_root: String,
    pub binding: PlatformBinding,
}

/// User-configurable application settings persisted by the backend.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub language: String,
    pub theme: String,
    pub timezone: String,
    pub default_activation_mode: ActivationMode,
    pub codex_path: Option<String>,
    pub claude_path: Option<String>,
    pub claude_desktop_path: Option<String>,
    pub codex_home: Option<String>,
    pub claude_config_dir: Option<String>,
    pub unify_codex_history: bool,
    pub unify_claude_code_history: bool,
    pub unify_claude_desktop_code_history: bool,
    pub claude_desktop_history_target: Option<String>,
}

/// Local session-history domain to scan or reconcile.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryScope {
    #[default]
    Codex,
    ClaudeCode,
    ClaudeDesktopCode,
}

/// One discovered source or target group for history reconciliation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryGroup {
    pub id: String,
    pub label: String,
    pub root_kind: String,
    pub is_current: bool,
    pub session_count: u64,
}

/// Request to calculate a history reconciliation plan without writing files.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPreviewRequest {
    pub scope: HistoryScope,
    pub target_group_id: Option<String>,
}

/// Summary of a history reconciliation plan.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPreview {
    pub scope: HistoryScope,
    pub groups: Vec<HistoryGroup>,
    pub target_group_id: Option<String>,
    pub files_scanned: u64,
    pub pending_copies: u64,
    pub metadata_updates: u64,
    pub identical_files: u64,
    pub conflicts: u64,
    pub invalid_files: u64,
}

/// Request to rescan and apply safe history changes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryApplyRequest {
    pub scope: HistoryScope,
    pub target_group_id: Option<String>,
}

/// Result of an applied history reconciliation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryApplyResult {
    pub scope: HistoryScope,
    pub copied: u64,
    pub metadata_updated: u64,
    pub identical_files: u64,
    pub conflicts: u64,
    pub invalid_files: u64,
}

/// Progress reported by a cancellable local scan.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationProgress {
    pub phase: OperationPhase,
    pub processed: u64,
    pub total: Option<u64>,
}

/// Current stage of a cancellable local scan.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    #[default]
    Discovering,
    Processing,
    Saving,
}

/// Initial non-secret state loaded by the frontend.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapResponse {
    pub profiles: Vec<ProviderProfile>,
    pub platforms: Vec<PlatformState>,
    pub settings: AppSettings,
}

/// Newer published YAAT version reported by the release service.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseUpdate {
    pub current_version: String,
    pub latest_version: String,
}

/// Stage of an updater download and installation.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePhase {
    #[default]
    Downloading,
    Installing,
}

/// Download progress emitted while installing a release update.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProgress {
    pub phase: UpdatePhase,
    pub downloaded: u64,
    pub total: Option<u64>,
}

/// Date range and timezone used to query local usage.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageQueryRequest {
    pub platform: Platform,
    pub start_date: String,
    pub end_date: String,
    pub timezone: String,
}

/// Request to rebuild one platform's local usage snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRescanRequest {
    pub platform: Platform,
}

/// Token categories normalized across supported clients.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenBreakdown {
    pub uncached_input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning_output: u64,
}

impl TokenBreakdown {
    /// Returns uncached, cache-read, and cache-write input tokens.
    #[must_use]
    pub const fn input(&self) -> u64 {
        self.uncached_input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }

    /// Returns input plus output tokens.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.input().saturating_add(self.output)
    }
}

/// Usage totals for one local calendar date.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBucket {
    pub date: String,
    pub tokens: TokenBreakdown,
    pub request_count: u64,
}

/// Scan coverage and data-quality counters shown with a usage report.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageDiagnostics {
    pub files_scanned: u64,
    pub malformed_records: u64,
    pub duplicate_records: u64,
    pub coverage_start: Option<i64>,
    pub coverage_end: Option<i64>,
    pub last_scanned_at: Option<i64>,
    pub is_partial: bool,
}

/// Aggregated local usage for a requested date range.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    pub platform: Platform,
    pub start_date: String,
    pub end_date: String,
    pub timezone: String,
    pub totals: TokenBreakdown,
    pub request_count: u64,
    pub buckets: Vec<UsageBucket>,
    pub diagnostics: UsageDiagnostics,
}

/// Common acknowledgement returned by state-changing commands.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationResult {
    pub message: String,
    pub warning: Option<String>,
}

/// Request to restore a platform's pre-YAAT global state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeactivateGlobalRequest {
    pub platform: Platform,
}

/// Request to open the native login flow for an official profile.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginRequest {
    pub profile_id: String,
    pub console: bool,
}

/// Request to capture credentials after an official login completes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureCredentialsRequest {
    pub profile_id: String,
}

/// Request to import the account active in a client's default configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportCurrentRequest {
    pub platform: Platform,
    pub name: String,
    pub account_label: Option<String>,
}

/// Request to launch a managed profile in an optional working directory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRequest {
    pub platform: Platform,
    pub profile_id: Option<String>,
    pub cwd: Option<String>,
}

/// Stable, serializable error returned across the Tauri IPC boundary.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    pub code: String,
    pub message: String,
}
