//! Isolated Claude Desktop profiles and native third-party provider entries.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use yaat_contracts::{HeaderEntry, Platform, ProviderKind, ProviderPlatformConfig, SecretKind};

use crate::activation::{
    ConfigFormat, OwnedPath, PatchEngine, PatchOperation, remove_atomically, replace_atomically,
};

use super::{
    AdapterContext, CommandSpec, CredentialSnapshot, CredentialState, GlobalConfigPlan,
    PlatformAdapter, ProfileRuntime, claude_desktop_credentials,
};

const USER_DATA_ENV: &str = "CLAUDE_USER_DATA_DIR";
const CONFIG_FILE: &str = "claude_desktop_config.json";
const CONFIG_LIBRARY_DIR: &str = "configLibrary";
const PROFILE_ID: &str = "00000000-0000-4000-8000-000000796161";
const PROFILE_NAME: &str = "YAAT";
const OFFICIAL_API_BASE_URL: &str = "https://api.anthropic.com";
const MAX_JSON_BYTES: u64 = 4 * 1024 * 1024;
const CREDENTIAL_STORAGE_KIND: &str = "claude_desktop_credential_v1";

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DesktopCredential {
    Account {
        data: claude_desktop_credentials::AccountData,
    },
    GatewayTarget {
        credential: String,
        secret_kind: SecretKind,
        base_url: String,
        models: Vec<String>,
        custom_headers: Vec<HeaderEntry>,
        user_agent: Option<String>,
    },
    GatewayState {
        meta_existed: bool,
        entries_existed: bool,
        applied_id: Option<Vec<u8>>,
        meta_entry: Option<Vec<u8>>,
        entry: Option<Vec<u8>>,
    },
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ClaudeDesktopAdapter;

impl ClaudeDesktopAdapter {
    pub const fn new() -> Self {
        Self
    }

    fn profile_root(
        &self,
        context: &AdapterContext,
        runtime: &ProfileRuntime<'_>,
    ) -> Result<PathBuf, String> {
        ensure_profile(runtime)?;
        crate::paths::validate_identifier(&runtime.profile.id)
            .map_err(|error| error.to_string())?;
        crate::paths::managed_profile_home_at(
            &context.data_root,
            Platform::ClaudeDesktop,
            &runtime.profile.id,
        )
        .map_err(|error| error.to_string())
    }

    fn prepare_deployment_mode(root: &Path, mode: &str) -> Result<(), String> {
        let path = root.join(CONFIG_FILE);
        let operation = PatchOperation::set(
            OwnedPath::from_segments(["deploymentMode"]).map_err(|error| error.to_string())?,
            Value::String(mode.into()),
        );
        let prepared = PatchEngine::prepare_file(&path, ConfigFormat::Json, vec![operation])
            .map_err(|error| error.to_string())?;
        PatchEngine::commit(prepared)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn prepare_gateway_profile(
        &self,
        _context: &AdapterContext,
        runtime: &ProfileRuntime<'_>,
        root: &Path,
    ) -> Result<(), String> {
        let profile = runtime.profile;
        if !matches!(
            profile.kind,
            ProviderKind::OfficialApi | ProviderKind::ThirdParty
        ) {
            return Err("only API-backed Claude Desktop profiles use 3P gateway mode".into());
        }
        if !matches!(
            profile.secret_kind,
            SecretKind::ApiKey | SecretKind::BearerToken
        ) || !profile.has_secret
        {
            return Err("Claude Desktop gateway profiles require a stored credential".into());
        }
        let secret = runtime.secret.ok_or_else(|| {
            "Claude Desktop gateway profile has no credential in the local database".to_string()
        })?;
        let base_url = match profile.kind {
            ProviderKind::OfficialApi => {
                if profile.base_url.is_some() {
                    return Err("Anthropic API profiles use the official endpoint".into());
                }
                OFFICIAL_API_BASE_URL.to_string()
            }
            ProviderKind::ThirdParty => {
                let value = profile.base_url.as_deref().ok_or_else(|| {
                    "a direct Claude Desktop provider requires a base URL".to_string()
                })?;
                crate::validation::validate_provider_url(value)
                    .map_err(|error| error.to_string())?;
                value.trim_end_matches('/').to_string()
            }
            ProviderKind::OfficialSubscription => unreachable!(),
        };

        let models = desktop_models(profile)?;
        validate_direct_models(&models, profile.kind == ProviderKind::ThirdParty)?;
        prepare_gateway_config(
            root,
            secret,
            profile.secret_kind,
            &base_url,
            &models,
            &profile.custom_headers,
            profile.user_agent.as_deref(),
        )
    }

    fn launch_command(
        &self,
        context: &AdapterContext,
        runtime: ProfileRuntime<'_>,
    ) -> Result<CommandSpec, String> {
        let root = self.prepare_profile(context, runtime.clone())?;
        let (program, _) = self.discover_cli(context)?;
        let env = BTreeMap::from([(
            USER_DATA_ENV.to_string(),
            root.to_string_lossy().into_owned(),
        )]);
        Ok(CommandSpec {
            program,
            args: Vec::new(),
            env,
            env_remove: vec![
                "ANTHROPIC_API_KEY".into(),
                "ANTHROPIC_AUTH_TOKEN".into(),
                "ANTHROPIC_BASE_URL".into(),
                "CLAUDE_AI_URL".into(),
            ],
            cwd: None,
        })
    }
}

pub(crate) fn global_credential_for_profile(
    profile: &yaat_contracts::ProviderProfile,
    secret: &str,
) -> Result<CredentialSnapshot, String> {
    if profile.platform != Platform::ClaudeDesktop
        || !matches!(
            profile.kind,
            ProviderKind::OfficialApi | ProviderKind::ThirdParty
        )
    {
        return Err(
            "only API-backed Claude Desktop profiles use a generated global credential".into(),
        );
    }
    if !matches!(
        profile.secret_kind,
        SecretKind::ApiKey | SecretKind::BearerToken
    ) || !profile.has_secret
    {
        return Err("Claude Desktop gateway profiles require a saved credential".into());
    }
    crate::paths::validate_identifier(&profile.id).map_err(|error| error.to_string())?;
    let base_url = match profile.kind {
        ProviderKind::OfficialApi => {
            if profile.base_url.is_some() {
                return Err("Anthropic API profiles use the official endpoint".into());
            }
            OFFICIAL_API_BASE_URL.to_string()
        }
        ProviderKind::ThirdParty => {
            let value = profile.base_url.as_deref().ok_or_else(|| {
                "a direct Claude Desktop provider requires a base URL".to_string()
            })?;
            crate::validation::validate_provider_url(value).map_err(|error| error.to_string())?;
            value.trim_end_matches('/').to_string()
        }
        ProviderKind::OfficialSubscription => unreachable!(),
    };
    let models = desktop_models(profile)?;
    validate_direct_models(&models, profile.kind == ProviderKind::ThirdParty)?;
    encode_credential(
        DesktopCredential::GatewayTarget {
            credential: secret.to_owned(),
            secret_kind: profile.secret_kind,
            base_url,
            models,
            custom_headers: profile.custom_headers.clone(),
            user_agent: profile.user_agent.clone(),
        },
        None,
        None,
    )
}

fn prepare_gateway_config(
    root: &Path,
    credential: &str,
    secret_kind: SecretKind,
    base_url: &str,
    models: &[String],
    custom_headers: &[HeaderEntry],
    user_agent: Option<&str>,
) -> Result<(), String> {
    let mut value = json!({
        "disableDeploymentModeChooser": true,
        "inferenceProvider": "gateway",
        "inferenceGatewayBaseUrl": base_url,
        "inferenceGatewayApiKey": credential,
        "inferenceGatewayAuthScheme": if secret_kind == SecretKind::ApiKey { "x-api-key" } else { "bearer" }
    });
    if !models.is_empty() {
        value["inferenceModels"] = json!(models);
    }
    let mut headers = custom_headers
        .iter()
        .map(|entry| (entry.name.clone(), Value::String(entry.value.clone())))
        .collect::<serde_json::Map<_, _>>();
    if let Some(user_agent) = user_agent.filter(|value| !value.trim().is_empty()) {
        headers.insert("User-Agent".into(), Value::String(user_agent.trim().into()));
    }
    if !headers.is_empty() {
        value["inferenceCustomHeaders"] = Value::Object(headers);
    }

    let library = root.join(CONFIG_LIBRARY_DIR);
    crate::paths::ensure_private_directory(&library).map_err(|error| error.to_string())?;
    write_json_atomic(&library.join(format!("{PROFILE_ID}.json")), &value)?;
    merge_meta(&library.join("_meta.json"))
}

impl PlatformAdapter for ClaudeDesktopAdapter {
    fn discover_cli(&self, context: &AdapterContext) -> Result<(PathBuf, String), String> {
        let program = match context.explicit_cli_path.as_ref() {
            Some(path) => path.clone(),
            None => default_executable()?,
        };
        if !program.is_absolute() {
            return Err("configured Claude Desktop executable path must be absolute".into());
        }
        let metadata = fs::metadata(&program)
            .map_err(|_| "configured Claude Desktop executable is not readable".to_string())?;
        if !metadata.file_type().is_file() {
            return Err("configured Claude Desktop executable must be a regular file".into());
        }
        let (status, stdout, _) =
            crate::process::run_with_timeout(&program, &["--version"], Duration::from_secs(3))?;
        if !status.success() {
            return Err("Claude Desktop --version exited unsuccessfully".into());
        }
        let version = String::from_utf8(stdout)
            .map_err(|_| "Claude Desktop version output is not UTF-8".to_string())?
            .trim()
            .to_string();
        if version.is_empty() || version.len() > 128 || version.chars().any(char::is_control) {
            return Err("Claude Desktop returned an invalid version".into());
        }
        Ok((program, version))
    }

    fn prepare_profile(
        &self,
        context: &AdapterContext,
        runtime: ProfileRuntime<'_>,
    ) -> Result<PathBuf, String> {
        let root = self.profile_root(context, &runtime)?;
        crate::paths::ensure_private_directory(&root).map_err(|error| error.to_string())?;
        match runtime.profile.kind {
            ProviderKind::OfficialSubscription => Self::prepare_deployment_mode(&root, "1p")?,
            ProviderKind::OfficialApi | ProviderKind::ThirdParty => {
                Self::prepare_deployment_mode(&root, "3p")?;
                self.prepare_gateway_profile(context, &runtime, &root)?;
            }
        }
        Ok(root)
    }

    fn login_spec(
        &self,
        context: &AdapterContext,
        runtime: ProfileRuntime<'_>,
        _console: bool,
    ) -> Result<CommandSpec, String> {
        if runtime.profile.kind != ProviderKind::OfficialSubscription {
            return Err(
                "Claude Desktop gateway profiles do not use the official sign-in flow".into(),
            );
        }
        self.launch_command(context, runtime)
    }

    fn launch_spec(
        &self,
        context: &AdapterContext,
        runtime: ProfileRuntime<'_>,
        _cwd: Option<PathBuf>,
        passthrough_args: Vec<String>,
    ) -> Result<CommandSpec, String> {
        if !passthrough_args.is_empty() {
            return Err("Claude Desktop managed launch does not accept CLI arguments".into());
        }
        self.launch_command(context, runtime)
    }

    fn capture_credentials(
        &self,
        _context: &AdapterContext,
        config_root: &Path,
    ) -> Result<CredentialSnapshot, String> {
        let config = read_json_object(&config_root.join("config.json"))?;
        let (data, warning) = claude_desktop_credentials::capture(config_root, &config)?;
        let label = data.label();
        encode_credential(DesktopCredential::Account { data }, Some(label), warning)
    }

    fn capture_credential_state(
        &self,
        _context: &AdapterContext,
        config_root: &Path,
    ) -> Result<CredentialState, String> {
        let desktop_config = read_json_object(&config_root.join(CONFIG_FILE))?;
        if desktop_config.get("deploymentMode").and_then(Value::as_str) == Some("3p") {
            return capture_gateway_state(config_root).map(CredentialState::Present);
        }
        let account_config = read_json_object(&config_root.join("config.json"))?;
        if !claude_desktop_credentials::has_account(&account_config) {
            return Ok(CredentialState::Absent);
        }
        let (data, warning) = claude_desktop_credentials::capture(config_root, &account_config)?;
        let label = data.label();
        encode_credential(DesktopCredential::Account { data }, Some(label), warning)
            .map(CredentialState::Present)
    }

    fn restore_credentials(
        &self,
        _context: &AdapterContext,
        config_root: &Path,
        snapshot: &CredentialSnapshot,
    ) -> Result<(), String> {
        match decode_credential(snapshot)? {
            DesktopCredential::Account { data } => {
                claude_desktop_credentials::restore(config_root, &data)
            }
            DesktopCredential::GatewayTarget {
                credential,
                secret_kind,
                base_url,
                models,
                custom_headers,
                user_agent,
            } => prepare_gateway_config(
                &global_gateway_root(config_root)?,
                &credential,
                secret_kind,
                &base_url,
                &models,
                &custom_headers,
                user_agent.as_deref(),
            ),
            DesktopCredential::GatewayState {
                meta_existed,
                entries_existed,
                applied_id,
                meta_entry,
                entry,
            } => restore_gateway_state(
                config_root,
                meta_existed,
                entries_existed,
                applied_id,
                meta_entry,
                entry,
            ),
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
            CredentialState::Absent => claude_desktop_credentials::clear(config_root),
        }
    }

    fn global_config_plan(
        &self,
        _context: &AdapterContext,
        runtime: ProfileRuntime<'_>,
    ) -> Result<GlobalConfigPlan, String> {
        ensure_profile(&runtime)?;
        let root = crate::paths::default_config_root(Platform::ClaudeDesktop)
            .map_err(|error| error.to_string())?;
        let mode = match runtime.profile.kind {
            ProviderKind::OfficialSubscription => "1p",
            ProviderKind::OfficialApi | ProviderKind::ThirdParty => "3p",
        };
        Ok(GlobalConfigPlan {
            path: root.join(CONFIG_FILE),
            format: ConfigFormat::Json,
            operations: vec![PatchOperation::set(
                OwnedPath::from_segments(["deploymentMode"]).map_err(|error| error.to_string())?,
                Value::String(mode.into()),
            )],
            sidecars: Vec::new(),
        })
    }
}

fn encode_credential(
    credential: DesktopCredential,
    account_label: Option<String>,
    warning: Option<String>,
) -> Result<CredentialSnapshot, String> {
    let opaque_payload = serde_json::to_vec(&credential).map_err(|error| error.to_string())?;
    Ok(CredentialSnapshot {
        storage_kind: CREDENTIAL_STORAGE_KIND.into(),
        opaque_payload,
        account_label,
        warning,
    })
}

fn decode_credential(snapshot: &CredentialSnapshot) -> Result<DesktopCredential, String> {
    if snapshot.storage_kind != CREDENTIAL_STORAGE_KIND {
        return Err("saved Claude Desktop credential has an unsupported format".into());
    }
    serde_json::from_slice(&snapshot.opaque_payload)
        .map_err(|_| "saved Claude Desktop credential is malformed".to_string())
}

fn capture_gateway_state(config_root: &Path) -> Result<CredentialSnapshot, String> {
    let root = global_gateway_root(config_root)?;
    let library = root.join(CONFIG_LIBRARY_DIR);
    let meta_path = library.join("_meta.json");
    let meta_existed = meta_path.exists();
    let meta = read_json_object(&meta_path)?;
    let entries_existed = meta.get("entries").is_some();
    let applied_id = meta
        .get("appliedId")
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|error| error.to_string())?;
    let meta_entry = meta
        .get("entries")
        .and_then(Value::as_array)
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry.get("id").and_then(Value::as_str) == Some(PROFILE_ID))
        })
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|error| error.to_string())?;
    let credential = DesktopCredential::GatewayState {
        meta_existed,
        entries_existed,
        applied_id,
        meta_entry,
        entry: read_optional_file(&library.join(format!("{PROFILE_ID}.json")), MAX_JSON_BYTES)?,
    };
    encode_credential(credential, None, None)
}

