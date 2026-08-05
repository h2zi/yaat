// Claude Code configuration, launch, and secure-storage integration.

use std::borrow::Cow;
use std::collections::BTreeMap;
#[cfg(windows)]
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::time::Duration;

use base64::Engine as _;
use directories::BaseDirs;
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use serde_json::{Map, Value};
#[cfg(any(target_os = "macos", windows, test))]
use sha2::{Digest, Sha256};
#[cfg(any(target_os = "macos", windows, test))]
use unicode_normalization::UnicodeNormalization;
use yaat_contracts::{Platform, ProviderKind, SecretKind};
use zeroize::{Zeroize, Zeroizing};

use crate::activation::{
    ConfigFormat, OwnedPath, PatchOperation, remove_atomically, replace_atomically,
};

use super::{
    AdapterContext, CommandSpec, CredentialSnapshot, CredentialState, GlobalConfigPlan,
    PlatformAdapter, ProfileRuntime,
};

const SETTINGS_FILE_NAME: &str = "settings.json";
const CREDENTIALS_FILE_NAME: &str = ".credentials.json";
const MAX_SETTINGS_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CREDENTIAL_BYTES: u64 = 1024 * 1024;
#[cfg(any(target_os = "macos", windows, test))]
const KEYCHAIN_SERVICE_PREFIX: &str = "Claude Code-credentials";
const CREDENTIAL_STORAGE_KIND: &str = "claude_code_account_fields_v1";
const SECURE_STORAGE_CONFIG_ENV: &str = "CLAUDE_SECURESTORAGE_CONFIG_DIR";
const CUSTOM_OAUTH_URL_ENV: &str = "CLAUDE_CODE_CUSTOM_OAUTH_URL";
#[cfg(windows)]
const WINDOWS_CREDMAN_FORCE_ENV: &str = "CLAUDE_CODE_FORCE_WINDOWS_CREDMAN";
#[cfg(any(windows, test))]
const WINDOWS_CREDMAN_ACCOUNT: &str = "claude-code-user";
const WINDOWS_CREDMAN_CHUNK_BYTES: usize = 2400;
const WINDOWS_CREDMAN_MAX_CHUNKS: usize = 256;

/// Fields that select the active first-party Claude account/authentication.
/// Every other secureStorage field belongs to a different feature and must
/// remain live when the user switches Claude accounts.
const ACCOUNT_CREDENTIAL_FIELDS: &[&str] = &[
    "claudeAiOauth",
    "organizationUuid",
    "trustedDeviceToken",
    "enterpriseGateway",
];

/// These are the only Claude user-settings paths YAAT owns in a managed profile.
/// Everything else is preserved byte-for-byte by the CST patcher.
const OWNED_SETTINGS_PATHS: &[&str] = &[
    "/apiKeyHelper",
    "/env/ANTHROPIC_API_KEY",
    "/env/ANTHROPIC_AUTH_TOKEN",
    "/env/ANTHROPIC_BASE_URL",
    "/env/ANTHROPIC_MODEL",
    "/env/CLAUDE_CODE_OAUTH_TOKEN",
    "/env/CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "/env/CLAUDE_CODE_USE_BEDROCK",
    "/env/CLAUDE_CODE_USE_FOUNDRY",
    "/env/CLAUDE_CODE_USE_GATEWAY",
    "/env/CLAUDE_CODE_USE_MANTLE",
    "/env/CLAUDE_CODE_USE_VERTEX",
];

const OWNED_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_MODEL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_GATEWAY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_VERTEX",
];

/// Process-level provider selectors that must never leak from the shell into a managed
/// Claude process. Provider-specific values are supplied by the managed settings file.
const COMPETING_PROVIDER_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_BEDROCK_MANTLE_BASE_URL",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_BASE_URL",
    "ANTHROPIC_FOUNDRY_RESOURCE",
    "ANTHROPIC_VERTEX_BASE_URL",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "CLAUDE_CODE_CUSTOM_OAUTH_URL",
    "CLAUDE_CODE_HOST_AUTH_ENV_VAR",
    "CLAUDE_CODE_HOST_CREDS_FILE",
    "CLAUDE_CODE_OAUTH_CLIENT_ID",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_GATEWAY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_SECURESTORAGE_CONFIG_DIR",
    "USE_LOCAL_OAUTH",
    "USE_STAGING_OAUTH",
];

#[derive(Clone, Copy, Debug, Default)]
pub struct ClaudeAdapter;

#[derive(Debug)]
struct DesiredSettings {
    api_key_helper: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
}

impl ClaudeAdapter {
    pub const fn new() -> Self {
        Self
    }

    /// Verifies the active credential slot without returning its contents.
    pub fn verify_credential_readback(
        &self,
        context: &AdapterContext,
        config_root: &Path,
        expected_snapshot: &[u8],
    ) -> Result<bool, String> {
        let slot = resolve_credential_slot(context, config_root, &OsCredentialStore)?;
        let Some(payload) = slot.payload.as_deref() else {
            return Ok(false);
        };
        account_fields_match(payload, expected_snapshot)
    }

    fn desired_settings(
        &self,
        context: &AdapterContext,
        runtime: ProfileRuntime<'_>,
    ) -> Result<DesiredSettings, String> {
        ensure_claude_profile(runtime.profile)?;

        match runtime.profile.kind {
            ProviderKind::OfficialSubscription => {
                if runtime.secret_ref.is_some()
                    || runtime.profile.secret_kind != SecretKind::None
                    || runtime.profile.has_secret
                {
                    return Err(
                        "Claude subscription profiles must be authenticated by the isolated official CLI login"
                            .into(),
                    );
                }
                if runtime.profile.base_url.is_some() {
                    return Err("Claude subscription profiles cannot override the API URL".into());
                }
                Ok(DesiredSettings {
                    api_key_helper: None,
                    base_url: None,
                    model: runtime.profile.model.clone(),
                })
            }
            ProviderKind::OfficialApi | ProviderKind::ThirdParty => {
                if runtime.profile.secret_kind != SecretKind::ApiKey {
                    return Err(
                        "Claude managed API profiles require an API key; bearer-token gateways cannot be injected without exposing plaintext"
                            .into(),
                    );
                }
                let secret_ref = runtime.secret_ref.ok_or_else(|| {
                    "Claude API profile has no credential reference in the local database"
                        .to_string()
                })?;
                validate_credential_reference(secret_ref)?;
                let helper = credential_helper_command(&context.helper_executable, secret_ref)?;

                let (base_url, model) = match runtime.profile.kind {
                    ProviderKind::OfficialApi => {
                        if runtime.profile.base_url.is_some() {
                            return Err(
                                "Anthropic Console API profiles use the official endpoint".into()
                            );
                        }
                        (None, runtime.profile.model.clone())
                    }
                    ProviderKind::ThirdParty => {
                        let base_url = runtime.profile.base_url.as_deref().ok_or_else(|| {
                            "a third-party Claude Messages provider requires a base URL".to_string()
                        })?;
                        crate::validation::validate_provider_url(base_url)
                            .map_err(|error| error.to_string())?;
                        let model = runtime.profile.model.as_deref().ok_or_else(|| {
                            "a third-party Claude Messages provider requires a model identifier"
                                .to_string()
                        })?;
                        if model.trim().is_empty() || model.chars().any(char::is_control) {
                            return Err("Claude model identifier is invalid".into());
                        }
                        (Some(base_url.to_string()), Some(model.to_string()))
                    }
                    ProviderKind::OfficialSubscription => unreachable!(),
                };

                Ok(DesiredSettings {
                    api_key_helper: Some(helper),
                    base_url,
                    model,
                })
            }
        }
    }

    fn profile_root(
        &self,
        context: &AdapterContext,
        runtime: ProfileRuntime<'_>,
    ) -> Result<PathBuf, String> {
        ensure_claude_profile(runtime.profile)?;
        crate::paths::validate_identifier(&runtime.profile.id)
            .map_err(|error| error.to_string())?;
        Ok(context
            .app_data_dir
            .join("profiles")
            .join(Platform::ClaudeCode.as_str())
            .join(&runtime.profile.id)
            .join("home"))
    }

    fn source_config_root(&self, context: &AdapterContext) -> Result<PathBuf, String> {
        if let Some(root) = &context.explicit_config_root {
            return Ok(root.clone());
        }
        crate::paths::default_config_root(Platform::ClaudeCode).map_err(|error| error.to_string())
    }
}

pub(crate) fn ensure_global_credential_namespace() -> Result<(), String> {
    if std::env::var_os(CUSTOM_OAUTH_URL_ENV).is_some_and(|value| !value.is_empty()) {
        return Err(
            "Claude account switching supports the production OAuth credential namespace only; CLAUDE_CODE_CUSTOM_OAUTH_URL is active"
                .into(),
        );
    }
    Ok(())
}

impl PlatformAdapter for ClaudeAdapter {
    fn discover_cli(&self, context: &AdapterContext) -> Result<(PathBuf, String), String> {
        let program = match &context.explicit_cli_path {
            Some(path) => path.clone(),
            None => which::which("claude")
                .map_err(|_| "Claude Code CLI was not found on PATH".to_string())?,
        };

        let metadata = fs::metadata(&program)
            .map_err(|_| "configured Claude Code CLI path is not readable".to_string())?;
        if !metadata.is_file() {
            return Err("configured Claude Code CLI path is not a file".into());
        }

        let (status, stdout, stderr) =
            crate::process::run_with_timeout(&program, &["--version"], Duration::from_secs(3))?;
        if !status.success() {
            return Err("`claude --version` exited unsuccessfully".into());
        }
        let text = if stdout.is_empty() {
            String::from_utf8(stderr)
                .map_err(|_| "Claude Code version output is not UTF-8".to_string())?
        } else {
            String::from_utf8(stdout)
                .map_err(|_| "Claude Code version output is not UTF-8".to_string())?
        };
        let version = parse_cli_version(&text)
            .ok_or_else(|| "unable to parse Claude Code CLI version".to_string())?;
        Ok((program, version.to_string()))
    }

    fn prepare_profile(
        &self,
        context: &AdapterContext,
        runtime: ProfileRuntime<'_>,
    ) -> Result<PathBuf, String> {
        let desired = self.desired_settings(context, runtime.clone())?;
        let profile_root = self.profile_root(context, runtime)?;
        crate::paths::ensure_private_directory(&profile_root).map_err(|error| error.to_string())?;

        let target = profile_root.join(SETTINGS_FILE_NAME);
        let (raw, target_existed) = if target.exists() {
            (read_bounded_text(&target, MAX_SETTINGS_BYTES)?, true)
        } else {
            let source = self.source_config_root(context)?.join(SETTINGS_FILE_NAME);
            if source == target || !source.exists() {
                ("{}\n".to_string(), false)
            } else {
                (read_bounded_text(&source, MAX_SETTINGS_BYTES)?, false)
            }
        };

        let patched = patch_managed_settings(&raw, &desired)?;
        if !target_existed || patched != raw {
            write_private_atomic(&target, patched.as_bytes())?;
        }
        Ok(profile_root)
    }

