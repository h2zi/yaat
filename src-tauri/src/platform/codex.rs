// Codex configuration, launch, and credential-slot integration.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use directories::BaseDirs;
use serde::Deserialize;
use toml_edit::{DocumentMut, Item, Table, value};
use url::Url;
use uuid::Uuid;
use yaat_contracts::{Platform, ProviderKind, SecretKind};
use zeroize::{Zeroize, Zeroizing};

use crate::activation::{ConfigFormat, OwnedPath, PatchOperation};

use super::codex_credentials;
use super::{
    AdapterContext, CommandSpec, CredentialSnapshot, CredentialState, GlobalConfigPlan,
    PlatformAdapter, ProfileRuntime,
};

const CODEX_HOME_ENV: &str = "CODEX_HOME";
#[cfg(test)]
const AUTH_FILE_NAME: &str = "auth.json";
const CONFIG_FILE_NAME: &str = "config.toml";
const MANAGED_PROVIDER_ID: &str = crate::history::CODEX_HISTORY_PROVIDER_ID;
const CREDENTIAL_STORAGE_KIND: &str = "codex_auth_json_v1";
const OPENAI_API_BASE_URL: &str = "https://api.openai.com/v1";
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_AUTH_BYTES: u64 = 1024 * 1024;
const VERSION_TIMEOUT: Duration = Duration::from_secs(3);

/// TOML paths that the Codex adapter may change in a managed, derived config.
///
/// The adapter never patches the user's source `config.toml`. It copies that
/// document to the profile home and changes only these account/provider paths.
/// In particular, it owns one child of `model_providers`, not the whole table.
pub const OWNED_TOML_PATHS: &[&str] = &[
    "model",
    "model_provider",
    "profile",
    "openai_base_url",
    "chatgpt_base_url",
    "base_url",
    "wire_api",
    "experimental_bearer_token",
    "cli_auth_credentials_store",
    "model_providers.yaat_managed_v1",
];

const ACCOUNT_ENV_VARS: &[&str] = &[
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "CODEX_ACCESS_TOKEN",
    "OPENAI_BASE_URL",
    "OPENAI_ORGANIZATION",
    "OPENAI_PROJECT",
];

#[derive(Clone, Copy, Debug, Default)]
pub struct CodexAdapter;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialIdentity {
    pub auth_mode: String,
    pub account_label: Option<String>,
}

#[derive(Deserialize, Zeroize)]
struct AuthBundleV1 {
    #[serde(default)]
    auth_mode: Option<String>,
    #[serde(rename = "OPENAI_API_KEY", default)]
    openai_api_key: Option<String>,
    #[serde(default)]
    tokens: Option<TokenBundleV1>,
}

#[derive(Deserialize, Zeroize)]
struct TokenBundleV1 {
    id_token: String,
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Default, Deserialize, Zeroize)]
struct JwtIdentityClaims {
    #[serde(default)]
    email: Option<String>,
    #[serde(rename = "https://api.openai.com/profile", default)]
    profile: Option<JwtProfileClaims>,
}

#[derive(Default, Deserialize, Zeroize)]
struct JwtProfileClaims {
    #[serde(default)]
    email: Option<String>,
}

impl CodexAdapter {
    pub const fn new() -> Self {
        Self
    }

    /// Resolve the source Codex home using Codex CLI semantics. An explicit
    /// app setting wins, then non-empty `CODEX_HOME`, then `~/.codex`.
    pub fn config_root(&self, context: &AdapterContext) -> Result<PathBuf, String> {
        if let Some(path) = context.explicit_config_root.as_ref() {
            return validate_explicit_root(path, "configured Codex home");
        }

        if let Some(value) = std::env::var_os(CODEX_HOME_ENV).filter(|value| !value.is_empty()) {
            return validate_codex_home_env(PathBuf::from(value));
        }

        let base = BaseDirs::new()
            .ok_or_else(|| "unable to resolve the user home directory".to_string())?;
        Ok(base.home_dir().join(".codex"))
    }

    /// Parse an opaque Codex auth payload without returning
    /// token or key material. Unknown top-level fields are accepted because
    /// switching preserves them; account-owned fields remain strictly checked.
    pub fn inspect_credential_payload(payload: &[u8]) -> Result<CredentialIdentity, String> {
        if payload.is_empty() {
            return Err("Codex auth.json is empty".into());
        }
        if payload.len() as u64 > MAX_AUTH_BYTES {
            return Err(format!(
                "Codex auth.json exceeds the supported {MAX_AUTH_BYTES} byte limit"
            ));
        }

        let mut bundle: AuthBundleV1 = serde_json::from_slice(payload).map_err(|error| {
            format!("unsupported Codex auth.json v1 shape; refusing credential operation: {error}")
        })?;

        let result = inspect_auth_bundle(&bundle);
        bundle.zeroize();
        result
    }

    /// Verify that the current file or keyring credential slot has the same
    /// account identity and account-owned fields as a snapshot. Unrelated
    /// fields deliberately remain live and are excluded from this comparison.
    pub fn verify_credentials(
        &self,
        context: &AdapterContext,
        config_root: &Path,
        snapshot: &CredentialSnapshot,
    ) -> Result<CredentialIdentity, String> {
        validate_snapshot_kind(snapshot)?;
        let current = codex_credentials::load(context, config_root)?;
        let identity = Self::inspect_credential_payload(&current)?;
        if !codex_credentials::account_fields_match(&current, &snapshot.opaque_payload)? {
            return Err("Codex credential payload verification failed".into());
        }
        Ok(identity)
    }

    fn managed_profile_home(&self, profile_id: &str) -> Result<PathBuf, String> {
        crate::paths::managed_profile_home(Platform::Codex, profile_id)
            .map_err(|error| error.to_string())
    }

