#![allow(
    dead_code,
    unused_imports,
    reason = "the harness path-includes production modules and exercises only an interoperability subset"
)]
//! Docker interoperability harness for the pinned Claude Code CLI baseline.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use yaat_contracts::{Platform, ProfileStatus, ProviderKind, ProviderProfile, SecretKind};

#[path = "../../../src-tauri/src/activation/mod.rs"]
mod activation;

mod paths {
    use std::path::{Path, PathBuf};

    use yaat_contracts::Platform;

    pub fn default_config_root(platform: Platform) -> Result<PathBuf, String> {
        let environment = match platform {
            Platform::Codex => "CODEX_HOME",
            Platform::ClaudeCode => "CLAUDE_CONFIG_DIR",
            Platform::ClaudeDesktop => "CLAUDE_USER_DATA_DIR",
        };
        std::env::var_os(environment)
            .map(PathBuf::from)
            .ok_or_else(|| format!("Docker interoperability test requires {environment}"))
    }

    pub fn validate_identifier(value: &str) -> Result<(), String> {
        let valid = !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        valid
            .then_some(())
            .ok_or_else(|| "invalid identifier".into())
    }

    pub fn managed_profile_home_at(
        data_root: &Path,
        platform: Platform,
        profile_id: &str,
    ) -> Result<PathBuf, String> {
        validate_identifier(profile_id)?;
        Ok(data_root
            .join("profiles")
            .join(platform.as_str())
            .join(profile_id)
            .join("home"))
    }

    pub fn ensure_private_directory(path: &Path) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

mod process {
    use super::*;

    pub fn run_with_timeout(
        program: &Path,
        args: &[&str],
        timeout: Duration,
    ) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), String> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| error.to_string())?;
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("command timed out".into());
                }
                Err(error) => return Err(error.to_string()),
            }
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        if let Some(mut pipe) = child.stdout.take() {
            pipe.read_to_end(&mut stdout)
                .map_err(|error| error.to_string())?;
        }
        if let Some(mut pipe) = child.stderr.take() {
            pipe.read_to_end(&mut stderr)
                .map_err(|error| error.to_string())?;
        }
        Ok((status, stdout, stderr))
    }
}

mod validation {
    pub fn validate_provider_url(value: &str) -> Result<url::Url, String> {
        let url = url::Url::parse(value).map_err(|error| error.to_string())?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("invalid provider URL".into());
        }
        Ok(url)
    }
}

mod platform {
    use super::*;
    use serde::{Deserialize, Serialize};

    pub mod claude {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../src-tauri/src/platform/claude.rs"
        ));
    }

    #[derive(Clone, Debug, Default, Deserialize, Serialize)]
    pub struct CommandSpec {
        pub program: PathBuf,
        pub args: Vec<String>,
        pub env: BTreeMap<String, String>,
        pub env_remove: Vec<String>,
        pub cwd: Option<PathBuf>,
    }

    #[derive(Clone, Debug)]
    pub struct AdapterContext {
        pub data_root: PathBuf,
        pub explicit_cli_path: Option<PathBuf>,
        pub explicit_config_root: Option<PathBuf>,
    }

    #[derive(Clone, Debug)]
    pub struct ProfileRuntime<'a> {
        pub profile: &'a ProviderProfile,
        pub secret: Option<&'a str>,
    }

    #[derive(Clone, Debug, Default)]
    pub struct CredentialSnapshot {
        pub storage_kind: String,
        pub opaque_payload: Vec<u8>,
        pub account_label: Option<String>,
        pub warning: Option<String>,
    }

    #[derive(Clone, Debug)]
    pub enum CredentialState {
        Present(CredentialSnapshot),
        Absent,
    }

    pub struct GlobalConfigPlan {
        pub path: PathBuf,
        pub format: activation::ConfigFormat,
        pub operations: Vec<activation::PatchOperation>,
        pub sidecars: Vec<SidecarPlan>,
    }

    pub struct SidecarPlan {
        pub path: PathBuf,
        pub contents: Option<Vec<u8>>,
    }

    pub trait PlatformAdapter: Send + Sync {
        fn discover_cli(&self, context: &AdapterContext) -> Result<(PathBuf, String), String>;
        fn prepare_profile(
            &self,
            context: &AdapterContext,
            runtime: ProfileRuntime<'_>,
        ) -> Result<PathBuf, String>;
        fn login_spec(
            &self,
            context: &AdapterContext,
            runtime: ProfileRuntime<'_>,
            console: bool,
        ) -> Result<CommandSpec, String>;
        fn launch_spec(
            &self,
            context: &AdapterContext,
            runtime: ProfileRuntime<'_>,
            cwd: Option<PathBuf>,
            passthrough_args: Vec<String>,
        ) -> Result<CommandSpec, String>;
        fn capture_credentials(
            &self,
            context: &AdapterContext,
            config_root: &Path,
        ) -> Result<CredentialSnapshot, String>;
        fn capture_credential_state(
            &self,
            context: &AdapterContext,
            config_root: &Path,
        ) -> Result<CredentialState, String> {
            self.capture_credentials(context, config_root)
                .map(CredentialState::Present)
        }
        fn restore_credentials(
            &self,
            context: &AdapterContext,
            config_root: &Path,
            snapshot: &CredentialSnapshot,
        ) -> Result<(), String>;
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
                    Err("this platform cannot restore an empty credential state".into())
                }
            }
        }
        fn global_config_plan(
            &self,
            context: &AdapterContext,
            runtime: ProfileRuntime<'_>,
        ) -> Result<GlobalConfigPlan, String>;
    }
}