    fn login_spec(
        &self,
        context: &AdapterContext,
        runtime: ProfileRuntime<'_>,
        console: bool,
    ) -> Result<CommandSpec, String> {
        if runtime.profile.kind == ProviderKind::ThirdParty {
            return Err("third-party Claude providers cannot run the official login flow".into());
        }
        let use_console = console || runtime.profile.kind == ProviderKind::OfficialApi;
        let config_root = self.prepare_profile(context, runtime)?;
        let (program, _) = self.discover_cli(context)?;
        managed_command_spec(
            program,
            vec![
                "auth".into(),
                "login".into(),
                if use_console {
                    "--console".into()
                } else {
                    "--claudeai".into()
                },
            ],
            config_root,
            None,
        )
    }

    fn launch_spec(
        &self,
        context: &AdapterContext,
        runtime: ProfileRuntime<'_>,
        cwd: Option<PathBuf>,
        passthrough_args: Vec<String>,
    ) -> Result<CommandSpec, String> {
        let config_root = self.prepare_profile(context, runtime)?;
        let (program, _) = self.discover_cli(context)?;
        managed_command_spec(program, passthrough_args, config_root, cwd)
    }

    fn capture_credentials(
        &self,
        context: &AdapterContext,
        config_root: &Path,
    ) -> Result<CredentialSnapshot, String> {
        let slot = resolve_credential_slot(context, config_root, &OsCredentialStore)?;
        let payload = slot
            .payload
            .as_deref()
            .ok_or_else(|| "the selected Claude credential slot is empty".to_string())?;
        let account_payload = extract_account_snapshot(payload)?;
        let account_label = account_label_from_snapshot(&account_payload)?;
        Ok(CredentialSnapshot {
            storage_kind: CREDENTIAL_STORAGE_KIND.into(),
            opaque_payload: account_payload,
            account_label,
            warning: None,
        })
    }

    fn capture_credential_state(
        &self,
        context: &AdapterContext,
        config_root: &Path,
    ) -> Result<CredentialState, String> {
        let slot = resolve_credential_slot(context, config_root, &OsCredentialStore)?;
        let Some(payload) = slot.payload.as_deref() else {
            return Ok(CredentialState::Absent);
        };
        let Some(account_payload) = extract_account_snapshot_optional(payload)? else {
            return Ok(CredentialState::Absent);
        };
        let account_label = account_label_from_snapshot(&account_payload)?;
        Ok(CredentialState::Present(CredentialSnapshot {
            storage_kind: CREDENTIAL_STORAGE_KIND.into(),
            opaque_payload: account_payload,
            account_label,
            warning: None,
        }))
    }

    fn restore_credentials(
        &self,
        context: &AdapterContext,
        config_root: &Path,
        snapshot: &CredentialSnapshot,
    ) -> Result<(), String> {
        validate_snapshot(snapshot)?;
        replace_account_fields(
            context,
            config_root,
            &OsCredentialStore,
            &snapshot.opaque_payload,
        )?;
        if self.verify_credential_readback(context, config_root, &snapshot.opaque_payload)? {
            Ok(())
        } else {
            Err("Claude account-field verification failed after credential replacement".into())
        }
    }

    fn restore_credential_state(
        &self,
        context: &AdapterContext,
        config_root: &Path,
        state: &CredentialState,
    ) -> Result<(), String> {
        match state {
            CredentialState::Present(snapshot) => {
                self.restore_credentials(context, config_root, snapshot)
            }
            CredentialState::Absent => {
                clear_account_fields(context, config_root, &OsCredentialStore)
            }
        }
    }

    fn global_config_plan(
        &self,
        context: &AdapterContext,
        runtime: ProfileRuntime<'_>,
    ) -> Result<GlobalConfigPlan, String> {
        let desired = self.desired_settings(context, runtime)?;
        let config_root = self.source_config_root(context)?;
        let metadata = fs::metadata(&config_root)
            .map_err(|error| format!("Claude config directory is unavailable: {error}"))?;
        if !metadata.is_dir() {
            return Err("Claude config path is not a directory".into());
        }

        let mut operations = Vec::with_capacity(OWNED_SETTINGS_PATHS.len());
        for pointer in OWNED_SETTINGS_PATHS {
            let path = OwnedPath::from_json_pointer(pointer).map_err(|error| error.to_string())?;
            let value = match *pointer {
                "/apiKeyHelper" => desired.api_key_helper.as_ref(),
                "/env/ANTHROPIC_BASE_URL" => desired.base_url.as_ref(),
                "/env/ANTHROPIC_MODEL" => desired.model.as_ref(),
                _ => None,
            };
            operations.push(match value {
                Some(value) => PatchOperation::set(path, value.clone()),
                None => PatchOperation::remove(path),
            });
        }

        Ok(GlobalConfigPlan {
            path: config_root.join(SETTINGS_FILE_NAME),
            format: ConfigFormat::Jsonc,
            operations,
        })
    }
}

fn ensure_claude_profile(profile: &yaat_contracts::ProviderProfile) -> Result<(), String> {
    if profile.platform != Platform::ClaudeCode {
        return Err("Claude adapter received a profile for another platform".into());
    }
    Ok(())
}

fn patch_managed_settings(raw: &str, desired: &DesiredSettings) -> Result<String, String> {
    let parse_input = if raw.trim().is_empty() { "{}" } else { raw };
    let root = CstRootNode::parse(parse_input, &ParseOptions::default())
        .map_err(|error| format!("managed Claude settings are malformed: {error}"))?;
    let object = root.object_value_or_create().ok_or_else(|| {
        "managed Claude settings are malformed: the root value must be an object".to_string()
    })?;

    patch_string_property(&object, "apiKeyHelper", desired.api_key_helper.as_deref());

    let needs_env = desired.base_url.is_some() || desired.model.is_some();
    let env = match object.get("env") {
        Some(prop) => Some(prop.object_value().ok_or_else(|| {
            "managed Claude settings are malformed: `env` must be an object".to_string()
        })?),
        None if needs_env => Some(
            object
                .object_value_or_create("env")
                .ok_or_else(|| "unable to create Claude settings `env` object".to_string())?,
        ),
        None => None,
    };

    if let Some(env) = env {
        for key in OWNED_ENV_KEYS {
            let desired_value = match *key {
                "ANTHROPIC_BASE_URL" => desired.base_url.as_deref(),
                "ANTHROPIC_MODEL" => desired.model.as_deref(),
                _ => None,
            };
            patch_string_property(&env, key, desired_value);
        }
    }

    let mut result = root.to_string();
    if raw.trim().is_empty() && raw.ends_with('\n') && !result.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
}

fn patch_string_property(object: &CstObject, key: &str, value: Option<&str>) {
    match (object.get(key), value) {
        (Some(prop), Some(value)) => prop.set_value(CstInputValue::String(value.to_string())),
        (Some(prop), None) => prop.remove(),
        (None, Some(value)) => {
            object.append(key, CstInputValue::String(value.to_string()));
        }
        (None, None) => {}
    }
}

fn credential_helper_command(helper: &Path, secret_ref: &str) -> Result<String, String> {
    if !helper.is_absolute() {
        return Err("credential helper executable must use an absolute path".into());
    }
    let helper = helper
        .to_str()
        .ok_or_else(|| "credential helper path is not valid UTF-8".to_string())?;
    let args = [
        helper,
        "--yaat-credential-helper",
        Platform::ClaudeCode.as_str(),
        secret_ref,
    ];
    Ok(args
        .into_iter()
        .map(|value| shell_escape::escape(Cow::Borrowed(value)).into_owned())
        .collect::<Vec<_>>()
        .join(" "))
}

fn validate_credential_reference(value: &str) -> Result<(), String> {
    crate::paths::validate_identifier(value).map_err(|_| {
        "credential reference must contain only ASCII letters, digits, '-' or '_'".to_string()
    })
}

fn managed_command_spec(
    program: PathBuf,
    args: Vec<String>,
    config_root: PathBuf,
    cwd: Option<PathBuf>,
) -> Result<CommandSpec, String> {
    let config_root_text = config_root
        .to_str()
        .ok_or_else(|| "Claude config directory is not valid UTF-8".to_string())?
        .to_string();
    let mut env = BTreeMap::new();
    env.insert("CLAUDE_CONFIG_DIR".into(), config_root_text);
    env.insert("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB".into(), "1".into());

    Ok(CommandSpec {
        program,
        args,
        env,
        env_remove: COMPETING_PROVIDER_ENV
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
        cwd,
    })
}

fn parse_cli_version(output: &str) -> Option<&str> {
    output.split_whitespace().find(|part| {
        let mut pieces = part.split('.');
        let major = pieces.next();
        let minor = pieces.next();
        let patch = pieces.next();
        pieces.next().is_none()
            && major
                .is_some_and(|piece| !piece.is_empty() && piece.bytes().all(|b| b.is_ascii_digit()))
            && minor
                .is_some_and(|piece| !piece.is_empty() && piece.bytes().all(|b| b.is_ascii_digit()))
            && patch
                .is_some_and(|piece| !piece.is_empty() && piece.bytes().all(|b| b.is_ascii_digit()))
    })
}

fn default_claude_config_root() -> Result<PathBuf, String> {
    BaseDirs::new()
        .map(|base| base.home_dir().join(".claude"))
        .ok_or_else(|| "unable to resolve the user home directory".to_string())
}