    fn derive_profile_config(
        &self,
        source: &str,
        runtime: &ProfileRuntime<'_>,
        helper_executable: &Path,
    ) -> Result<String, String> {
        validate_runtime(runtime, helper_executable)?;
        derive_profile_config(source, runtime, helper_executable)
    }
}

impl PlatformAdapter for CodexAdapter {
    fn discover_cli(&self, context: &AdapterContext) -> Result<(PathBuf, String), String> {
        let path = resolve_cli_path(context)?;
        let version = read_cli_version(&path)?;
        Ok((path, version))
    }

    fn prepare_profile(
        &self,
        context: &AdapterContext,
        runtime: ProfileRuntime<'_>,
    ) -> Result<PathBuf, String> {
        validate_runtime(&runtime, &context.helper_executable)?;
        let source_root = self.config_root(context)?;
        let profile_home = self.managed_profile_home(&runtime.profile.id)?;
        crate::paths::ensure_private_directory(&profile_home)
            .map_err(|error| format!("failed to create Codex profile home: {error}"))?;

        if paths_refer_to_same_location(&source_root, &profile_home)? {
            return Err("managed Codex profile home must differ from the source Codex home".into());
        }

        let target_path = profile_home.join(CONFIG_FILE_NAME);
        let source_path = source_root.join(CONFIG_FILE_NAME);
        let base = read_profile_config_base(&source_path, &target_path)?;
        let derived = self.derive_profile_config(&base, &runtime, &context.helper_executable)?;
        atomic_write_private(&target_path, derived.as_bytes())?;
        Ok(profile_home)
    }

    fn login_spec(
        &self,
        context: &AdapterContext,
        runtime: ProfileRuntime<'_>,
        console: bool,
    ) -> Result<CommandSpec, String> {
        if runtime.profile.kind != ProviderKind::OfficialSubscription {
            return Err(
                "Codex API-key and third-party profiles use the YAAT credential helper and do not require `codex login`"
                    .into(),
            );
        }
        let profile_home = self.prepare_profile(context, runtime)?;
        let (program, _) = self.discover_cli(context)?;
        let mut args = vec!["login".into()];
        if console {
            args.push("--device-auth".into());
        }
        Ok(codex_command_spec(program, args, &profile_home, None))
    }

    fn launch_spec(
        &self,
        context: &AdapterContext,
        runtime: ProfileRuntime<'_>,
        cwd: Option<PathBuf>,
        passthrough_args: Vec<String>,
    ) -> Result<CommandSpec, String> {
        let profile_home = self.prepare_profile(context, runtime)?;
        let (program, _) = self.discover_cli(context)?;
        Ok(codex_command_spec(
            program,
            passthrough_args,
            &profile_home,
            cwd,
        ))
    }

    fn capture_credentials(
        &self,
        context: &AdapterContext,
        config_root: &Path,
    ) -> Result<CredentialSnapshot, String> {
        let payload = codex_credentials::load(context, config_root)?;
        let identity = Self::inspect_credential_payload(&payload)?;
        Ok(CredentialSnapshot {
            storage_kind: CREDENTIAL_STORAGE_KIND.into(),
            opaque_payload: payload,
            account_label: identity.account_label,
            warning: None,
        })
    }

    fn capture_credential_state(
        &self,
        context: &AdapterContext,
        config_root: &Path,
    ) -> Result<CredentialState, String> {
        let Some(payload) = codex_credentials::load_optional(context, config_root)? else {
            return Ok(CredentialState::Absent);
        };
        if !codex_credentials::account_fields_present(&payload)? {
            return Ok(CredentialState::Absent);
        }
        let identity = Self::inspect_credential_payload(&payload)?;
        Ok(CredentialState::Present(CredentialSnapshot {
            storage_kind: CREDENTIAL_STORAGE_KIND.into(),
            opaque_payload: payload,
            account_label: identity.account_label,
            warning: None,
        }))
    }