fn restore_gateway_state(
    config_root: &Path,
    meta_existed: bool,
    entries_existed: bool,
    applied_id: Option<Vec<u8>>,
    meta_entry: Option<Vec<u8>>,
    entry: Option<Vec<u8>>,
) -> Result<(), String> {
    let root = global_gateway_root(config_root)?;
    let library = root.join(CONFIG_LIBRARY_DIR);
    restore_meta_state(
        &library.join("_meta.json"),
        meta_existed,
        entries_existed,
        applied_id.as_deref(),
        meta_entry.as_deref(),
    )?;
    restore_optional_file(
        &library.join(format!("{PROFILE_ID}.json")),
        entry.as_deref(),
    )?;
    Ok(())
}

fn restore_meta_state(
    path: &Path,
    meta_existed: bool,
    entries_existed: bool,
    applied_id: Option<&[u8]>,
    saved_entry: Option<&[u8]>,
) -> Result<(), String> {
    let mut value = read_json_object(path)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| "Claude Desktop configLibrary metadata must be an object".to_string())?;
    let mut entries = match object.get("entries") {
        Some(Value::Array(entries)) => entries.clone(),
        Some(_) => return Err("Claude Desktop configLibrary entries must be an array".into()),
        None => Vec::new(),
    };
    entries.retain(|entry| entry.get("id").and_then(Value::as_str) != Some(PROFILE_ID));
    if let Some(saved_entry) = saved_entry {
        let entry: Value = serde_json::from_slice(saved_entry)
            .map_err(|_| "saved Claude Desktop configLibrary entry is malformed".to_string())?;
        entries.push(entry);
    }
    if entries_existed || !entries.is_empty() {
        object.insert("entries".into(), Value::Array(entries));
    } else {
        object.remove("entries");
    }
    match applied_id {
        Some(applied_id) => {
            let applied_id: Value = serde_json::from_slice(applied_id)
                .map_err(|_| "saved Claude Desktop applied provider is malformed".to_string())?;
            object.insert("appliedId".into(), applied_id);
        }
        None => {
            object.remove("appliedId");
        }
    }
    if !meta_existed && object.is_empty() {
        remove_atomically(path).map_err(|error| error.to_string())
    } else {
        write_json_atomic(path, &value)
    }
}