use activation::PatchEngine;
use platform::claude::ClaudeAdapter;
use platform::{AdapterContext, CredentialState, PlatformAdapter, ProfileRuntime};

fn main() {
    let claude = PathBuf::from(
        std::env::var_os("CLAUDE_BIN").expect("Docker image must provide CLAUDE_BIN"),
    );
    let temp = TempDir::new().unwrap();
    let source_root = PathBuf::from(
        std::env::var_os("CLAUDE_CONFIG_DIR").expect("Docker test must provide CLAUDE_CONFIG_DIR"),
    );
    fs::create_dir(&source_root).unwrap();
    fs::write(
        source_root.join("settings.json"),
        r#"{
  // user comment must survive
  "apiKeyHelper": "obsolete YAAT helper",
  "theme": "dark",
  "permissions": { "allow": ["Read"] },
  "mcpServers": { "user": { "command": "do-not-run" } }
}
"#,
    )
    .unwrap();
    let profile = ProviderProfile {
        id: "docker-profile".into(),
        platform: Platform::ClaudeCode,
        kind: ProviderKind::ThirdParty,
        name: "Docker Anthropic Provider".into(),
        account_label: None,
        base_url: Some("https://gateway.example.invalid".into()),
        model: Some("claude-sonnet-test".into()),
        custom_headers: Vec::new(),
        user_agent: Some("YAAT-Interop/1".into()),
        platform_config: yaat_contracts::ProviderPlatformConfig::ClaudeCode {
            default_model: Some("claude-sonnet-test".into()),
            sonnet: Some("claude-sonnet-test".into()),
            opus: None,
            haiku: None,
            fable: None,
            subagent: None,
        },
        secret_kind: SecretKind::ApiKey,
        has_secret: true,
        profile_home: None,
        status: ProfileStatus::Ready,
        created_at: 0,
        updated_at: 0,
    };
    let context = AdapterContext {
        data_root: temp.path().join("yaat-data"),
        explicit_cli_path: Some(claude.clone()),
        explicit_config_root: None,
    };
    let adapter = ClaudeAdapter::new();
    let (_, version) = adapter.discover_cli(&context).unwrap();
    assert_eq!(version, "2.1.226");

    let runtime = ProfileRuntime {
        profile: &profile,
        secret: Some("fake-token"),
    };
    let plan = adapter
        .global_config_plan(&context, runtime.clone())
        .unwrap();
    PatchEngine::apply_file(&plan.path, plan.format, plan.operations).unwrap();
    let global = fs::read_to_string(source_root.join("settings.json")).unwrap();
    assert!(global.contains("user comment must survive"));
    assert!(global.contains("\"permissions\""));
    assert!(global.contains("\"mcpServers\""));
    assert!(global.contains("\"ANTHROPIC_API_KEY\": \"fake-token\""));
    assert!(global.contains("User-Agent: YAAT-Interop/1"));
    assert!(!global.contains("apiKeyHelper"));

    let project = temp.path().join("project");
    fs::create_dir(&project).unwrap();
    let spec = adapter
        .launch_spec(&context, runtime, Some(project.clone()), Vec::new())
        .unwrap();
    assert_eq!(spec.cwd.as_deref(), Some(project.as_path()));
    let managed_root = PathBuf::from(spec.env.get("CLAUDE_CONFIG_DIR").unwrap());
    let managed = fs::read_to_string(managed_root.join("settings.json")).unwrap();
    assert!(managed.contains("user comment must survive"));
    assert!(managed.contains("\"ANTHROPIC_API_KEY\": \"fake-token\""));
    assert!(!managed.contains("apiKeyHelper"));
    assert!(
        spec.env_remove
            .iter()
            .any(|name| name == "ANTHROPIC_API_KEY")
    );

    let account_root = temp.path().join("claude-account");
    fs::create_dir(&account_root).unwrap();
    fs::write(
        account_root.join(".credentials.json"),
        serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "access-docker",
                "refreshToken": "refresh-docker"
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let account = adapter
        .capture_credentials(&context, &account_root)
        .unwrap();
    let original = adapter
        .capture_credential_state(&context, &source_root)
        .unwrap();
    assert!(matches!(original, CredentialState::Absent));
    adapter
        .restore_credential_state(&context, &source_root, &CredentialState::Present(account))
        .unwrap();
    assert!(source_root.join(".credentials.json").exists());
    adapter
        .restore_credential_state(&context, &source_root, &original)
        .unwrap();
    assert!(!source_root.join(".credentials.json").exists());

    let output = Command::new(&spec.program)
        .arg("--help")
        .envs(&spec.env)
        .env_remove("ANTHROPIC_API_KEY")
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Claude rejected YAAT's managed environment: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!(
        "Claude Code {version} accepted YAAT settings and restored an initially empty credential slot"
    );
}