    fn restore_credentials(
        &self,
        context: &AdapterContext,
        config_root: &Path,
        snapshot: &CredentialSnapshot,
    ) -> Result<(), String> {
        validate_snapshot_kind(snapshot)?;
        Self::inspect_credential_payload(&snapshot.opaque_payload)?;

        codex_credentials::replace(context, config_root, &snapshot.opaque_payload)?;
        self.verify_credentials(context, config_root, snapshot)
            .map(|_| ())
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
                codex_credentials::clear_account_fields(context, config_root)
            }
        }
    }

    fn global_config_plan(
        &self,
        context: &AdapterContext,
        runtime: ProfileRuntime<'_>,
    ) -> Result<GlobalConfigPlan, String> {
        validate_runtime(&runtime, &context.helper_executable)?;
        let config_root = self.config_root(context)?;
        ensure_existing_directory(&config_root, "Codex home")?;

        let profile = runtime.profile;
        let mut operations = Vec::with_capacity(OWNED_TOML_PATHS.len());
        let path = |value: &str| {
            OwnedPath::from_segments(value.split('.')).map_err(|error| error.to_string())
        };
        let set_or_remove = |operations: &mut Vec<PatchOperation>,
                             key: &str,
                             value: Option<serde_json::Value>|
         -> Result<(), String> {
            let owned = path(key)?;
            operations.push(match value {
                Some(value) => PatchOperation::set(owned, value),
                None => PatchOperation::remove(owned),
            });
            Ok(())
        };

        set_or_remove(
            &mut operations,
            "model",
            profile
                .model
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| serde_json::Value::String(value.to_owned())),
        )?;
        set_or_remove(
            &mut operations,
            "model_provider",
            Some(serde_json::Value::String(MANAGED_PROVIDER_ID.into())),
        )?;
        for key in [
            "profile",
            "openai_base_url",
            "chatgpt_base_url",
            "base_url",
            "wire_api",
            "experimental_bearer_token",
        ] {
            set_or_remove(&mut operations, key, None)?;
        }
        let provider = match profile.kind {
            ProviderKind::OfficialSubscription => {
                let mut provider = serde_json::Map::new();
                provider.insert("name".into(), serde_json::Value::String("OpenAI".into()));
                provider.insert(
                    "wire_api".into(),
                    serde_json::Value::String("responses".into()),
                );
                provider.insert("requires_openai_auth".into(), serde_json::Value::Bool(true));
                provider.insert("supports_websockets".into(), serde_json::Value::Bool(true));
                provider.insert(
                    "supports_standalone_web_search".into(),
                    serde_json::Value::Bool(true),
                );
                Some(serde_json::Value::Object(provider))
            }
            ProviderKind::OfficialApi | ProviderKind::ThirdParty => {
                let raw_base_url = match profile.kind {
                    ProviderKind::OfficialApi => {
                        profile.base_url.as_deref().unwrap_or(OPENAI_API_BASE_URL)
                    }
                    ProviderKind::ThirdParty => profile.base_url.as_deref().ok_or_else(|| {
                        "third-party Codex profile is missing base_url".to_string()
                    })?,
                    ProviderKind::OfficialSubscription => unreachable!(),
                };
                let base_url = validate_base_url(raw_base_url)?;
                let mut provider = serde_json::Map::new();
                provider.insert(
                    "name".into(),
                    serde_json::Value::String(
                        if profile.kind == ProviderKind::OfficialApi {
                            "OpenAI"
                        } else {
                            profile.name.trim()
                        }
                        .into(),
                    ),
                );
                provider.insert("base_url".into(), serde_json::Value::String(base_url));
                provider.insert(
                    "wire_api".into(),
                    serde_json::Value::String("responses".into()),
                );
                provider.insert(
                    "requires_openai_auth".into(),
                    serde_json::Value::Bool(false),
                );
                if profile.kind == ProviderKind::OfficialApi {
                    provider.insert("supports_websockets".into(), serde_json::Value::Bool(true));
                    provider.insert(
                        "supports_standalone_web_search".into(),
                        serde_json::Value::Bool(true),
                    );
                }
                if profile.secret_kind != SecretKind::None {
                    let helper = context.helper_executable.to_str().ok_or_else(|| {
                        "YAAT credential helper path is not valid UTF-8".to_string()
                    })?;
                    let secret_ref = runtime
                        .secret_ref
                        .ok_or_else(|| "YAAT credential reference is unavailable".to_string())?;
                    provider.insert(
                        "auth".into(),
                        serde_json::json!({
                            "command": helper,
                            "args": ["--yaat-credential-helper", "codex", secret_ref]
                        }),
                    );
                }
                Some(serde_json::Value::Object(provider))
            }
        };
        set_or_remove(&mut operations, "model_providers.yaat_managed_v1", provider)?;

        Ok(GlobalConfigPlan {
            path: config_root.join(CONFIG_FILE_NAME),
            format: ConfigFormat::Toml,
            operations,
        })
    }
}

fn read_profile_config_base(source_path: &Path, target_path: &Path) -> Result<String, String> {
    if target_path.exists() {
        read_optional_config(target_path)
    } else {
        read_optional_config(source_path)
    }
}

fn validate_runtime(runtime: &ProfileRuntime<'_>, helper_executable: &Path) -> Result<(), String> {
    let profile = runtime.profile;
    if profile.platform != Platform::Codex {
        return Err("Codex adapter received a profile for another platform".into());
    }
    crate::paths::validate_identifier(&profile.id).map_err(|error| error.to_string())?;
    if profile.name.trim().is_empty() {
        return Err("Codex profile name must not be empty".into());
    }

    let has_ref = runtime
        .secret_ref
        .is_some_and(|value| !value.trim().is_empty());
    if runtime.secret_ref.is_some_and(|value| {
        value.is_empty()
            || value.len() > 512
            || value.contains('\0')
            || value.contains(['\r', '\n'])
    }) {
        return Err("invalid YAAT credential reference".into());
    }

    match profile.kind {
        ProviderKind::OfficialSubscription => {
            if profile.secret_kind != SecretKind::None || profile.has_secret || has_ref {
                return Err(
                    "official Codex subscription profiles must authenticate with isolated `codex login`, not a stored API secret"
                        .into(),
                );
            }
            if profile
                .base_url
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            {
                return Err("official Codex subscription profiles cannot override base_url".into());
            }
        }
        ProviderKind::OfficialApi => {
            if !matches!(
                profile.secret_kind,
                SecretKind::ApiKey | SecretKind::BearerToken
            ) || !profile.has_secret
                || !has_ref
            {
                return Err(
                    "official Codex API profiles require a YAAT credential reference".into(),
                );
            }
            validate_helper_executable(helper_executable)?;
            validate_base_url(profile.base_url.as_deref().unwrap_or(OPENAI_API_BASE_URL))?;
        }
        ProviderKind::ThirdParty => {
            let base_url = profile
                .base_url
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| "third-party Codex profiles require base_url".to_string())?;
            validate_base_url(base_url)?;
            match profile.secret_kind {
                SecretKind::None if profile.has_secret || has_ref => {
                    return Err("a no-auth Codex profile cannot carry a secret reference".into());
                }
                SecretKind::None => {}
                SecretKind::ApiKey | SecretKind::BearerToken if profile.has_secret && has_ref => {
                    validate_helper_executable(helper_executable)?;
                }
                SecretKind::ApiKey | SecretKind::BearerToken => {
                    return Err("third-party Codex profile secret is unavailable".into());
                }
            }
        }
    }
    Ok(())
}