fn read_bounded_text(path: &Path, limit: u64) -> Result<String, String> {
    let metadata =
        fs::metadata(path).map_err(|_| format!("unable to inspect {}", path.display()))?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(format!(
            "{} has an unsupported size or type",
            path.display()
        ));
    }
    let bytes = fs::read(path).map_err(|_| format!("unable to read {}", path.display()))?;
    String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "managed file has no parent directory".to_string())?;
    crate::paths::ensure_private_directory(parent).map_err(|error| error.to_string())?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "managed file name is not valid UTF-8".to_string())?;
    let temp_path = parent.join(format!(
        ".{file_name}.yaat-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));

    let write_result = (|| -> Result<(), String> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp_path)
            .map_err(|_| "unable to create private temporary file".to_string())?;
        file.write_all(bytes)
            .map_err(|_| "unable to write private temporary file".to_string())?;
        file.sync_all()
            .map_err(|_| "unable to synchronize private temporary file".to_string())?;
        drop(file);
        atomic_replace(&temp_path, path)?;
        sync_parent_directory(parent)?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

#[cfg(not(windows))]
fn atomic_replace(temp_path: &Path, target_path: &Path) -> Result<(), String> {
    fs::rename(temp_path, target_path)
        .map_err(|_| "unable to atomically replace managed file".to_string())
}

#[cfg(windows)]
fn atomic_replace(temp_path: &Path, target_path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

    if !target_path.exists() {
        return fs::rename(temp_path, target_path)
            .map_err(|_| "unable to atomically install managed file".to_string());
    }
    let target: Vec<u16> = target_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let replacement: Vec<u16> = temp_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: Both path buffers are owned, NUL-terminated UTF-16 strings that
    // outlive the call. Null backup/exclusion pointers are allowed by the API.
    let replaced = unsafe {
        ReplaceFileW(
            target.as_ptr(),
            replacement.as_ptr(),
            ptr::null(),
            REPLACEFILE_WRITE_THROUGH,
            ptr::null(),
            ptr::null(),
        )
    };
    if replaced == 0 {
        Err("unable to atomically replace managed file".into())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<(), String> {
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "unable to synchronize managed directory".to_string())
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<(), String> {
    Ok(())
}

fn validate_snapshot(snapshot: &CredentialSnapshot) -> Result<(), String> {
    if snapshot.storage_kind != CREDENTIAL_STORAGE_KIND {
        return Err("Claude credential snapshot uses an incompatible storage kind".into());
    }
    if snapshot.opaque_payload.is_empty()
        || snapshot.opaque_payload.len() as u64 > MAX_CREDENTIAL_BYTES
    {
        return Err("Claude credential snapshot has an unsupported size".into());
    }
    validate_account_snapshot(&snapshot.opaque_payload)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CredentialKey {
    service: String,
    account: String,
    target: Option<String>,
}

impl CredentialKey {
    fn new(service: String, account: String, windows_target: bool) -> Self {
        let target = windows_target.then(|| format!("{service}/{account}"));
        Self {
            service,
            account,
            target,
        }
    }

    fn child(&self, suffix: &str) -> Self {
        Self::new(
            self.service.clone(),
            format!("{}{suffix}", self.account),
            self.target.is_some(),
        )
    }
}

trait CredentialStoreAccess: Send + Sync {
    fn load(&self, key: &CredentialKey) -> Result<Option<Zeroizing<Vec<u8>>>, String>;
    fn save(&self, key: &CredentialKey, value: &[u8]) -> Result<(), String>;
    fn delete(&self, key: &CredentialKey) -> Result<bool, String>;
}

struct OsCredentialStore;

impl OsCredentialStore {
    fn entry(key: &CredentialKey) -> Result<keyring::Entry, String> {
        #[cfg(windows)]
        {
            let target = key.target.as_deref().ok_or_else(|| {
                "Claude Credential Manager entry is missing its exact target".to_string()
            })?;
            let modifiers = HashMap::from([("target", target)]);
            let inner =
                keyring_core::Entry::new_with_modifiers(&key.service, &key.account, &modifiers)
                    .map_err(|error| format!("unable to open the OS credential entry: {error}"))?;
            return Ok(keyring::Entry { inner });
        }
        #[cfg(not(windows))]
        {
            keyring::Entry::new(&key.service, &key.account)
                .map_err(|error| format!("unable to open the OS credential entry: {error}"))
        }
    }
}

impl CredentialStoreAccess for OsCredentialStore {
    fn load(&self, key: &CredentialKey) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
        match Self::entry(key)?.get_secret() {
            Ok(value) => {
                if value.len() as u64 > MAX_CREDENTIAL_BYTES {
                    return Err("Claude OS credential entry exceeds the supported size".into());
                }
                Ok(Some(Zeroizing::new(value)))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(format!(
                "unable to read the Claude OS credential entry: {error}"
            )),
        }
    }

    fn save(&self, key: &CredentialKey, value: &[u8]) -> Result<(), String> {
        if value.is_empty() || value.len() as u64 > MAX_CREDENTIAL_BYTES {
            return Err("Claude OS credential value has an unsupported size".into());
        }
        Self::entry(key)?
            .set_secret(value)
            .map_err(|error| format!("unable to write the Claude OS credential entry: {error}"))
    }

    fn delete(&self, key: &CredentialKey) -> Result<bool, String> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(error) => Err(format!(
                "unable to delete the Claude OS credential entry: {error}"
            )),
        }
    }
}

#[derive(Clone, Debug)]
enum CredentialBackend {
    File(PathBuf),
    #[allow(
        dead_code,
        reason = "the keyring backend is selected only on macOS and feature-enabled Windows builds"
    )]
    Keyring {
        key: CredentialKey,
        windows_chunk_count: Option<usize>,
    },
}

struct ResolvedCredentialSlot {
    backend: CredentialBackend,
    payload: Option<Vec<u8>>,
    fallback_path: PathBuf,
    fallback_payload: Option<Vec<u8>>,
}

impl Drop for ResolvedCredentialSlot {
    fn drop(&mut self) {
        if let Some(payload) = &mut self.payload {
            payload.zeroize();
        }
        if let Some(payload) = &mut self.fallback_payload {
            payload.zeroize();
        }
    }
}

struct SecureStorageScope {
    root: PathBuf,
    #[allow(
        dead_code,
        reason = "unscoped storage affects only macOS and Windows keyring naming"
    )]
    unscoped: bool,
}

fn effective_secure_storage_scope(
    context: &AdapterContext,
    config_root: &Path,
) -> Result<SecureStorageScope, String> {
    let default_root = default_claude_config_root()?;
    let source_root = match context.explicit_config_root.as_deref() {
        Some(root) => root.to_path_buf(),
        None => crate::paths::default_config_root(Platform::ClaudeCode)
            .map_err(|error| error.to_string())?,
    };
    let is_source_slot = config_root == source_root;

    if is_source_slot && let Some(override_root) = std::env::var_os(SECURE_STORAGE_CONFIG_ENV) {
        if override_root.is_empty() {
            return Ok(SecureStorageScope {
                root: default_root,
                unscoped: true,
            });
        }
        return Ok(SecureStorageScope {
            root: PathBuf::from(override_root),
            unscoped: false,
        });
    }

    Ok(SecureStorageScope {
        root: config_root.to_path_buf(),
        unscoped: context.explicit_config_root.is_none() && config_root == default_root,
    })
}

#[cfg(any(target_os = "macos", windows, test))]
fn keychain_service_name(scope: &SecureStorageScope) -> Result<String, String> {
    if scope.unscoped {
        return Ok(KEYCHAIN_SERVICE_PREFIX.into());
    }
    let root = scope
        .root
        .to_str()
        .ok_or_else(|| "Claude secure-storage directory is not valid UTF-8".to_string())?;
    let normalized = root.nfc().collect::<String>();
    let hash = Sha256::digest(normalized.as_bytes());
    Ok(format!(
        "{KEYCHAIN_SERVICE_PREFIX}-{}",
        &hex::encode(hash)[..8]
    ))
}

fn resolve_credential_slot(
    context: &AdapterContext,
    config_root: &Path,
    _store: &dyn CredentialStoreAccess,
) -> Result<ResolvedCredentialSlot, String> {
    let scope = effective_secure_storage_scope(context, config_root)?;
    let fallback_path = scope.root.join(CREDENTIALS_FILE_NAME);
    let fallback_payload = read_optional_credential_file(&fallback_path)?;

    #[cfg(any(target_os = "macos", windows))]
    let mut fallback_payload = fallback_payload;

    #[cfg(target_os = "macos")]
    {
        let key = CredentialKey::new(
            keychain_service_name(&scope)?,
            macos_keychain_account(),
            false,
        );
        let primary = match _store.load(&key) {
            Ok(primary) => primary,
            Err(error) => {
                if let Some(payload) = &mut fallback_payload {
                    payload.zeroize();
                }
                return Err(error);
            }
        };
        if let Some(primary) = primary {
            return Ok(ResolvedCredentialSlot {
                backend: CredentialBackend::Keyring {
                    key,
                    windows_chunk_count: None,
                },
                payload: Some(primary.to_vec()),
                fallback_path,
                fallback_payload,
            });
        }
        if fallback_payload.is_some() {
            return Ok(ResolvedCredentialSlot {
                backend: CredentialBackend::File(fallback_path.clone()),
                payload: fallback_payload.clone(),
                fallback_path,
                fallback_payload,
            });
        }
        Ok(ResolvedCredentialSlot {
            backend: CredentialBackend::Keyring {
                key,
                windows_chunk_count: None,
            },
            payload: None,
            fallback_path,
            fallback_payload: None,
        })
    }

    #[cfg(windows)]
    {
        if windows_credential_manager_enabled(&scope.root)? {
            let key = CredentialKey::new(
                keychain_service_name(&scope)?,
                WINDOWS_CREDMAN_ACCOUNT.into(),
                true,
            );
            let primary = match read_windows_credential(_store, &key) {
                Ok(primary) => primary,
                Err(error) => {
                    if let Some(payload) = &mut fallback_payload {
                        payload.zeroize();
                    }
                    return Err(error);
                }
            };
            if let Some(primary) = primary.payload {
                return Ok(ResolvedCredentialSlot {
                    backend: CredentialBackend::Keyring {
                        key,
                        windows_chunk_count: primary.chunk_count,
                    },
                    payload: Some(primary.value),
                    fallback_path,
                    fallback_payload,
                });
            }
            if fallback_payload.is_some() {
                return Ok(ResolvedCredentialSlot {
                    backend: CredentialBackend::File(fallback_path.clone()),
                    payload: fallback_payload.clone(),
                    fallback_path,
                    fallback_payload,
                });
            }
            return Ok(ResolvedCredentialSlot {
                backend: CredentialBackend::Keyring {
                    key,
                    windows_chunk_count: None,
                },
                payload: None,
                fallback_path,
                fallback_payload: None,
            });
        }
    }

    #[cfg(not(target_os = "macos"))]
    Ok(ResolvedCredentialSlot {
        backend: CredentialBackend::File(fallback_path.clone()),
        payload: fallback_payload.clone(),
        fallback_path,
        fallback_payload,
    })
}

fn extract_account_snapshot(payload: &[u8]) -> Result<Vec<u8>, String> {
    extract_account_snapshot_optional(payload)?.ok_or_else(|| {
        "Claude secure-storage document contains no supported first-party authentication".into()
    })
}

fn extract_account_snapshot_optional(payload: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let document = parse_sensitive_object(payload, "Claude secure-storage document")?;
    let mut snapshot = Map::new();
    for field in ACCOUNT_CREDENTIAL_FIELDS {
        if let Some(value) = document.object().get(*field) {
            snapshot.insert((*field).to_string(), value.clone());
        }
    }
    let snapshot = SensitiveJson(Value::Object(snapshot));
    validate_account_object(snapshot.object(), false)?;
    if snapshot.object().get("claudeAiOauth").is_none()
        && snapshot.object().get("enterpriseGateway").is_none()
    {
        return Ok(None);
    }
    validate_account_object(snapshot.object(), true)?;
    serialize_sensitive_json(&snapshot, "Claude account snapshot").map(Some)
}