fn global_gateway_root(config_root: &Path) -> Result<PathBuf, String> {
    if !config_root.is_absolute() {
        return Err("Claude Desktop config root must be absolute".into());
    }
    let name = config_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "Claude Desktop config root has no file name".to_string())?;
    if name.ends_with("-3p") {
        return Ok(config_root.to_path_buf());
    }
    Ok(config_root.with_file_name(format!("{name}-3p")))
}

fn read_optional_file(path: &Path, maximum: u64) -> Result<Option<Vec<u8>>, String> {
    match fs::metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(format!("refusing non-regular file {}", path.display()));
            }
            if metadata.len() > maximum {
                return Err(format!("file is too large: {}", path.display()));
            }
            fs::read(path).map(Some).map_err(|error| error.to_string())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn restore_optional_file(path: &Path, bytes: Option<&[u8]>) -> Result<(), String> {
    match bytes {
        Some(bytes) => {
            if let Some(parent) = path.parent() {
                crate::paths::ensure_private_directory(parent)
                    .map_err(|error| error.to_string())?;
            }
            replace_atomically(path, bytes)
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
        None => remove_atomically(path).map_err(|error| error.to_string()),
    }
}

fn ensure_profile(runtime: &ProfileRuntime<'_>) -> Result<(), String> {
    if runtime.profile.platform != Platform::ClaudeDesktop {
        return Err("profile does not belong to Claude Desktop".into());
    }
    match runtime.profile.kind {
        ProviderKind::OfficialSubscription => {
            if runtime.profile.secret_kind != SecretKind::None
                || runtime.profile.has_secret
                || runtime.secret.is_some()
                || runtime.profile.base_url.is_some()
            {
                return Err(
                    "Claude Desktop subscription profiles authenticate only inside their isolated user-data directory"
                        .into(),
                );
            }
        }
        ProviderKind::OfficialApi | ProviderKind::ThirdParty => {}
    }
    Ok(())
}

pub(crate) fn validate_direct_model(model: Option<&str>, required: bool) -> Result<(), String> {
    if required && model.is_none() {
        return Err("a direct Claude Desktop provider requires a model route".into());
    }
    if let Some(model) = model
        && !is_safe_direct_model(model)
    {
        return Err(
            "Claude Desktop direct mode accepts only claude-sonnet-*, claude-opus-*, claude-haiku-* or claude-fable-* routes; this provider needs a local mapping proxy"
                .into(),
        );
    }
    Ok(())
}

fn validate_direct_models(models: &[String], required: bool) -> Result<(), String> {
    if required && models.is_empty() {
        return Err("a direct Claude Desktop provider requires at least one model".into());
    }
    for model in models {
        validate_direct_model(Some(model), true)?;
    }
    Ok(())
}

fn desktop_models(profile: &yaat_contracts::ProviderProfile) -> Result<Vec<String>, String> {
    let ProviderPlatformConfig::ClaudeDesktop { models } = &profile.platform_config else {
        return Err("Claude Desktop profile has mismatched platform config".into());
    };
    let mut values = models.clone();
    if values.is_empty()
        && let Some(model) = profile
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    {
        values.push(model.to_owned());
    }
    values.sort();
    values.dedup();
    Ok(values)
}

fn is_safe_direct_model(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    let Some(tail) = normalized
        .strip_prefix("anthropic/claude-")
        .or_else(|| normalized.strip_prefix("claude-"))
    else {
        return false;
    };
    ["sonnet-", "opus-", "haiku-", "fable-"]
        .iter()
        .any(|prefix| {
            tail.strip_prefix(prefix)
                .is_some_and(|rest| !rest.is_empty())
        })
}

fn merge_meta(path: &Path) -> Result<(), String> {
    let mut value = read_json_object(path)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| "Claude Desktop configLibrary metadata must be an object".to_string())?;
    let mut entries = match object.get("entries") {
        Some(Value::Array(entries)) => entries.clone(),
        Some(_) => return Err("Claude Desktop configLibrary entries must be an array".into()),
        None => Vec::new(),
    };
    entries.retain(|entry| entry.get("id").and_then(Value::as_str) != Some(PROFILE_ID));
    entries.push(json!({ "id": PROFILE_ID, "name": PROFILE_NAME }));
    object.insert("entries".into(), Value::Array(entries));
    object.insert("appliedId".into(), Value::String(PROFILE_ID.into()));
    write_json_atomic(path, &value)
}