fn validate_helper_executable(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("YAAT credential helper path must be absolute".into());
    }
    let metadata = fs::metadata(path).map_err(|error| {
        format!(
            "YAAT credential helper {} is unavailable: {error}",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "YAAT credential helper {} is not a file",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "YAAT credential helper {} is not executable",
                path.display()
            ));
        }
    }
    Ok(())
}

fn validate_base_url(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    let parsed = Url::parse(raw).map_err(|error| format!("invalid Codex base_url: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("Codex base_url must be an absolute http(s) URL with a host".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("Codex base_url must not contain credentials".into());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("Codex base_url must not contain a query or fragment".into());
    }
    Ok(raw.trim_end_matches('/').to_string())
}

fn derive_profile_config(
    source: &str,
    runtime: &ProfileRuntime<'_>,
    helper_executable: &Path,
) -> Result<String, String> {
    let profile = runtime.profile;
    let mut doc = if source.trim().is_empty() {
        DocumentMut::new()
    } else {
        source
            .parse::<DocumentMut>()
            .map_err(|error| format!("source Codex config.toml is malformed: {error}"))?
    };

    for key in [
        "model",
        "model_provider",
        "profile",
        "openai_base_url",
        "chatgpt_base_url",
        "base_url",
        "wire_api",
        "experimental_bearer_token",
        "cli_auth_credentials_store",
    ] {
        doc.as_table_mut().remove(key);
    }
    if let Some(providers) = doc
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
    {
        providers.remove(MANAGED_PROVIDER_ID);
    } else if doc.get("model_providers").is_some() {
        return Err("source Codex model_providers must be a table".into());
    }

    // File storage is deliberate: each managed CODEX_HOME gets its own
    // auth.json and YAAT never needs to read a private keyring entry.
    doc["cli_auth_credentials_store"] = value("file");
    if let Some(model) = profile
        .model
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        doc["model"] = value(model);
    }

    doc["model_provider"] = value(MANAGED_PROVIDER_ID);
    match profile.kind {
        ProviderKind::OfficialSubscription => {
            let mut provider = Table::new();
            provider["name"] = value("OpenAI");
            provider["wire_api"] = value("responses");
            provider["requires_openai_auth"] = value(true);
            provider["supports_websockets"] = value(true);
            provider["supports_standalone_web_search"] = value(true);
            if doc.get("model_providers").is_none() {
                let mut providers = Table::new();
                providers.set_implicit(true);
                doc["model_providers"] = Item::Table(providers);
            }
            let providers = doc
                .get_mut("model_providers")
                .and_then(Item::as_table_like_mut)
                .ok_or_else(|| "source Codex model_providers must be a table".to_string())?;
            providers.insert(MANAGED_PROVIDER_ID, Item::Table(provider));
        }
        ProviderKind::OfficialApi | ProviderKind::ThirdParty => {
            let base_url = match profile.kind {
                ProviderKind::OfficialApi => {
                    profile.base_url.as_deref().unwrap_or(OPENAI_API_BASE_URL)
                }
                ProviderKind::ThirdParty => profile
                    .base_url
                    .as_deref()
                    .ok_or_else(|| "third-party Codex profile is missing base_url".to_string())?,
                ProviderKind::OfficialSubscription => unreachable!(),
            };
            let base_url = validate_base_url(base_url)?;

            let mut provider = Table::new();
            provider["name"] = value(if profile.kind == ProviderKind::OfficialApi {
                "OpenAI"
            } else {
                profile.name.trim()
            });
            provider["base_url"] = value(base_url);
            provider["wire_api"] = value("responses");
            provider["requires_openai_auth"] = value(false);
            if profile.kind == ProviderKind::OfficialApi {
                provider["supports_websockets"] = value(true);
                provider["supports_standalone_web_search"] = value(true);
            }

            if profile.secret_kind != SecretKind::None {
                let helper = helper_executable.to_str().ok_or_else(|| {
                    "YAAT credential helper path is not valid UTF-8 and cannot be written to Codex TOML"
                        .to_string()
                })?;
                let secret_ref = runtime
                    .secret_ref
                    .ok_or_else(|| "YAAT credential reference is unavailable".to_string())?;
                let mut auth = Table::new();
                auth["command"] = value(helper);
                let mut args = toml_edit::Array::new();
                args.push("--yaat-credential-helper");
                args.push("codex");
                args.push(secret_ref);
                auth["args"] = value(args);
                provider["auth"] = Item::Table(auth);
            }

            if doc.get("model_providers").is_none() {
                let mut providers = Table::new();
                providers.set_implicit(true);
                doc["model_providers"] = Item::Table(providers);
            }
            let providers = doc
                .get_mut("model_providers")
                .and_then(Item::as_table_like_mut)
                .ok_or_else(|| "source Codex model_providers must be a table".to_string())?;
            providers.insert(MANAGED_PROVIDER_ID, Item::Table(provider));
        }
    }

    Ok(doc.to_string())
}

fn codex_command_spec(
    program: PathBuf,
    args: Vec<String>,
    profile_home: &Path,
    cwd: Option<PathBuf>,
) -> CommandSpec {
    let mut env = BTreeMap::new();
    env.insert(
        CODEX_HOME_ENV.into(),
        profile_home.to_string_lossy().into_owned(),
    );
    CommandSpec {
        program,
        args,
        env,
        env_remove: ACCOUNT_ENV_VARS.iter().map(|name| (*name).into()).collect(),
        cwd,
    }
}