fn validate_account_snapshot(payload: &[u8]) -> Result<(), String> {
    let snapshot = parse_sensitive_object(payload, "Claude account snapshot")?;
    validate_account_object(snapshot.object(), true)
}

fn validate_account_object(object: &Map<String, Value>, require_auth: bool) -> Result<(), String> {
    if object
        .keys()
        .any(|key| !ACCOUNT_CREDENTIAL_FIELDS.contains(&key.as_str()))
    {
        return Err("Claude account snapshot contains a non-account secure-storage field".into());
    }

    let oauth = object.get("claudeAiOauth");
    let gateway = object.get("enterpriseGateway");
    if oauth.is_some() && gateway.is_some() {
        return Err("Claude account snapshot contains conflicting authentication modes".into());
    }
    if require_auth && oauth.is_none() && gateway.is_none() {
        return Err(
            "Claude account snapshot contains no supported first-party authentication".into(),
        );
    }
    if let Some(oauth) = oauth {
        let oauth = oauth
            .as_object()
            .ok_or_else(|| "Claude OAuth credential must be a JSON object".to_string())?;
        for field in ["accessToken", "refreshToken"] {
            if oauth
                .get(field)
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return Err(format!("Claude OAuth credential is missing `{field}`"));
            }
        }
    }
    if let Some(gateway) = gateway {
        let gateway = gateway.as_object().ok_or_else(|| {
            "Claude enterprise gateway credential must be a JSON object".to_string()
        })?;
        for field in ["url", "jwt"] {
            if gateway
                .get(field)
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return Err(format!(
                    "Claude enterprise gateway credential is missing `{field}`"
                ));
            }
        }
    }
    Ok(())
}

fn account_label_from_snapshot(payload: &[u8]) -> Result<Option<String>, String> {
    let snapshot = parse_sensitive_object(payload, "Claude account snapshot")?;
    let oauth = snapshot
        .object()
        .get("claudeAiOauth")
        .and_then(Value::as_object);
    let label = oauth
        .and_then(|oauth| {
            oauth
                .get("tokenAccount")
                .and_then(Value::as_object)
                .and_then(|account| account.get("emailAddress"))
                .and_then(Value::as_str)
                .or_else(|| oauth.get("emailAddress").and_then(Value::as_str))
        })
        .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
        .map(str::to_string);
    Ok(label)
}

fn replace_account_fields(
    context: &AdapterContext,
    config_root: &Path,
    store: &dyn CredentialStoreAccess,
    target_snapshot: &[u8],
) -> Result<(), String> {
    validate_account_snapshot(target_snapshot)?;
    mutate_account_fields(context, config_root, store, target_snapshot)
}

fn mutate_account_fields(
    context: &AdapterContext,
    config_root: &Path,
    store: &dyn CredentialStoreAccess,
    target_snapshot: &[u8],
) -> Result<(), String> {
    let slot = resolve_credential_slot(context, config_root, store)?;
    let replacement = merge_account_fields(
        slot.payload.as_deref().unwrap_or(b"{}"),
        target_snapshot,
        true,
    )?;
    write_resolved_slot(store, &slot, &replacement, target_snapshot)
}

fn clear_account_fields(
    context: &AdapterContext,
    config_root: &Path,
    store: &dyn CredentialStoreAccess,
) -> Result<(), String> {
    let slot = resolve_credential_slot(context, config_root, store)?;
    let Some(current) = slot.payload.as_deref() else {
        return Ok(());
    };
    let empty = b"{}";
    if account_fields_match(current, empty)? {
        return Ok(());
    }
    let replacement = merge_account_fields(current, empty, false)?;
    if parse_sensitive_object(&replacement, "cleared Claude credential")?
        .object()
        .is_empty()
    {
        return remove_resolved_slot(store, &slot);
    }
    write_resolved_slot(store, &slot, &replacement, empty)
}

fn remove_resolved_slot(
    store: &dyn CredentialStoreAccess,
    slot: &ResolvedCredentialSlot,
) -> Result<(), String> {
    match &slot.backend {
        CredentialBackend::File(path) => {
            remove_atomically(path)
                .map_err(|error| format!("failed to remove Claude credential file: {error}"))?;
            if read_optional_credential_file(path)?.is_none() {
                return Ok(());
            }
            let rollback = slot.payload.as_deref().map_or(Ok(()), |previous| {
                replace_atomically(path, previous)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            });
            Err(format!(
                "Claude credential file remained after removal; rollback: {}",
                result_detail(&rollback)
            ))
        }
        CredentialBackend::Keyring {
            key,
            windows_chunk_count,
        } => {
            if key.target.is_some() {
                return remove_windows_credential_manager(
                    store,
                    key,
                    *windows_chunk_count,
                    slot.payload.as_deref(),
                );
            }
            store.delete(key)?;
            if store.load(key)?.is_none() {
                return Ok(());
            }
            let rollback = slot
                .payload
                .as_deref()
                .map_or(Ok(()), |previous| store.save(key, previous));
            Err(format!(
                "Claude OS credential remained after removal; rollback: {}",
                result_detail(&rollback)
            ))
        }
    }
}

fn write_resolved_slot(
    store: &dyn CredentialStoreAccess,
    slot: &ResolvedCredentialSlot,
    replacement: &[u8],
    target_snapshot: &[u8],
) -> Result<(), String> {
    match &slot.backend {
        CredentialBackend::File(path) => {
            replace_credential_file(path, slot.payload.as_deref(), replacement, target_snapshot)
        }
        CredentialBackend::Keyring {
            key,
            windows_chunk_count,
        } => replace_credential_keyring(
            store,
            slot,
            key,
            *windows_chunk_count,
            replacement,
            target_snapshot,
        ),
    }
}

fn merge_account_fields(
    current: &[u8],
    target: &[u8],
    require_target_auth: bool,
) -> Result<Vec<u8>, String> {
    let mut current = parse_sensitive_object(current, "current Claude secure-storage document")?;
    let target = parse_sensitive_object(target, "target Claude account snapshot")?;
    validate_account_object(target.object(), require_target_auth)?;

    for field in ACCOUNT_CREDENTIAL_FIELDS {
        current.object_mut().remove(*field);
        if let Some(value) = target.object().get(*field) {
            current
                .object_mut()
                .insert((*field).to_string(), value.clone());
        }
    }
    serialize_sensitive_json(&current, "merged Claude secure-storage document")
}

fn account_fields_match(current: &[u8], snapshot: &[u8]) -> Result<bool, String> {
    let current = parse_sensitive_object(current, "current Claude secure-storage document")?;
    let snapshot = parse_sensitive_object(snapshot, "Claude account snapshot")?;
    validate_account_object(snapshot.object(), false)?;
    Ok(ACCOUNT_CREDENTIAL_FIELDS
        .iter()
        .all(|field| current.object().get(*field) == snapshot.object().get(*field)))
}

fn replace_credential_file(
    path: &Path,
    previous: Option<&[u8]>,
    replacement: &[u8],
    target_snapshot: &[u8],
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Claude credential file has no parent directory".to_string())?;
    crate::paths::ensure_private_directory(parent).map_err(|error| error.to_string())?;
    replace_atomically(path, replacement)
        .map_err(|error| format!("failed to replace Claude credential file: {error}"))?;

    let verified = read_optional_credential_file(path)
        .and_then(|current| {
            current.ok_or_else(|| "Claude credential file disappeared after replacement".into())
        })
        .and_then(|mut current| {
            let verified = account_fields_match(&current, target_snapshot);
            current.zeroize();
            verified
        });
    if matches!(verified, Ok(true)) {
        return Ok(());
    }

    let rollback = rollback_credential_file(path, previous);
    Err(format!(
        "Claude credential-file readback verification failed; rollback: {}",
        result_detail(&rollback)
    ))
}

fn replace_credential_keyring(
    store: &dyn CredentialStoreAccess,
    slot: &ResolvedCredentialSlot,
    key: &CredentialKey,
    windows_chunk_count: Option<usize>,
    replacement: &[u8],
    target_snapshot: &[u8],
) -> Result<(), String> {
    if key.target.is_some() {
        return replace_windows_credential_manager(
            store,
            slot,
            key,
            windows_chunk_count,
            replacement,
            target_snapshot,
        );
    }

    if let Err(error) = store.save(key, replacement) {
        let rollback =
            rollback_keyring_account_fields(store, key, target_snapshot, slot.payload.as_deref());
        return Err(format!(
            "failed to write Claude OS credential entry before verification: {error}; rollback: {}",
            result_detail(&rollback)
        ));
    }

    let verified = store.load(key).and_then(|current| {
        current
            .as_deref()
            .ok_or_else(|| "Claude OS credential disappeared after replacement".into())
            .and_then(|current| account_fields_match(current, target_snapshot))
    });
    if !matches!(verified, Ok(true)) {
        let rollback =
            rollback_keyring_account_fields(store, key, target_snapshot, slot.payload.as_deref());
        return Err(format!(
            "Claude OS credential readback verification failed: {}; rollback: {}",
            verified
                .err()
                .unwrap_or_else(|| "account fields did not match".into()),
            result_detail(&rollback)
        ));
    }

    if let Some(fallback) = slot.fallback_payload.as_deref() {
        match remove_atomically(&slot.fallback_path) {
            Ok(()) => {}
            Err(error) => {
                let keyring_rollback = rollback_keyring_account_fields(
                    store,
                    key,
                    target_snapshot,
                    slot.payload.as_deref(),
                );
                let file_rollback = restore_missing_file(&slot.fallback_path, fallback);
                return Err(format!(
                    "Claude plaintext fallback cleanup failed: {error}; keyring rollback: {}; fallback rollback: {}",
                    result_detail(&keyring_rollback),
                    result_detail(&file_rollback)
                ));
            }
        }
    }
    Ok(())
}