fn write_json_atomic(path: &Path, value: &Value) -> Result<(), String> {
    let _ = read_json_object(path)?;
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    replace_atomically(path, &bytes)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn read_json_object(path: &Path) -> Result<Value, String> {
    match fs::metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(format!(
                    "refusing non-regular Claude Desktop config {}",
                    path.display()
                ));
            }
            if metadata.len() > MAX_JSON_BYTES {
                return Err(format!(
                    "Claude Desktop config is too large: {}",
                    path.display()
                ));
            }
            let bytes = fs::read(path).map_err(|error| error.to_string())?;
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|error| format!("Claude Desktop config is malformed: {error}"))?;
            if !value.is_object() {
                return Err("Claude Desktop config must be a JSON object".into());
            }
            Ok(value)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(error.to_string()),
    }
}

fn default_executable() -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        let system = PathBuf::from("/Applications/Claude.app/Contents/MacOS/Claude");
        if system.is_file() {
            return Ok(system);
        }
        let home = directories::BaseDirs::new()
            .ok_or_else(|| "unable to resolve the user home directory".to_string())?;
        let user = home
            .home_dir()
            .join("Applications/Claude.app/Contents/MacOS/Claude");
        if user.is_file() {
            return Ok(user);
        }
    }
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let local = PathBuf::from(local);
            for candidate in [
                local.join("AnthropicClaude/claude.exe"),
                local.join("Programs/Claude/Claude.exe"),
            ] {
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }
    Err("Claude Desktop executable was not found; configure its absolute path in Settings".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use yaat_contracts::{ProfileStatus, ProviderProfile};

    fn profile(kind: ProviderKind, model: Option<&str>) -> ProviderProfile {
        ProviderProfile {
            id: "desktop-profile".into(),
            platform: Platform::ClaudeDesktop,
            kind,
            name: "Desktop".into(),
            base_url: (kind == ProviderKind::ThirdParty)
                .then(|| "https://gateway.example.com".into()),
            model: model.map(str::to_owned),
            custom_headers: Vec::new(),
            user_agent: None,
            platform_config: ProviderPlatformConfig::ClaudeDesktop {
                models: model.into_iter().map(str::to_owned).collect(),
            },
            secret_kind: if kind == ProviderKind::OfficialSubscription {
                SecretKind::None
            } else {
                SecretKind::ApiKey
            },
            has_secret: kind != ProviderKind::OfficialSubscription,
            status: ProfileStatus::Ready,
            ..Default::default()
        }
    }

    fn context(temp: &Path) -> AdapterContext {
        AdapterContext {
            data_root: temp.join("data"),
            explicit_cli_path: None,
            explicit_config_root: None,
        }
    }

    #[test]
    fn official_profile_changes_only_deployment_mode() {
        let temp = tempfile::tempdir().unwrap();
        let context = context(temp.path());
        let profile = profile(ProviderKind::OfficialSubscription, None);
        let root = context
            .data_root
            .join("profiles/claude_desktop/desktop-profile/home");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join(CONFIG_FILE),
            br#"{"theme":"keep","deploymentMode":"3p"}"#,
        )
        .unwrap();
        ClaudeDesktopAdapter::new()
            .prepare_profile(
                &context,
                ProfileRuntime {
                    profile: &profile,
                    secret: None,
                },
            )
            .unwrap();
        let value: Value =
            serde_json::from_slice(&fs::read(root.join(CONFIG_FILE)).unwrap()).unwrap();
        assert_eq!(value["deploymentMode"], "1p");
        assert_eq!(value["theme"], "keep");
    }

    #[test]
    fn gateway_profile_writes_direct_credential_and_preserves_other_library_entries() {
        let temp = tempfile::tempdir().unwrap();
        let context = context(temp.path());
        let profile = profile(ProviderKind::ThirdParty, Some("claude-sonnet-5"));
        let root = context
            .data_root
            .join("profiles/claude_desktop/desktop-profile/home");
        let library = root.join(CONFIG_LIBRARY_DIR);
        fs::create_dir_all(&library).unwrap();
        fs::write(
            library.join("_meta.json"),
            br#"{"entries":[{"id":"other","name":"Keep"}],"unowned":true}"#,
        )
        .unwrap();
        ClaudeDesktopAdapter::new()
            .prepare_profile(
                &context,
                ProfileRuntime {
                    profile: &profile,
                    secret: Some("test-secret"),
                },
            )
            .unwrap();
        let written: Value =
            serde_json::from_slice(&fs::read(library.join(format!("{PROFILE_ID}.json"))).unwrap())
                .unwrap();
        assert_eq!(written["inferenceGatewayApiKey"], "test-secret");
        assert_eq!(written["inferenceGatewayAuthScheme"], "x-api-key");
        assert!(written.get("inferenceCredentialHelper").is_none());
        let meta: Value =
            serde_json::from_slice(&fs::read(library.join("_meta.json")).unwrap()).unwrap();
        assert_eq!(meta["unowned"], true);
        assert_eq!(meta["entries"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn global_gateway_restore_preserves_entries_added_while_managed() {
        let temp = tempfile::tempdir().unwrap();
        let context = context(temp.path());
        let config_root = temp.path().join("Claude");
        let gateway_root = global_gateway_root(&config_root).unwrap();
        let library = gateway_root.join(CONFIG_LIBRARY_DIR);
        fs::create_dir_all(&library).unwrap();
        fs::write(
            library.join("_meta.json"),
            br#"{"entries":[{"id":"other","name":"Other"}],"appliedId":"other","keep":true}"#,
        )
        .unwrap();
        let snapshot = capture_gateway_state(&config_root).unwrap();

        fs::write(
            library.join("_meta.json"),
            format!(
                r#"{{"entries":[{{"id":"other","name":"Other"}},{{"id":"new","name":"New"}},{{"id":"{PROFILE_ID}","name":"YAAT"}}],"appliedId":"{PROFILE_ID}","keep":true}}"#
            ),
        )
        .unwrap();
        fs::write(library.join(format!("{PROFILE_ID}.json")), b"{}").unwrap();
        ClaudeDesktopAdapter::new()
            .restore_credentials(&context, &config_root, &snapshot)
            .unwrap();

        let meta: Value =
            serde_json::from_slice(&fs::read(library.join("_meta.json")).unwrap()).unwrap();
        let ids = meta["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["id"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["other", "new"]);
        assert_eq!(meta["appliedId"], "other");
        assert_eq!(meta["keep"], true);
        assert!(!library.join(format!("{PROFILE_ID}.json")).exists());
    }

    #[test]
    fn unsafe_model_requires_a_mapping_proxy() {
        assert!(!is_safe_direct_model("gpt-5"));
        assert!(!is_safe_direct_model("claude-3-5-sonnet"));
        assert!(is_safe_direct_model("claude-opus-5"));
        assert!(is_safe_direct_model("anthropic/claude-haiku-4-5"));
    }
}