fn resolve_cli_path(context: &AdapterContext) -> Result<PathBuf, String> {
    let path = if let Some(explicit) = context.explicit_cli_path.as_ref() {
        if explicit.components().count() == 1 {
            which::which(explicit).map_err(|error| {
                format!(
                    "configured Codex CLI `{}` was not found: {error}",
                    explicit.display()
                )
            })?
        } else {
            explicit.clone()
        }
    } else {
        which::which("codex")
            .map_err(|error| format!("Codex CLI was not found in PATH: {error}"))?
    };

    let metadata = fs::metadata(&path)
        .map_err(|error| format!("Codex CLI {} is unavailable: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("Codex CLI {} is not a file", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!("Codex CLI {} is not executable", path.display()));
        }
    }
    path.canonicalize()
        .map_err(|error| format!("failed to resolve Codex CLI {}: {error}", path.display()))
}

fn read_cli_version(path: &Path) -> Result<String, String> {
    let (status, stdout, stderr) =
        crate::process::run_with_timeout(path, &["--version"], VERSION_TIMEOUT)?;
    if !status.success() {
        let detail = String::from_utf8_lossy(&stderr).trim().to_string();
        return Err(if detail.is_empty() {
            format!("`{} --version` exited with {status}", path.display())
        } else {
            format!("`{} --version` failed: {detail}", path.display())
        });
    }
    let raw_version = if stdout.is_empty() { stderr } else { stdout };
    let version = String::from_utf8(raw_version)
        .map_err(|_| "Codex CLI version output was not UTF-8".to_string())?;
    let version = version.trim();
    if version.is_empty() || version.len() > 256 || version.lines().count() != 1 {
        return Err("Codex CLI returned an invalid version string".into());
    }
    Ok(version.to_string())
}

fn validate_explicit_root(path: &Path, label: &str) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if path.exists() {
        ensure_existing_directory(path, label)?;
        path.canonicalize()
            .map_err(|error| format!("failed to resolve {label} {}: {error}", path.display()))
    } else if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Err(format!("{label} must be an absolute path"))
    }
}

fn validate_codex_home_env(path: PathBuf) -> Result<PathBuf, String> {
    let metadata = fs::metadata(&path).map_err(|error| {
        format!(
            "CODEX_HOME points to {}, but it is unavailable: {error}",
            path.display()
        )
    })?;
    if !metadata.is_dir() {
        return Err(format!(
            "CODEX_HOME points to {}, but it is not a directory",
            path.display()
        ));
    }
    path.canonicalize()
        .map_err(|error| format!("failed to resolve CODEX_HOME {}: {error}", path.display()))
}

fn ensure_existing_directory(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("{label} {} is unavailable: {error}", path.display()))?;
    if !metadata.is_dir() {
        return Err(format!("{label} {} is not a directory", path.display()));
    }
    Ok(())
}

fn paths_refer_to_same_location(left: &Path, right: &Path) -> Result<bool, String> {
    if left == right {
        return Ok(true);
    }
    if left.exists() && right.exists() {
        let left = left
            .canonicalize()
            .map_err(|error| format!("failed to resolve {}: {error}", left.display()))?;
        let right = right
            .canonicalize()
            .map_err(|error| format!("failed to resolve {}: {error}", right.display()))?;
        return Ok(left == right);
    }
    Ok(false)
}

fn read_optional_config(path: &Path) -> Result<String, String> {
    match fs::metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(format!("Codex config {} is not a file", path.display()));
            }
            if metadata.len() > MAX_CONFIG_BYTES {
                return Err(format!(
                    "Codex config {} exceeds the supported {} byte limit",
                    path.display(),
                    MAX_CONFIG_BYTES
                ));
            }
            let bytes = fs::read(path).map_err(|error| {
                format!("failed to read Codex config {}: {error}", path.display())
            })?;
            String::from_utf8(bytes)
                .map_err(|_| format!("Codex config {} is not valid UTF-8", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!(
            "failed to inspect Codex config {}: {error}",
            path.display()
        )),
    }
}

fn inspect_auth_bundle(bundle: &AuthBundleV1) -> Result<CredentialIdentity, String> {
    let has_api_key = bundle
        .openai_api_key
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    let has_tokens = bundle.tokens.is_some();
    if has_api_key == has_tokens {
        return Err(
            "Codex auth.json must contain exactly one supported credential mode (API key or ChatGPT tokens)"
                .into(),
        );
    }

    let inferred_mode = if has_api_key { "apikey" } else { "chatgpt" };
    if let Some(mode) = bundle.auth_mode.as_deref()
        && mode != inferred_mode
    {
        return Err(format!(
            "unsupported or inconsistent Codex auth mode `{mode}`; YAAT currently supports apikey and chatgpt auth.json bundles"
        ));
    }

    match inferred_mode {
        "apikey" => Ok(CredentialIdentity {
            auth_mode: "apikey".into(),
            account_label: None,
        }),
        "chatgpt" => inspect_chatgpt_tokens(bundle.tokens.as_ref().expect("checked above")),
        _ => unreachable!(),
    }
}

fn inspect_chatgpt_tokens(tokens: &TokenBundleV1) -> Result<CredentialIdentity, String> {
    if tokens.id_token.trim().is_empty()
        || tokens.access_token.trim().is_empty()
        || tokens.refresh_token.trim().is_empty()
    {
        return Err("Codex ChatGPT auth bundle contains an empty token".into());
    }
    let mut claims = decode_jwt_identity(&tokens.id_token)?;
    let email = claims
        .email
        .as_deref()
        .or_else(|| {
            claims
                .profile
                .as_ref()
                .and_then(|profile| profile.email.as_deref())
        })
        .map(str::to_string);
    let account_label = email.or_else(|| tokens.account_id.clone());
    claims.zeroize();
    Ok(CredentialIdentity {
        auth_mode: "chatgpt".into(),
        account_label,
    })
}