fn replace_windows_credential_manager(
    store: &dyn CredentialStoreAccess,
    slot: &ResolvedCredentialSlot,
    key: &CredentialKey,
    previous_chunk_count: Option<usize>,
    replacement: &[u8],
    target_snapshot: &[u8],
) -> Result<(), String> {
    let replacement_chunk_count = windows_chunk_count(replacement, false)?;
    let stale_chunk_count = previous_chunk_count
        .unwrap_or_default()
        .max(replacement_chunk_count.unwrap_or_default());

    if let Err(error) = write_windows_credential(
        store,
        key,
        replacement,
        stale_chunk_count,
        replacement_chunk_count.is_some(),
    ) {
        let rollback = rollback_windows_credential(
            store,
            key,
            slot.payload.as_deref(),
            previous_chunk_count,
            stale_chunk_count,
        );
        return Err(format!(
            "failed to write Claude Windows Credential Manager chunks: {error}; rollback: {}",
            windows_rollback_detail(&rollback)
        ));
    }

    let verified = read_windows_credential(store, key).and_then(|current| {
        current
            .payload
            .ok_or_else(|| {
                "Claude Windows Credential Manager value disappeared after replacement".into()
            })
            .and_then(|current| {
                if current.chunk_count != replacement_chunk_count {
                    return Err(
                        "Claude Windows Credential Manager layout did not match the published metadata"
                            .into(),
                    );
                }
                account_fields_match(&current.value, target_snapshot)
            })
    });
    if !matches!(verified, Ok(true)) {
        let rollback = rollback_windows_credential(
            store,
            key,
            slot.payload.as_deref(),
            previous_chunk_count,
            stale_chunk_count,
        );
        return Err(format!(
            "Claude Windows Credential Manager readback verification failed: {}; rollback: {}",
            verified
                .err()
                .unwrap_or_else(|| "account fields did not match".into()),
            windows_rollback_detail(&rollback)
        ));
    }

    if let Some(fallback) = slot.fallback_payload.as_deref()
        && let Err(error) = remove_atomically(&slot.fallback_path)
    {
        let keyring_rollback = rollback_windows_credential(
            store,
            key,
            slot.payload.as_deref(),
            previous_chunk_count,
            stale_chunk_count,
        );
        let file_rollback = restore_missing_file(&slot.fallback_path, fallback);
        return Err(format!(
            "Claude plaintext fallback cleanup failed: {error}; Credential Manager rollback: {}; fallback rollback: {}",
            windows_rollback_detail(&keyring_rollback),
            result_detail(&file_rollback)
        ));
    }
    Ok(())
}

fn remove_windows_credential_manager(
    store: &dyn CredentialStoreAccess,
    key: &CredentialKey,
    previous_chunk_count: Option<usize>,
    previous: Option<&[u8]>,
) -> Result<(), String> {
    let stale_chunk_count = previous_chunk_count.unwrap_or_default();
    if let Err(error) = remove_windows_credential(store, key, stale_chunk_count) {
        let rollback = rollback_windows_credential(
            store,
            key,
            previous,
            previous_chunk_count,
            stale_chunk_count,
        );
        return Err(format!(
            "failed to remove Claude Windows Credential Manager value: {error}; rollback: {}",
            windows_rollback_detail(&rollback)
        ));
    }
    match read_windows_credential(store, key) {
        Ok(WindowsCredentialRead { payload: None }) => Ok(()),
        Ok(_) => {
            let rollback = rollback_windows_credential(
                store,
                key,
                previous,
                previous_chunk_count,
                stale_chunk_count,
            );
            Err(format!(
                "Claude Windows Credential Manager value remained after removal; rollback: {}",
                windows_rollback_detail(&rollback)
            ))
        }
        Err(error) => {
            let rollback = rollback_windows_credential(
                store,
                key,
                previous,
                previous_chunk_count,
                stale_chunk_count,
            );
            Err(format!(
                "Claude Windows Credential Manager removal verification failed: {error}; rollback: {}",
                windows_rollback_detail(&rollback)
            ))
        }
    }
}

fn windows_rollback_detail(result: &Result<(), String>) -> String {
    match result {
        Ok(()) => "verified".into(),
        Err(error) => format!("failed ({error})"),
    }
}

fn rollback_windows_credential(
    store: &dyn CredentialStoreAccess,
    key: &CredentialKey,
    previous: Option<&[u8]>,
    previous_chunk_count: Option<usize>,
    stale_chunk_count: usize,
) -> Result<(), String> {
    match previous {
        Some(previous) => write_windows_credential(
            store,
            key,
            previous,
            stale_chunk_count,
            previous_chunk_count.is_some(),
        )?,
        None => remove_windows_credential(store, key, stale_chunk_count)?,
    }

    let restored = read_windows_credential(store, key)?;
    if restored
        .payload
        .as_ref()
        .map(|payload| payload.value.as_slice())
        == previous
    {
        return Ok(());
    }
    Err("Windows Credential Manager rollback could not be verified".into())
}

/// Bun stores values larger than one Credential Manager blob as base64 chunks.
/// `#p` makes readers reject an in-progress layout, `#m` publishes the chunk
/// count and decoded length, and `#0..` contain the encoded payload. Windows
/// has no transaction across those entries, so callers must verify and roll
/// back the logical value when any step fails.
fn write_windows_credential(
    store: &dyn CredentialStoreAccess,
    key: &CredentialKey,
    value: &[u8],
    stale_chunk_count: usize,
    force_chunked: bool,
) -> Result<(), String> {
    let chunk_count = windows_chunk_count(value, force_chunked)?;
    let metadata = windows_chunk_metadata(value.len(), chunk_count.unwrap_or_default());
    let pending = key.child("#p");
    store
        .save(&pending, &metadata)
        .map_err(|error| format!("unable to create Bun's pending marker: {error}"))?;

    match chunk_count {
        Some(chunk_count) => {
            let encoded = Zeroizing::new(
                base64::engine::general_purpose::STANDARD
                    .encode(value)
                    .into_bytes(),
            );
            for (index, chunk) in encoded.chunks(WINDOWS_CREDMAN_CHUNK_BYTES).enumerate() {
                store
                    .save(&key.child(&format!("#{index}")), chunk)
                    .map_err(|error| format!("unable to write Bun chunk #{index}: {error}"))?;
            }
            store
                .save(&key.child("#m"), &metadata)
                .map_err(|error| format!("unable to publish Bun chunk metadata: {error}"))?;
            store
                .delete(key)
                .map_err(|error| format!("unable to remove the old direct value: {error}"))?;
            remove_stale_windows_chunks(store, key, chunk_count, stale_chunk_count)?;
        }
        None => {
            store
                .save(key, value)
                .map_err(|error| format!("unable to write the direct value: {error}"))?;
            store
                .delete(&key.child("#m"))
                .map_err(|error| format!("unable to remove old Bun chunk metadata: {error}"))?;
            remove_stale_windows_chunks(store, key, 0, stale_chunk_count)?;
        }
    }

    match store.delete(&pending) {
        Ok(true) => Ok(()),
        Ok(false) => Err("Bun's pending marker disappeared before commit completed".into()),
        Err(error) => Err(format!("unable to clear Bun's pending marker: {error}")),
    }
}

fn remove_windows_credential(
    store: &dyn CredentialStoreAccess,
    key: &CredentialKey,
    stale_chunk_count: usize,
) -> Result<(), String> {
    let pending = key.child("#p");
    let metadata = windows_chunk_metadata(0, 0);
    store
        .save(&pending, &metadata)
        .map_err(|error| format!("unable to create Bun's pending marker: {error}"))?;
    store
        .delete(key)
        .map_err(|error| format!("unable to remove the direct value: {error}"))?;
    store
        .delete(&key.child("#m"))
        .map_err(|error| format!("unable to remove Bun chunk metadata: {error}"))?;
    remove_stale_windows_chunks(store, key, 0, stale_chunk_count)?;
    match store.delete(&pending) {
        Ok(true) => Ok(()),
        Ok(false) => Err("Bun's pending marker disappeared before removal completed".into()),
        Err(error) => Err(format!("unable to clear Bun's pending marker: {error}")),
    }
}

fn windows_chunk_count(value: &[u8], force_chunked: bool) -> Result<Option<usize>, String> {
    if !force_chunked && value.len() <= WINDOWS_CREDMAN_CHUNK_BYTES {
        return Ok(None);
    }
    let encoded_len = value.len().div_ceil(3).saturating_mul(4);
    let chunk_count = encoded_len.div_ceil(WINDOWS_CREDMAN_CHUNK_BYTES);
    if !(1..=WINDOWS_CREDMAN_MAX_CHUNKS).contains(&chunk_count) {
        return Err(format!(
            "Claude credential requires {chunk_count} Bun chunks; at most {WINDOWS_CREDMAN_MAX_CHUNKS} are supported"
        ));
    }
    Ok(Some(chunk_count))
}

fn windows_chunk_metadata(decoded_len: usize, chunk_count: usize) -> Vec<u8> {
    format!(r#"{{"n":{chunk_count},"l":{decoded_len}}}"#).into_bytes()
}

fn remove_stale_windows_chunks(
    store: &dyn CredentialStoreAccess,
    key: &CredentialKey,
    keep: usize,
    stale_chunk_count: usize,
) -> Result<(), String> {
    for index in keep..stale_chunk_count {
        store
            .delete(&key.child(&format!("#{index}")))
            .map_err(|error| format!("unable to remove stale Bun chunk #{index}: {error}"))?;
    }
    Ok(())
}

fn rollback_keyring_account_fields(
    store: &dyn CredentialStoreAccess,
    key: &CredentialKey,
    _applied_snapshot: &[u8],
    previous: Option<&[u8]>,
) -> Result<(), String> {
    match previous {
        Some(previous) => store.save(key, previous)?,
        None => {
            store.delete(key)?;
        }
    }
    let restored = store.load(key)?;
    if restored.as_deref().map(Vec::as_slice) == previous {
        return Ok(());
    }
    Err("OS credential rollback could not be verified".into())
}

fn read_optional_credential_file(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("unable to inspect the Claude credential file".into()),
    };
    if !metadata.is_file() || metadata.len() > MAX_CREDENTIAL_BYTES {
        return Err("Claude credential file has an unsupported size or type".into());
    }

    let file = File::open(path).map_err(|_| "unable to open the Claude credential file")?;
    let opened = file
        .metadata()
        .map_err(|_| "unable to inspect the open Claude credential file")?;
    if !opened.is_file() || opened.len() > MAX_CREDENTIAL_BYTES {
        return Err("Claude credential file changed while it was being opened".into());
    }
    let capacity = usize::try_from(opened.len())
        .map_err(|_| "Claude credential file is too large for this platform")?;
    let mut payload = Vec::with_capacity(capacity);
    file.take(MAX_CREDENTIAL_BYTES + 1)
        .read_to_end(&mut payload)
        .map_err(|_| "unable to read the Claude credential file")?;
    if payload.len() as u64 > MAX_CREDENTIAL_BYTES {
        payload.zeroize();
        return Err("Claude credential file grew beyond the supported limit".into());
    }
    if payload.is_empty() {
        return Err("Claude credential file is empty".into());
    }
    Ok(Some(payload))
}