fn decode_jwt_identity(jwt: &str) -> Result<JwtIdentityClaims, String> {
    let mut segments = jwt.split('.');
    let Some(_header) = segments.next() else {
        return Err("Codex id_token is not a JWT".into());
    };
    let Some(payload) = segments.next() else {
        return Err("Codex id_token is not a JWT".into());
    };
    let Some(_signature) = segments.next() else {
        return Err("Codex id_token is not a JWT".into());
    };
    if segments.next().is_some() || payload.is_empty() {
        return Err("Codex id_token is not a supported JWT".into());
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .map_err(|_| "Codex id_token has an invalid JWT payload".to_string())?;
    let decoded = Zeroizing::new(decoded);
    serde_json::from_slice(&decoded)
        .map_err(|_| "Codex id_token has an unsupported identity payload".to_string())
}

fn validate_snapshot_kind(snapshot: &CredentialSnapshot) -> Result<(), String> {
    if snapshot.storage_kind != CREDENTIAL_STORAGE_KIND {
        return Err(format!(
            "unsupported Codex credential snapshot kind `{}`",
            snapshot.storage_kind
        ));
    }
    Ok(())
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let resolved_path = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(path)
            .map_err(|error| format!("failed to resolve {}: {error}", path.display()))?,
        _ => path.to_path_buf(),
    };
    let path = resolved_path.as_path();
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    ensure_existing_directory(parent, "destination directory")?;

    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(format!("destination {} is not a file", path.display()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "failed to inspect destination {}: {error}",
                path.display()
            ));
        }
    }

    let temp_path = parent.join(format!(
        ".{}.yaat-{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("codex-config"),
        Uuid::new_v4()
    ));
    let write_result = (|| -> Result<(), String> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp_path).map_err(|error| {
            format!(
                "failed to create temporary file {}: {error}",
                temp_path.display()
            )
        })?;
        file.write_all(bytes).map_err(|error| {
            format!(
                "failed to write temporary file {}: {error}",
                temp_path.display()
            )
        })?;
        file.sync_all().map_err(|error| {
            format!(
                "failed to sync temporary file {}: {error}",
                temp_path.display()
            )
        })?;
        drop(file);
        replace_file(&temp_path, path)?;
        sync_parent_directory(parent)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<(), String> {
    fs::rename(source, destination).map_err(|error| {
        format!(
            "failed to atomically replace {}: {error}",
            destination.display()
        )
    })
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<(), String> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: `source` and `destination` are owned, NUL-terminated UTF-16
    // buffers that remain alive for the duration of this read-only FFI call.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(format!(
            "failed to atomically replace {}: {}",
            PathBuf::from(std::ffi::OsString::from_wide(
                &destination[..destination.len() - 1]
            ))
            .display(),
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<(), String> {
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("failed to sync directory {}: {error}", parent.display()))
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use base64::Engine;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use yaat_contracts::{ProfileStatus, ProviderProfile};

    use crate::activation::PatchEngine;

    use super::*;

    fn profile(kind: ProviderKind, secret_kind: SecretKind) -> ProviderProfile {
        ProviderProfile {
            id: "profile-1".into(),
            platform: Platform::Codex,
            kind,
            name: "Example Provider".into(),
            account_label: None,
            base_url: match kind {
                ProviderKind::OfficialSubscription => None,
                ProviderKind::OfficialApi => Some(OPENAI_API_BASE_URL.into()),
                ProviderKind::ThirdParty => Some("https://gateway.example/v1/".into()),
            },
            model: Some("gpt-example".into()),
            secret_kind,
            has_secret: secret_kind != SecretKind::None,
            profile_home: None,
            status: ProfileStatus::Ready,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn helper_file(temp: &TempDir) -> PathBuf {
        let path = temp.path().join("yaat-helper");
        fs::write(&path, "helper").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        path
    }

    fn fake_chatgpt_auth(email: &str, account_id: &str) -> Vec<u8> {
        let claims = serde_json::json!({
            "sub": "user-1",
            "email": email,
            "https://api.openai.com/auth": {
                "chatgpt_user_id": "user-1",
                "chatgpt_account_id": account_id
            }
        });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        serde_json::to_vec_pretty(&serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": format!("header.{payload}.signature"),
                "access_token": "access-secret",
                "refresh_token": "refresh-secret",
                "account_id": account_id
            },
            "last_refresh": "2026-08-02T01:02:03Z"
        }))
        .unwrap()
    }

    fn context(temp: &TempDir) -> AdapterContext {
        AdapterContext {
            app_data_dir: temp.path().join("app-data"),
            helper_executable: helper_file(temp),
            explicit_cli_path: None,
            explicit_config_root: None,
        }
    }

    #[test]
    fn owned_paths_never_claim_the_whole_model_provider_table() {
        assert!(OWNED_TOML_PATHS.contains(&"model_providers.yaat_managed_v1"));
        assert!(!OWNED_TOML_PATHS.contains(&"model_providers"));
        assert!(!OWNED_TOML_PATHS.contains(&"mcp_servers"));
    }

    #[test]
    fn global_plan_patches_only_codex_account_fields() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("codex-home");
        fs::create_dir(&root).unwrap();
        let source = r#"# keep this comment
approval_policy = "never"
model_provider = "old"
openai_base_url = "https://old.example/v1"
cli_auth_credentials_store = "keyring"

[mcp_servers.files]
command = "files-mcp"