fn rollback_credential_file(path: &Path, previous: Option<&[u8]>) -> Result<(), String> {
    previous.map_or_else(
        || remove_atomically(path).map_err(|error| error.to_string()),
        |previous| {
            replace_atomically(path, previous)
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
    )
}

fn restore_missing_file(path: &Path, previous: &[u8]) -> Result<(), String> {
    replace_atomically(path, previous)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn result_detail(result: &Result<(), String>) -> &str {
    match result {
        Ok(()) => "verified",
        Err(_) => "failed",
    }
}

struct SensitiveJson(Value);

impl SensitiveJson {
    fn object(&self) -> &Map<String, Value> {
        self.0
            .as_object()
            .expect("SensitiveJson was validated as an object")
    }

    fn object_mut(&mut self) -> &mut Map<String, Value> {
        self.0
            .as_object_mut()
            .expect("SensitiveJson was validated as an object")
    }
}

impl Drop for SensitiveJson {
    fn drop(&mut self) {
        zeroize_json_value(&mut self.0);
    }
}

fn zeroize_json_value(value: &mut Value) {
    match value {
        Value::String(value) => value.zeroize(),
        Value::Array(values) => values.iter_mut().for_each(zeroize_json_value),
        Value::Object(values) => {
            values.values_mut().for_each(zeroize_json_value);
            for (mut key, _) in std::mem::take(values) {
                key.zeroize();
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn parse_sensitive_object(payload: &[u8], label: &str) -> Result<SensitiveJson, String> {
    if payload.is_empty() || payload.len() as u64 > MAX_CREDENTIAL_BYTES {
        return Err(format!("{label} has an unsupported size"));
    }
    let value: Value = serde_json::from_slice(payload)
        .map_err(|error| format!("{label} is not valid JSON: {error}"))?;
    if !value.is_object() {
        return Err(format!("{label} must be a JSON object"));
    }
    Ok(SensitiveJson(value))
}

fn serialize_sensitive_json(value: &SensitiveJson, label: &str) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&value.0).map_err(|error| format!("failed to serialize {label}: {error}"))
}

struct WindowsCredentialPayload {
    value: Vec<u8>,
    chunk_count: Option<usize>,
}

struct WindowsCredentialRead {
    payload: Option<WindowsCredentialPayload>,
}

fn read_windows_credential(
    store: &dyn CredentialStoreAccess,
    key: &CredentialKey,
) -> Result<WindowsCredentialRead, String> {
    if store.load(&key.child("#p"))?.is_some() {
        return Err("Claude Windows Credential Manager contains an incomplete Bun write".into());
    }
    let Some(meta) = store.load(&key.child("#m"))? else {
        let direct = store.load(key)?;
        if direct
            .as_ref()
            .is_some_and(|value| value.len() > WINDOWS_CREDMAN_CHUNK_BYTES)
        {
            return Err("Claude direct Credential Manager value exceeds Bun's size limit".into());
        }
        return Ok(WindowsCredentialRead {
            payload: direct.map(|value| WindowsCredentialPayload {
                value: value.to_vec(),
                chunk_count: None,
            }),
        });
    };

    let meta: Value = serde_json::from_slice(&meta)
        .map_err(|_| "Claude Credential Manager chunk metadata is malformed".to_string())?;
    let count = meta
        .get("n")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| (1..=WINDOWS_CREDMAN_MAX_CHUNKS).contains(value))
        .ok_or_else(|| "Claude Credential Manager chunk count is invalid".to_string())?;
    let expected_len = meta
        .get("l")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value <= MAX_CREDENTIAL_BYTES as usize)
        .ok_or_else(|| "Claude Credential Manager decoded length is invalid".to_string())?;

    let mut encoded = Zeroizing::new(Vec::new());
    for index in 0..count {
        let chunk = store
            .load(&key.child(&format!("#{index}")))?
            .ok_or_else(|| format!("Claude Credential Manager chunk #{index} is missing"))?;
        if chunk.len() > WINDOWS_CREDMAN_CHUNK_BYTES {
            return Err(format!(
                "Claude Credential Manager chunk #{index} exceeds Bun's size limit"
            ));
        }
        encoded.extend_from_slice(&chunk);
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&encoded)
        .map_err(|_| "Claude Credential Manager chunks are not valid base64".to_string())?;
    if decoded.len() != expected_len {
        return Err("Claude Credential Manager decoded length does not match metadata".into());
    }
    Ok(WindowsCredentialRead {
        payload: Some(WindowsCredentialPayload {
            value: decoded,
            chunk_count: Some(count),
        }),
    })
}

#[cfg(windows)]
fn windows_credential_manager_enabled(config_root: &Path) -> Result<bool, String> {
    if std::env::var_os(WINDOWS_CREDMAN_FORCE_ENV).is_some_and(|value| value == "1") {
        return Ok(true);
    }
    let candidates = [
        config_root.join(".config.json"),
        config_root
            .parent()
            .unwrap_or(config_root)
            .join(".claude.json"),
    ];
    for path in candidates {
        if !path.exists() {
            continue;
        }
        let raw = read_bounded_text(&path, MAX_SETTINGS_BYTES)?;
        let value: Value = serde_json::from_str(&raw).map_err(|_| {
            format!(
                "unable to safely inspect Claude feature flags in {}",
                path.display()
            )
        })?;
        if value
            .get("cachedGrowthBookFeatures")
            .and_then(|value| value.get("tengu_windows_credman"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(target_os = "macos")]
fn macos_keychain_account() -> String {
    std::env::var("USER")
        .ok()
        .filter(|value| valid_keychain_account(value))
        .or_else(|| {
            Command::new("id")
                .arg("-un")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|value| value.trim().to_string())
                .filter(|value| valid_keychain_account(value))
        })
        .unwrap_or_else(|| "claude-code-user".into())
}

#[cfg(target_os = "macos")]
fn valid_keychain_account(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activation::PatchEngine;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use yaat_contracts::{ProfileStatus, ProviderProfile};

    #[derive(Default)]
    struct MockCredentialStore {
        values: Mutex<std::collections::HashMap<CredentialKey, Vec<u8>>>,
    }

    impl MockCredentialStore {
        fn insert(&self, key: CredentialKey, value: impl Into<Vec<u8>>) {
            self.values.lock().unwrap().insert(key, value.into());
        }
    }

    impl CredentialStoreAccess for MockCredentialStore {
        fn load(&self, key: &CredentialKey) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
            Ok(self
                .values
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .map(Zeroizing::new))
        }

        fn save(&self, key: &CredentialKey, value: &[u8]) -> Result<(), String> {
            self.values
                .lock()
                .unwrap()
                .insert(key.clone(), value.to_vec());
            Ok(())
        }

        fn delete(&self, key: &CredentialKey) -> Result<bool, String> {
            Ok(self.values.lock().unwrap().remove(key).is_some())
        }
    }

    #[derive(Default)]
    struct ReadbackFailureStore {
        inner: MockCredentialStore,
        loads: AtomicUsize,
    }

    impl CredentialStoreAccess for ReadbackFailureStore {
        fn load(&self, key: &CredentialKey) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
            if self.loads.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err("injected readback failure".into());
            }
            self.inner.load(key)
        }

        fn save(&self, key: &CredentialKey, value: &[u8]) -> Result<(), String> {
            self.inner.save(key, value)
        }

        fn delete(&self, key: &CredentialKey) -> Result<bool, String> {
            self.inner.delete(key)
        }
    }

    struct FailOnceCredentialStore {
        inner: MockCredentialStore,
        account_suffix: &'static str,
        failed: AtomicBool,
    }

    impl FailOnceCredentialStore {
        fn new(account_suffix: &'static str) -> Self {
            Self {
                inner: MockCredentialStore::default(),
                account_suffix,
                failed: AtomicBool::new(false),
            }
        }
    }

    impl CredentialStoreAccess for FailOnceCredentialStore {
        fn load(&self, key: &CredentialKey) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
            self.inner.load(key)
        }

        fn save(&self, key: &CredentialKey, value: &[u8]) -> Result<(), String> {
            if key.account.ends_with(self.account_suffix)
                && !self.failed.swap(true, Ordering::SeqCst)
            {
                return Err("injected Credential Manager write failure".into());
            }
            self.inner.save(key, value)
        }

        fn delete(&self, key: &CredentialKey) -> Result<bool, String> {
            self.inner.delete(key)
        }
    }

    fn account_snapshot(name: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": format!("access-{name}"),
                "refreshToken": format!("refresh-{name}"),
                "expiresAt": 4_000_000_000_000_u64,
                "scopes": ["user:inference"]
            }
        }))
        .unwrap()
    }

    fn large_account_snapshot(name: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": format!("access-{name}"),
                "refreshToken": "r".repeat(WINDOWS_CREDMAN_CHUNK_BYTES * 2),
                "expiresAt": 4_000_000_000_000_u64,
                "scopes": ["user:inference"]
            }
        }))
        .unwrap()
    }

    fn profile(kind: ProviderKind) -> ProviderProfile {
        ProviderProfile {
            id: "profile-1".into(),
            platform: Platform::ClaudeCode,
            kind,
            name: "Claude".into(),
            account_label: None,
            base_url: None,
            model: None,
            secret_kind: if kind == ProviderKind::OfficialSubscription {
                SecretKind::None
            } else {
                SecretKind::ApiKey
            },
            has_secret: kind != ProviderKind::OfficialSubscription,
            profile_home: None,
            status: ProfileStatus::Ready,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn credential_context(temp: &tempfile::TempDir) -> AdapterContext {
        AdapterContext {
            app_data_dir: temp.path().join("data"),
            helper_executable: temp.path().join("yaat"),
            explicit_cli_path: None,
            explicit_config_root: Some(temp.path().to_path_buf()),
        }
    }

    #[test]
    fn version_parser_accepts_native_cli_output_only() {
        assert_eq!(
            parse_cli_version("2.1.220 (Claude Code)\n"),
            Some("2.1.220")
        );
        assert_eq!(parse_cli_version("Claude Code v2"), None);
        assert_eq!(parse_cli_version("2.1.220.1"), None);
    }

    #[test]
    fn provider_patch_preserves_unowned_settings_and_comments() {
        let raw = r#"{
  // must survive exactly
  "permissions": { "allow": ["Read"] },
  "env": {
    "KEEP_ME": "yes",
    "ANTHROPIC_API_KEY": "must-be-removed",
    "ANTHROPIC_BASE_URL": "https://old.example.com"
  },
  "enabledPlugins": { "x@y": true }
}
"#;
        let desired = DesiredSettings {
            api_key_helper: Some(
                "/Applications/YAAT --yaat-credential-helper claude_code ref-1".into(),
            ),
            base_url: Some("https://messages.example.com".into()),
            model: Some("claude-compatible".into()),
        };
        let patched = patch_managed_settings(raw, &desired).unwrap();

        assert!(patched.contains("// must survive exactly"));
        assert!(patched.contains(r#""permissions": { "allow": ["Read"] }"#));
        assert!(patched.contains(r#""KEEP_ME": "yes""#));
        assert!(patched.contains(r#""enabledPlugins": { "x@y": true }"#));
        assert!(!patched.contains("must-be-removed"));
        assert!(patched.contains("https://messages.example.com"));
        assert!(patched.contains("claude-compatible"));
        assert!(patched.contains("apiKeyHelper"));
    }

    #[test]
    fn global_plan_patches_only_claude_account_fields() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("claude-home");
        fs::create_dir(&root).unwrap();
        let source = r#"{
  // keep this comment
  "apiKeyHelper": "old helper",
  "permissions": { "allow": ["Read"] },
  "env": {
    "ANTHROPIC_AUTH_TOKEN": "old",
    "EDITOR": "vim"
  },
  "enabledPlugins": { "x@y": true }
}
"#;
        fs::write(root.join(SETTINGS_FILE_NAME), source).unwrap();
        let profile = profile(ProviderKind::OfficialSubscription);
        let context = AdapterContext {
            app_data_dir: temp.path().join("data"),
            helper_executable: temp.path().join("yaat"),
            explicit_cli_path: None,
            explicit_config_root: Some(root.clone()),
        };

        let plan = ClaudeAdapter
            .global_config_plan(
                &context,
                ProfileRuntime {
                    profile: &profile,
                    secret_ref: None,
                },
            )
            .unwrap();
        PatchEngine::apply_file(&plan.path, plan.format, plan.operations).unwrap();
        let after = fs::read_to_string(&plan.path).unwrap();

        assert!(after.contains("// keep this comment"));
        assert!(after.contains(r#""permissions": { "allow": ["Read"] }"#));
        assert!(after.contains(r#""EDITOR": "vim""#));
        assert!(after.contains(r#""enabledPlugins": { "x@y": true }"#));
        assert!(!after.contains("apiKeyHelper"));
        assert!(!after.contains("ANTHROPIC_AUTH_TOKEN"));
    }

    #[test]
    fn subscription_patch_removes_only_owned_provider_fields() {
        let raw = r#"{
  "apiKeyHelper": "old helper",
  "env": {
    "ANTHROPIC_AUTH_TOKEN": "old token",
    "ANTHROPIC_MODEL": "old-model",
    "EDITOR": "vim"
  },
  "theme": "dark"
}"#;
        let patched = patch_managed_settings(
            raw,
            &DesiredSettings {
                api_key_helper: None,
                base_url: None,
                model: None,
            },
        )
        .unwrap();

        assert!(!patched.contains("apiKeyHelper"));
        assert!(!patched.contains("ANTHROPIC_AUTH_TOKEN"));
        assert!(!patched.contains("ANTHROPIC_MODEL"));
        assert!(patched.contains(r#""EDITOR": "vim""#));
        assert!(patched.contains(r#""theme": "dark""#));
    }

    #[test]
    fn malformed_env_is_rejected_instead_of_overwritten() {
        let result = patch_managed_settings(
            r#"{"env":"do not destroy me","permissions":{"allow":[]}}"#,
            &DesiredSettings {
                api_key_helper: None,
                base_url: Some("https://example.com".into()),
                model: Some("model".into()),
            },
        );
        assert!(result.unwrap_err().contains("`env` must be an object"));
    }

    #[test]
    fn helper_protocol_is_quoted_and_contains_only_a_reference() {
        let command = credential_helper_command(
            Path::new("/Applications/Yet Another Account Tool/yaat"),
            "credential-ref_1",
        )
        .unwrap();
        assert!(command.contains("--yaat-credential-helper claude_code credential-ref_1"));
        assert!(command.starts_with('\''));
    }

    #[test]
    fn command_scrubs_competing_provider_environment() {
        let spec = managed_command_spec(
            PathBuf::from("/usr/bin/claude"),
            vec![],
            PathBuf::from("/private/yaat/profile"),
            None,
        )
        .unwrap();
        assert_eq!(
            spec.env.get("CLAUDE_CONFIG_DIR").unwrap(),
            "/private/yaat/profile"
        );
        assert_eq!(
            spec.env.get("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB").unwrap(),
            "1"
        );
        assert!(spec.env_remove.contains(&"ANTHROPIC_API_KEY".into()));
        assert!(
            spec.env_remove
                .contains(&"CLAUDE_SECURESTORAGE_CONFIG_DIR".into())
        );
        assert!(!spec.env.values().any(|value| value.starts_with("sk-")));
    }

    #[test]
    fn account_snapshot_excludes_every_unowned_secure_storage_field() {
        let full = serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "access-a",
                "refreshToken": "refresh-a"
            },
            "organizationUuid": "org-a",
            "trustedDeviceToken": "device-a",
            "pluginSecrets": {"plugin": "must-not-enter-yaat"},
            "mcpOAuth": {"server": "must-not-enter-yaat"},
            "mcpXaaIdp": {"server": "must-not-enter-yaat"},
            "designOauth": {"accessToken": "must-not-enter-yaat"},
            "gatewayTrust": {"host": true}
        }))
        .unwrap();

        let snapshot = extract_account_snapshot(&full).unwrap();
        let value: Value = serde_json::from_slice(&snapshot).unwrap();
        assert!(value.get("claudeAiOauth").is_some());
        assert_eq!(
            value.get("organizationUuid"),
            Some(&Value::String("org-a".into()))
        );
        assert_eq!(
            value.get("trustedDeviceToken"),
            Some(&Value::String("device-a".into()))
        );
        for field in [
            "pluginSecrets",
            "mcpOAuth",
            "mcpXaaIdp",
            "designOauth",
            "gatewayTrust",
        ] {
            assert!(value.get(field).is_none(), "snapshot leaked {field}");
        }
    }

    #[test]
    fn account_merge_replaces_only_account_fields() {
        let current = serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "access-a",
                "refreshToken": "refresh-a"
            },
            "organizationUuid": "old-org",
            "trustedDeviceToken": "old-device",
            "pluginSecrets": {"plugin": "keep"},
            "mcpOAuth": {"server": "keep"},
            "designOauth": {"accessToken": "keep"}
        }))
        .unwrap();
        let merged = merge_account_fields(&current, &account_snapshot("b"), true).unwrap();
        let value: Value = serde_json::from_slice(&merged).unwrap();

        assert_eq!(
            value["claudeAiOauth"]["accessToken"],
            Value::String("access-b".into())
        );
        assert!(value.get("organizationUuid").is_none());
        assert!(value.get("trustedDeviceToken").is_none());
        assert_eq!(value["pluginSecrets"]["plugin"], "keep");
        assert_eq!(value["mcpOAuth"]["server"], "keep");
        assert_eq!(value["designOauth"]["accessToken"], "keep");
    }

    #[test]
    fn empty_credential_slot_accepts_the_first_account() {
        let temp = tempfile::tempdir().unwrap();
        let context = credential_context(&temp);
        let store = MockCredentialStore::default();
        let target = account_snapshot("a");
        mutate_account_fields(&context, temp.path(), &store, &target).unwrap();

        let slot = resolve_credential_slot(&context, temp.path(), &store).unwrap();
        let stored = slot.payload.as_deref().expect("credential was written");
        assert!(account_fields_match(stored, &target).unwrap());
        drop(slot);

        clear_account_fields(&context, temp.path(), &store).unwrap();
        let slot = resolve_credential_slot(&context, temp.path(), &store).unwrap();
        assert!(slot.payload.is_none());
    }

    #[test]
    fn clearing_account_fields_preserves_unowned_secure_storage() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(CREDENTIALS_FILE_NAME);
        let current = serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "access-a",
                "refreshToken": "refresh-a"
            },
            "organizationUuid": "org-a",
            "pluginSecrets": {"plugin": "keep"},
            "mcpOAuth": {"server": "keep"}
        }))
        .unwrap();
        fs::write(&path, current).unwrap();

        clear_account_fields(
            &credential_context(&temp),
            temp.path(),
            &MockCredentialStore::default(),
        )
        .unwrap();

        let stored = fs::read(path).unwrap();
        assert!(account_fields_match(&stored, b"{}").unwrap());
        let stored: Value = serde_json::from_slice(&stored).unwrap();
        assert_eq!(stored["pluginSecrets"]["plugin"], "keep");
        assert_eq!(stored["mcpOAuth"]["server"], "keep");
    }

    #[test]
    fn credential_file_switch_preserves_unowned_fields() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(CREDENTIALS_FILE_NAME);
        let previous = serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "access-a",
                "refreshToken": "refresh-a"
            },
            "pluginSecrets": {"plugin": "keep"},
            "mcpOAuth": {"server": "keep"}
        }))
        .unwrap();
        fs::write(&path, &previous).unwrap();
        let target = account_snapshot("b");
        let replacement = merge_account_fields(&previous, &target, true).unwrap();

        replace_credential_file(&path, Some(&previous), &replacement, &target).unwrap();

        let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(value["claudeAiOauth"]["accessToken"], "access-b");
        assert_eq!(value["pluginSecrets"]["plugin"], "keep");
        assert_eq!(value["mcpOAuth"]["server"], "keep");
    }

    #[test]
    fn keyring_switch_preserves_unowned_fields_and_removes_unchanged_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let fallback_path = temp.path().join(CREDENTIALS_FILE_NAME);
        let previous = serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "access-a",
                "refreshToken": "refresh-a"
            },
            "pluginSecrets": {"plugin": "keep"},
            "designOauth": {"accessToken": "keep"}
        }))
        .unwrap();
        fs::write(&fallback_path, &previous).unwrap();
        let store = MockCredentialStore::default();
        let key = CredentialKey::new("Claude Code-credentials".into(), "user".into(), false);
        store.insert(key.clone(), previous.clone());
        let target = account_snapshot("b");
        let replacement = merge_account_fields(&previous, &target, true).unwrap();
        let slot = ResolvedCredentialSlot {
            backend: CredentialBackend::Keyring {
                key: key.clone(),
                windows_chunk_count: None,
            },
            payload: Some(previous.clone()),
            fallback_path: fallback_path.clone(),
            fallback_payload: Some(previous),
        };

        replace_credential_keyring(&store, &slot, &key, None, &replacement, &target).unwrap();

        let current = store.load(&key).unwrap().unwrap();
        let value: Value = serde_json::from_slice(&current).unwrap();
        assert_eq!(value["claudeAiOauth"]["accessToken"], "access-b");
        assert_eq!(value["pluginSecrets"]["plugin"], "keep");
        assert_eq!(value["designOauth"]["accessToken"], "keep");
        assert!(!fallback_path.exists());
    }

    #[test]
    fn keyring_readback_error_restores_previous_account_fields() {
        let temp = tempfile::tempdir().unwrap();
        let fallback_path = temp.path().join(CREDENTIALS_FILE_NAME);
        let store = ReadbackFailureStore::default();
        let key = CredentialKey::new("Claude Code-credentials".into(), "user".into(), false);
        let mut previous_value: Value = serde_json::from_slice(&account_snapshot("a")).unwrap();
        previous_value.as_object_mut().unwrap().insert(
            "pluginSecrets".into(),
            serde_json::json!({"plugin": "keep"}),
        );
        let previous = serde_json::to_vec(&previous_value).unwrap();
        store.inner.insert(key.clone(), previous.clone());
        let target = account_snapshot("b");
        let replacement = merge_account_fields(&previous, &target, true).unwrap();
        let slot = ResolvedCredentialSlot {
            backend: CredentialBackend::Keyring {
                key: key.clone(),
                windows_chunk_count: None,
            },
            payload: Some(previous),
            fallback_path,
            fallback_payload: None,
        };

        let error = replace_credential_keyring(&store, &slot, &key, None, &replacement, &target)
            .unwrap_err();

        assert!(error.contains("injected readback failure"));
        let restored = store.inner.load(&key).unwrap().unwrap();
        assert!(account_fields_match(&restored, &account_snapshot("a")).unwrap());
        let value: Value = serde_json::from_slice(&restored).unwrap();
        assert_eq!(value["pluginSecrets"]["plugin"], "keep");
    }

    #[test]
    fn windows_bun_chunk_reader_reassembles_without_mutating_entries() {
        let store = MockCredentialStore::default();
        let key = CredentialKey::new(
            "Claude Code-credentials-deadbeef".into(),
            WINDOWS_CREDMAN_ACCOUNT.into(),
            true,
        );
        let value = account_snapshot("chunked");
        let encoded = base64::engine::general_purpose::STANDARD.encode(&value);
        let chunks = encoded
            .as_bytes()
            .chunks(17)
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>();
        let chunk_count = chunks.len();
        store.insert(
            key.child("#m"),
            serde_json::to_vec(&serde_json::json!({"n": chunk_count, "l": value.len()})).unwrap(),
        );
        for (index, chunk) in chunks.into_iter().enumerate() {
            store.insert(key.child(&format!("#{index}")), chunk);
        }

        let loaded = read_windows_credential(&store, &key)
            .unwrap()
            .payload
            .unwrap();
        assert_eq!(loaded.chunk_count, Some(chunk_count));
        assert_eq!(loaded.value, value);
    }

    #[test]
    fn windows_bun_chunk_writer_switches_large_credentials_and_preserves_unowned_fields() {
        let temp = tempfile::tempdir().unwrap();
        let store = MockCredentialStore::default();
        let key = CredentialKey::new(
            "Claude Code-credentials-deadbeef".into(),
            WINDOWS_CREDMAN_ACCOUNT.into(),
            true,
        );
        let previous = serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "access-a",
                "refreshToken": "refresh-a"
            },
            "pluginSecrets": {"plugin": "keep"}
        }))
        .unwrap();
        store.insert(key.clone(), previous.clone());
        let target = large_account_snapshot("b");
        let replacement = merge_account_fields(&previous, &target, true).unwrap();
        let slot = ResolvedCredentialSlot {
            backend: CredentialBackend::Keyring {
                key: key.clone(),
                windows_chunk_count: None,
            },
            payload: Some(previous),
            fallback_path: temp.path().join(CREDENTIALS_FILE_NAME),
            fallback_payload: None,
        };

        replace_credential_keyring(&store, &slot, &key, None, &replacement, &target).unwrap();

        let loaded = read_windows_credential(&store, &key)
            .unwrap()
            .payload
            .unwrap();
        assert!(loaded.chunk_count.is_some());
        assert!(account_fields_match(&loaded.value, &target).unwrap());
        let document: Value = serde_json::from_slice(&loaded.value).unwrap();
        assert_eq!(document["pluginSecrets"]["plugin"], "keep");
        assert!(store.load(&key).unwrap().is_none());
        assert!(store.load(&key.child("#p")).unwrap().is_none());
    }

    #[test]
    fn windows_bun_chunked_value_can_switch_back_to_a_direct_value() {
        let temp = tempfile::tempdir().unwrap();
        let store = MockCredentialStore::default();
        let key = CredentialKey::new(
            "Claude Code-credentials-deadbeef".into(),
            WINDOWS_CREDMAN_ACCOUNT.into(),
            true,
        );
        let previous = large_account_snapshot("a");
        write_windows_credential(&store, &key, &previous, 0, true).unwrap();
        let previous_chunk_count = read_windows_credential(&store, &key)
            .unwrap()
            .payload
            .unwrap()
            .chunk_count
            .unwrap();
        let target = account_snapshot("b");
        let replacement = merge_account_fields(&previous, &target, true).unwrap();
        let slot = ResolvedCredentialSlot {
            backend: CredentialBackend::Keyring {
                key: key.clone(),
                windows_chunk_count: Some(previous_chunk_count),
            },
            payload: Some(previous),
            fallback_path: temp.path().join(CREDENTIALS_FILE_NAME),
            fallback_payload: None,
        };

        replace_credential_keyring(
            &store,
            &slot,
            &key,
            Some(previous_chunk_count),
            &replacement,
            &target,
        )
        .unwrap();

        let loaded = read_windows_credential(&store, &key)
            .unwrap()
            .payload
            .unwrap();
        assert_eq!(loaded.chunk_count, None);
        assert!(account_fields_match(&loaded.value, &target).unwrap());
        assert!(store.load(&key.child("#m")).unwrap().is_none());
        for index in 0..previous_chunk_count {
            assert!(
                store
                    .load(&key.child(&format!("#{index}")))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn windows_bun_chunk_write_failure_reports_stage_and_restores_previous_value() {
        let temp = tempfile::tempdir().unwrap();
        let store = FailOnceCredentialStore::new("#1");
        let key = CredentialKey::new(
            "Claude Code-credentials-deadbeef".into(),
            WINDOWS_CREDMAN_ACCOUNT.into(),
            true,
        );
        let previous = account_snapshot("a");
        store.inner.insert(key.clone(), previous.clone());
        let target = large_account_snapshot("b");
        let replacement = merge_account_fields(&previous, &target, true).unwrap();
        let slot = ResolvedCredentialSlot {
            backend: CredentialBackend::Keyring {
                key: key.clone(),
                windows_chunk_count: None,
            },
            payload: Some(previous.clone()),
            fallback_path: temp.path().join(CREDENTIALS_FILE_NAME),
            fallback_payload: None,
        };

        let error = replace_credential_keyring(&store, &slot, &key, None, &replacement, &target)
            .unwrap_err();

        assert!(error.contains("unable to write Bun chunk #1"));
        assert!(error.contains("rollback: verified"));
        let restored = read_windows_credential(&store, &key)
            .unwrap()
            .payload
            .unwrap();
        assert_eq!(restored.value, previous);
        assert_eq!(restored.chunk_count, None);
        assert!(store.load(&key.child("#p")).unwrap().is_none());
    }

    #[test]
    fn windows_bun_chunked_value_can_be_removed() {
        let store = MockCredentialStore::default();
        let key = CredentialKey::new(
            "Claude Code-credentials-deadbeef".into(),
            WINDOWS_CREDMAN_ACCOUNT.into(),
            true,
        );
        let previous = large_account_snapshot("a");
        write_windows_credential(&store, &key, &previous, 0, true).unwrap();
        let previous_chunk_count = read_windows_credential(&store, &key)
            .unwrap()
            .payload
            .unwrap()
            .chunk_count
            .unwrap();

        remove_windows_credential_manager(
            &store,
            &key,
            Some(previous_chunk_count),
            Some(&previous),
        )
        .unwrap();

        assert!(
            read_windows_credential(&store, &key)
                .unwrap()
                .payload
                .is_none()
        );
        assert!(store.load(&key.child("#p")).unwrap().is_none());
        assert!(store.load(&key.child("#m")).unwrap().is_none());
        for index in 0..previous_chunk_count {
            assert!(
                store
                    .load(&key.child(&format!("#{index}")))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn secure_storage_service_hash_normalizes_unicode_to_nfc() {
        let composed = SecureStorageScope {
            root: PathBuf::from("/tmp/Claude-é"),
            unscoped: false,
        };
        let decomposed = SecureStorageScope {
            root: PathBuf::from("/tmp/Claude-e\u{301}"),
            unscoped: false,
        };
        assert_eq!(
            keychain_service_name(&composed).unwrap(),
            keychain_service_name(&decomposed).unwrap()
        );
        assert_eq!(
            keychain_service_name(&SecureStorageScope {
                root: PathBuf::from("ignored"),
                unscoped: true,
            })
            .unwrap(),
            KEYCHAIN_SERVICE_PREFIX
        );
    }

    #[test]
    fn full_secure_storage_document_is_rejected_as_a_snapshot() {
        let payload = serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "access-a",
                "refreshToken": "refresh-a"
            },
            "pluginSecrets": {"plugin": "must-not-be-restored"}
        }))
        .unwrap();
        let snapshot = CredentialSnapshot {
            storage_kind: CREDENTIAL_STORAGE_KIND.into(),
            opaque_payload: payload,
            account_label: None,
            warning: None,
        };
        assert!(validate_snapshot(&snapshot).is_err());
    }

    #[test]
    fn prepare_profile_derives_settings_without_copying_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join(SETTINGS_FILE_NAME),
            r#"{"permissions":{"allow":["Read"]},"env":{"KEEP":"1"}}"#,
        )
        .unwrap();
        fs::write(source.join(".credentials.json"), b"never copy this").unwrap();
        let helper = temp.path().join("yaat helper");
        let context = AdapterContext {
            app_data_dir: temp.path().join("data"),
            helper_executable: helper,
            explicit_cli_path: None,
            explicit_config_root: Some(source),
        };
        let profile = profile(ProviderKind::OfficialSubscription);
        let root = ClaudeAdapter
            .prepare_profile(
                &context,
                ProfileRuntime {
                    profile: &profile,
                    secret_ref: None,
                },
            )
            .unwrap();

        let settings = fs::read_to_string(root.join(SETTINGS_FILE_NAME)).unwrap();
        assert!(settings.contains("permissions"));
        assert!(settings.contains("KEEP"));
        assert!(!root.join(".credentials.json").exists());
        assert!(!root.join(".claude.json").exists());
    }

    #[test]
    fn third_party_requires_its_own_api_key_reference() {
        let temp = tempfile::tempdir().unwrap();
        let context = AdapterContext {
            app_data_dir: temp.path().join("data"),
            helper_executable: temp.path().join("yaat"),
            explicit_cli_path: None,
            explicit_config_root: Some(temp.path().join("source")),
        };
        let mut profile = profile(ProviderKind::ThirdParty);
        profile.base_url = Some("https://messages.example.com".into());
        profile.model = Some("provider-model".into());

        let error = ClaudeAdapter
            .prepare_profile(
                &context,
                ProfileRuntime {
                    profile: &profile,
                    secret_ref: None,
                },
            )
            .unwrap_err();
        assert!(error.contains("no credential reference"));
    }
}