[model_providers.personal]
name = "Personal"
base_url = "https://personal.example/v1"
wire_api = "responses"
"#;
        fs::write(root.join(CONFIG_FILE_NAME), source).unwrap();
        let profile = profile(ProviderKind::OfficialSubscription, SecretKind::None);
        let context = AdapterContext {
            app_data_dir: temp.path().join("data"),
            helper_executable: helper_file(&temp),
            explicit_cli_path: None,
            explicit_config_root: Some(root.clone()),
        };

        let plan = CodexAdapter
            .global_config_plan(
                &context,
                ProfileRuntime {
                    profile: &profile,
                    secret_ref: None,
                },
            )
            .unwrap();
        let receipt = PatchEngine::apply_file(&plan.path, plan.format, plan.operations).unwrap();
        let after = fs::read_to_string(&plan.path).unwrap();

        assert!(after.contains("# keep this comment"));
        assert!(after.contains("approval_policy = \"never\""));
        assert!(after.contains("[mcp_servers.files]"));
        assert!(after.contains("[model_providers.personal]"));
        assert!(after.contains("model_provider = \"yaat_managed_v1\""));
        let parsed = after.parse::<DocumentMut>().unwrap();
        assert_eq!(
            parsed["model_providers"][MANAGED_PROVIDER_ID]["requires_openai_auth"].as_bool(),
            Some(true)
        );
        assert!(after.contains("cli_auth_credentials_store = \"keyring\""));
        assert!(!after.contains("openai_base_url"));
        assert_eq!(
            receipt
                .changes()
                .iter()
                .map(|change| change.path.to_json_pointer())
                .collect::<Vec<_>>(),
            vec![
                "/model",
                "/model_provider",
                "/openai_base_url",
                "/model_providers/yaat_managed_v1"
            ]
        );
    }

    #[test]
    fn official_subscription_derivation_preserves_every_unowned_field() {
        let source = r#"# user comment
approval_policy = "on-request"
model = "old-model"
model_provider = "personal"
profile = "personal-overrides"
openai_base_url = "https://old.example/v1"
chatgpt_base_url = "https://unsafe.example/backend-api/codex"

[mcp_servers.files]
command = "files-mcp"

[model_providers.personal]
name = "Personal"
base_url = "https://personal.example/v1"
wire_api = "responses"
"#;
        let temp = TempDir::new().unwrap();
        let profile = profile(ProviderKind::OfficialSubscription, SecretKind::None);
        let runtime = ProfileRuntime {
            profile: &profile,
            secret_ref: None,
        };
        let derived = derive_profile_config(source, &runtime, &helper_file(&temp)).unwrap();
        let doc = derived.parse::<DocumentMut>().unwrap();

        assert_eq!(doc["approval_policy"].as_str(), Some("on-request"));
        assert_eq!(
            doc["mcp_servers"]["files"]["command"].as_str(),
            Some("files-mcp")
        );
        assert_eq!(
            doc["model_providers"]["personal"]["base_url"].as_str(),
            Some("https://personal.example/v1")
        );
        assert_eq!(doc["model"].as_str(), Some("gpt-example"));
        assert_eq!(doc["model_provider"].as_str(), Some(MANAGED_PROVIDER_ID));
        assert_eq!(
            doc["model_providers"][MANAGED_PROVIDER_ID]["requires_openai_auth"].as_bool(),
            Some(true)
        );
        assert_eq!(doc["cli_auth_credentials_store"].as_str(), Some("file"));
        assert!(doc.get("openai_base_url").is_none());
        assert!(doc.get("chatgpt_base_url").is_none());
        assert!(doc.get("profile").is_none());
        assert!(derived.contains("# user comment"));
    }

    #[test]
    fn existing_managed_profile_is_never_reseeded_from_global_config() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("global.toml");
        let target = temp.path().join("managed.toml");
        fs::write(&source, "[mcp_servers.global]\ncommand = 'global'\n").unwrap();
        fs::write(&target, "[mcp_servers.profile]\ncommand = 'profile'\n").unwrap();

        let base = read_profile_config_base(&source, &target).unwrap();

        assert!(base.contains("mcp_servers.profile"));
        assert!(!base.contains("mcp_servers.global"));
    }

    #[test]
    fn third_party_derivation_uses_native_responses_command_auth() {
        let source = r#"sandbox_mode = "workspace-write"
model_providers = { personal = { name = "Personal", base_url = "https://personal.example/v1", wire_api = "responses" } }
"#;
        let temp = TempDir::new().unwrap();
        let helper = helper_file(&temp);
        let profile = profile(ProviderKind::ThirdParty, SecretKind::BearerToken);
        let runtime = ProfileRuntime {
            profile: &profile,
            secret_ref: Some("credential-ref-123"),
        };
        let derived = derive_profile_config(source, &runtime, &helper).unwrap();
        let doc = derived.parse::<DocumentMut>().unwrap();
        let managed = &doc["model_providers"][MANAGED_PROVIDER_ID];

        assert_eq!(doc["sandbox_mode"].as_str(), Some("workspace-write"));
        assert_eq!(doc["model_provider"].as_str(), Some(MANAGED_PROVIDER_ID));
        assert_eq!(
            managed["base_url"].as_str(),
            Some("https://gateway.example/v1")
        );
        assert_eq!(managed["wire_api"].as_str(), Some("responses"));
        assert_eq!(managed["requires_openai_auth"].as_bool(), Some(false));
        assert_eq!(managed["auth"]["command"].as_str(), helper.to_str());
        assert_eq!(
            managed["auth"]["args"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|value| value.as_str())
                .collect::<Vec<_>>(),
            vec!["--yaat-credential-helper", "codex", "credential-ref-123"]
        );
        assert!(derived.contains("personal"));
        assert!(!derived.contains("access-secret"));
        assert!(!derived.contains("refresh-secret"));
    }

    #[test]
    fn no_auth_third_party_omits_auth_table() {
        let temp = TempDir::new().unwrap();
        let profile = profile(ProviderKind::ThirdParty, SecretKind::None);
        let runtime = ProfileRuntime {
            profile: &profile,
            secret_ref: None,
        };
        let derived = derive_profile_config("", &runtime, &helper_file(&temp)).unwrap();
        let doc = derived.parse::<DocumentMut>().unwrap();
        assert!(
            doc["model_providers"][MANAGED_PROVIDER_ID]
                .get("auth")
                .is_none()
        );
    }

    #[test]
    fn malformed_model_providers_fails_closed() {
        let temp = TempDir::new().unwrap();
        let profile = profile(ProviderKind::OfficialSubscription, SecretKind::None);
        let runtime = ProfileRuntime {
            profile: &profile,
            secret_ref: None,
        };
        let error = derive_profile_config(
            "model_providers = 42\napproval_policy = \"never\"\n",
            &runtime,
            &helper_file(&temp),
        )
        .unwrap_err();
        assert!(error.contains("model_providers must be a table"));
    }

    #[test]
    fn credential_identity_is_stable_across_token_refresh() {
        let first = fake_chatgpt_auth("person@example.com", "account-1");
        let mut second_value: serde_json::Value = serde_json::from_slice(&first).unwrap();
        second_value["tokens"]["access_token"] = serde_json::json!("new-access-secret");
        second_value["tokens"]["refresh_token"] = serde_json::json!("new-refresh-secret");
        let second = serde_json::to_vec(&second_value).unwrap();

        let first_identity = CodexAdapter::inspect_credential_payload(&first).unwrap();
        let second_identity = CodexAdapter::inspect_credential_payload(&second).unwrap();
        assert_eq!(first_identity, second_identity);
        assert_eq!(
            first_identity.account_label.as_deref(),
            Some("person@example.com")
        );
        assert_eq!(first_identity.auth_mode, "chatgpt");
    }

    #[test]
    fn credential_parser_accepts_unowned_latest_fields_without_using_them() {
        let mut payload: serde_json::Value =
            serde_json::from_slice(&fake_chatgpt_auth("person@example.com", "account-1")).unwrap();
        payload["future_private_credential"] = serde_json::json!("must-not-be-dropped");
        let payload = serde_json::to_vec(&payload).unwrap();
        let identity = CodexAdapter::inspect_credential_payload(&payload).unwrap();
        assert_eq!(identity.auth_mode, "chatgpt");
    }

    #[test]
    fn file_credentials_round_trip_and_verify() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join(CONFIG_FILE_NAME),
            "cli_auth_credentials_store = \"file\"\n",
        )
        .unwrap();
        let payload = fake_chatgpt_auth("person@example.com", "account-1");
        fs::write(temp.path().join(AUTH_FILE_NAME), &payload).unwrap();
        let adapter = CodexAdapter::new();
        let context = context(&temp);
        let snapshot = adapter.capture_credentials(&context, temp.path()).unwrap();

        fs::write(
            temp.path().join(AUTH_FILE_NAME),
            serde_json::to_vec(&serde_json::json!({
                "auth_mode": "apikey",
                "OPENAI_API_KEY": "other-key",
                "agent_identity": {"must": "survive"}
            }))
            .unwrap(),
        )
        .unwrap();
        adapter
            .restore_credentials(&context, temp.path(), &snapshot)
            .unwrap();
        let identity = adapter
            .verify_credentials(&context, temp.path(), &snapshot)
            .unwrap();
        assert_eq!(
            identity.account_label.as_deref(),
            Some("person@example.com")
        );
        let restored: serde_json::Value =
            serde_json::from_slice(&fs::read(temp.path().join(AUTH_FILE_NAME)).unwrap()).unwrap();
        assert_eq!(restored["agent_identity"]["must"], "survive");
        assert_eq!(restored["tokens"]["account_id"], "account-1");
    }

    #[test]
    fn ephemeral_mode_remains_unsupported() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join(CONFIG_FILE_NAME),
            "cli_auth_credentials_store = \"ephemeral\"\n",
        )
        .unwrap();
        let adapter = CodexAdapter::new();
        let error = adapter
            .capture_credentials(&context(&temp), temp.path())
            .unwrap_err();
        assert!(error.contains("ephemeral"));
    }

    #[cfg(unix)]
    #[test]
    fn credential_operations_follow_symlinked_auth_file() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join(CONFIG_FILE_NAME), "").unwrap();
        let outside = temp.path().join("outside.json");
        fs::write(
            &outside,
            fake_chatgpt_auth("person@example.com", "account-1"),
        )
        .unwrap();
        symlink(&outside, temp.path().join(AUTH_FILE_NAME)).unwrap();
        let adapter = CodexAdapter::new();
        let snapshot = adapter
            .capture_credentials(&context(&temp), temp.path())
            .unwrap();
        assert_eq!(
            snapshot.account_label.as_deref(),
            Some("person@example.com")
        );
    }

    #[test]
    fn command_spec_isolates_codex_home_and_removes_inherited_secrets() {
        let spec = codex_command_spec(
            PathBuf::from("/usr/bin/codex"),
            vec!["login".into(), "status".into()],
            Path::new("/tmp/profile"),
            Some(PathBuf::from("/tmp/project")),
        );
        assert_eq!(
            spec.env.get(CODEX_HOME_ENV).map(String::as_str),
            Some("/tmp/profile")
        );
        assert!(spec.env_remove.contains(&"OPENAI_API_KEY".to_string()));
        assert!(spec.env_remove.contains(&"CODEX_ACCESS_TOKEN".to_string()));
        assert_eq!(spec.cwd, Some(PathBuf::from("/tmp/project")));
    }
}
