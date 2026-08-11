#![allow(
    dead_code,
    unused_imports,
    reason = "the harness path-includes production modules and exercises only an interoperability subset"
)]
//! Docker interoperability harness for the pinned Codex CLI baseline.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use tempfile::TempDir;
use toml_edit::DocumentMut;
use yaat_contracts::{Platform, ProfileStatus, ProviderKind, ProviderProfile, SecretKind};

#[path = "../../../src-tauri/src/activation/mod.rs"]
mod activation;

mod history {
    pub const CODEX_HISTORY_PROVIDER_ID: &str = "custom";
}

mod process {
    use std::io::Read;
    use std::path::Path;
    use std::process::{Command, ExitStatus, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    const MAX_OUTPUT_BYTES: u64 = 64 * 1024;

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
            .map_err(|error| format!("failed to start {}: {error}", program.display()))?;
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{} timed out", program.display()));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error.to_string());
                }
            }
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        if let Some(pipe) = child.stdout.take() {
            pipe.take(MAX_OUTPUT_BYTES + 1)
                .read_to_end(&mut stdout)
                .map_err(|error| error.to_string())?;
        }
        if let Some(pipe) = child.stderr.take() {
            pipe.take(MAX_OUTPUT_BYTES + 1)
                .read_to_end(&mut stderr)
                .map_err(|error| error.to_string())?;
        }
        if stdout.len() as u64 > MAX_OUTPUT_BYTES || stderr.len() as u64 > MAX_OUTPUT_BYTES {
            return Err(format!("{} returned too much output", program.display()));
        }
        Ok((status, stdout, stderr))
    }
}

mod paths {
    use std::path::{Path, PathBuf};

    use yaat_contracts::Platform;

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

    pub fn codex_catalog_path_at(data_root: &Path, profile_id: &str) -> Result<PathBuf, String> {
        validate_identifier(profile_id)?;
        Ok(data_root
            .join("catalogs")
            .join("codex")
            .join(format!("{profile_id}.json")))
    }

    pub fn ensure_private_directory(path: &Path) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(path)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
    }

    pub fn ensure_private_file(path: &Path) -> Result<(), std::io::Error> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
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
}

mod platform {
    use super::*;
    use serde::{Deserialize, Serialize};

    pub(super) mod codex_credentials {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../src-tauri/src/platform/codex_credentials.rs"
        ));
    }

    pub mod codex {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../src-tauri/src/platform/codex.rs"
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
use platform::codex::CodexAdapter;
use platform::{AdapterContext, CredentialState, PlatformAdapter, ProfileRuntime};

const INTEGRATION_TOKEN: &str = "docker-integration-token";

fn main() {
    let codex =
        PathBuf::from(std::env::var_os("CODEX_BIN").expect("Docker image must provide CODEX_BIN"));
    assert!(codex.is_file());
    let version = Command::new(&codex).arg("--version").output().unwrap();
    assert!(version.status.success());
    eprintln!(
        "testing {}",
        String::from_utf8_lossy(&version.stdout).trim()
    );

    let temp = TempDir::new().unwrap();
    let config_root = temp.path().join("codex-home");
    fs::create_dir(&config_root).unwrap();
    let source = r#"# must survive YAAT activation
approval_policy = "never"
hide_agent_reasoning = true

[mcp_servers.user_owned]
command = "do-not-run-this-test"
enabled = false
"#;
    fs::write(config_root.join("config.toml"), source).unwrap();

    let (base_url, received) = start_mock_responses_server();
    let profile = ProviderProfile {
        id: "docker-profile".into(),
        platform: Platform::Codex,
        kind: ProviderKind::ThirdParty,
        name: "Docker Responses Provider".into(),
        account_label: None,
        base_url: Some(base_url),
        model: Some("docker-test-model".into()),
        custom_headers: Vec::new(),
        user_agent: None,
        platform_config: yaat_contracts::ProviderPlatformConfig::Codex {
            default_model: Some("docker-test-model".into()),
            catalog: Vec::new(),
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
        explicit_cli_path: Some(codex.clone()),
        explicit_config_root: Some(config_root.clone()),
    };
    let plan = CodexAdapter
        .global_config_plan(
            &context,
            ProfileRuntime {
                profile: &profile,
                secret: Some(INTEGRATION_TOKEN),
            },
        )
        .unwrap();
    assert_eq!(plan.sidecars.len(), 1);
    assert!(plan.sidecars[0].contents.is_none());
    PatchEngine::apply_file(&plan.path, plan.format, plan.operations).unwrap();

    let patched = fs::read_to_string(config_root.join("config.toml")).unwrap();
    assert!(patched.contains("# must survive YAAT activation"));
    assert!(patched.contains("hide_agent_reasoning = true"));
    assert!(patched.contains("[mcp_servers.user_owned]"));
    assert!(patched.contains(INTEGRATION_TOKEN));
    let parsed = patched.parse::<DocumentMut>().unwrap();
    assert_eq!(
        parsed["model_providers"]["custom"]["experimental_bearer_token"].as_str(),
        Some(INTEGRATION_TOKEN)
    );
    assert!(patched.contains("[model_providers.custom]"));

    let mut child = Command::new(&codex)
        .args([
            "--strict-config",
            "exec",
            "--skip-git-repo-check",
            "--sandbox",
            "read-only",
            "--color",
            "never",
            "send one short response",
        ])
        .current_dir(temp.path())
        .env("CODEX_HOME", &config_root)
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .env_remove("CODEX_ACCESS_TOKEN")
        .env_remove("OPENAI_BASE_URL")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let observed = received.recv_timeout(Duration::from_secs(20));
    let early_status = child.try_wait().unwrap();
    let _ = child.kill();
    let _ = child.wait();
    let authorization = observed.unwrap_or_else(|error| {
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        panic!(
            "Codex made no mock Responses request ({error}); early status: {early_status:?}; stderr: {stderr}"
        )
    });
    assert_eq!(authorization, format!("Bearer {INTEGRATION_TOKEN}"));
    eprintln!("Codex accepted YAAT strict config and direct provider credential");

    verify_official_credential_switch(&codex, temp.path());
}

fn verify_official_credential_switch(codex: &Path, directory: &Path) {
    let adapter = CodexAdapter::new();
    let active_root = directory.join("official-active");
    let account_b_root = directory.join("official-account-b");
    fs::create_dir(&active_root).unwrap();
    fs::create_dir(&account_b_root).unwrap();

    let source = r#"# official-switch comment must survive
approval_policy = "never"
hide_agent_reasoning = true
model = "old-model"
model_provider = "old-provider"
cli_auth_credentials_store = "file"

[model_providers.user_owned]
name = "User-owned provider"
base_url = "https://user-owned.invalid/v1"
wire_api = "responses"

[mcp_servers.user_owned]
command = "do-not-run-this-test"
enabled = false
"#;
    fs::write(active_root.join("config.toml"), source).unwrap();
    fs::write(
        account_b_root.join("config.toml"),
        "cli_auth_credentials_store = \"file\"\n",
    )
    .unwrap();

    let mut account_a_doc: serde_json::Value = serde_json::from_slice(&fake_chatgpt_auth(
        "a@example.test",
        "user-a",
        "account-a",
        "a",
    ))
    .unwrap();
    account_a_doc["yaat_test_unowned"] = serde_json::json!({
        "value": "must-survive-account-switches"
    });
    let account_a = serde_json::to_vec_pretty(&account_a_doc).unwrap();
    let account_b = fake_chatgpt_auth("b@example.test", "user-b", "account-b", "b");
    fs::write(active_root.join("auth.json"), &account_a).unwrap();
    fs::write(account_b_root.join("auth.json"), &account_b).unwrap();

    let context = AdapterContext {
        data_root: directory.join("yaat-data"),
        explicit_cli_path: Some(codex.to_owned()),
        explicit_config_root: Some(active_root.clone()),
    };
    let account_a_snapshot = adapter.capture_credentials(&context, &active_root).unwrap();
    let account_b_snapshot = adapter
        .capture_credentials(&context, &account_b_root)
        .unwrap();
    assert_ne!(
        account_a_snapshot.opaque_payload,
        account_b_snapshot.opaque_payload
    );
    assert_eq!(
        account_a_snapshot.account_label.as_deref(),
        Some("a@example.test")
    );
    assert_eq!(
        account_b_snapshot.account_label.as_deref(),
        Some("b@example.test")
    );

    let profile = ProviderProfile {
        id: "official-account-b".into(),
        platform: Platform::Codex,
        kind: ProviderKind::OfficialSubscription,
        name: "Official account B".into(),
        account_label: account_b_snapshot.account_label.clone(),
        base_url: None,
        model: None,
        custom_headers: Vec::new(),
        user_agent: None,
        platform_config: yaat_contracts::ProviderPlatformConfig::empty_for(Platform::Codex),
        secret_kind: SecretKind::None,
        has_secret: false,
        profile_home: None,
        status: ProfileStatus::Ready,
        created_at: 0,
        updated_at: 0,
    };
    let plan = adapter
        .global_config_plan(
            &context,
            ProfileRuntime {
                profile: &profile,
                secret: None,
            },
        )
        .unwrap();
    PatchEngine::apply_file(&plan.path, plan.format, plan.operations).unwrap();

    let patched = fs::read_to_string(active_root.join("config.toml")).unwrap();
    assert!(patched.contains("# official-switch comment must survive"));
    assert!(patched.contains("hide_agent_reasoning = true"));
    assert!(patched.contains("[model_providers.user_owned]"));
    assert!(patched.contains("[mcp_servers.user_owned]"));
    let parsed = patched.parse::<DocumentMut>().unwrap();
    assert_eq!(parsed["model_provider"].as_str(), Some("custom"));
    assert_eq!(parsed["cli_auth_credentials_store"].as_str(), Some("file"));
    assert!(parsed.get("model").is_none());
    assert_eq!(
        parsed["model_providers"]["custom"]["requires_openai_auth"].as_bool(),
        Some(true)
    );

    adapter
        .restore_credentials(&context, &active_root, &account_b_snapshot)
        .unwrap();
    adapter
        .verify_credentials(&context, &active_root, &account_b_snapshot)
        .unwrap();
    assert_switched_auth(&active_root.join("auth.json"), &account_b);
    assert_codex_recognizes_chatgpt_login(codex, &active_root);

    adapter
        .restore_credentials(&context, &active_root, &account_a_snapshot)
        .unwrap();
    adapter
        .verify_credentials(&context, &active_root, &account_a_snapshot)
        .unwrap();
    assert_switched_auth(&active_root.join("auth.json"), &account_a);
    assert_codex_recognizes_chatgpt_login(codex, &active_root);

    let empty_root = directory.join("official-empty");
    fs::create_dir(&empty_root).unwrap();
    fs::write(
        empty_root.join("config.toml"),
        "cli_auth_credentials_store = \"file\"\n",
    )
    .unwrap();
    let empty_context = AdapterContext {
        explicit_config_root: Some(empty_root.clone()),
        ..context.clone()
    };
    let original = adapter
        .capture_credential_state(&empty_context, &empty_root)
        .unwrap();
    assert!(matches!(original, CredentialState::Absent));
    adapter
        .restore_credential_state(
            &empty_context,
            &empty_root,
            &CredentialState::Present(account_b_snapshot.clone()),
        )
        .unwrap();
    assert_codex_recognizes_chatgpt_login(codex, &empty_root);
    adapter
        .restore_credential_state(&empty_context, &empty_root, &original)
        .unwrap();
    assert!(!empty_root.join("auth.json").exists());

    eprintln!(
        "Codex recognized copied official credentials, restored an empty slot, and preserved unowned config"
    );
}

fn assert_switched_auth(path: &Path, expected_account: &[u8]) {
    let actual: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let expected: serde_json::Value = serde_json::from_slice(expected_account).unwrap();

    for field in ["auth_mode", "OPENAI_API_KEY", "tokens", "last_refresh"] {
        assert_eq!(
            actual.get(field),
            expected.get(field),
            "account field {field}"
        );
    }
    assert_eq!(
        actual["yaat_test_unowned"]["value"], "must-survive-account-switches",
        "YAAT must preserve non-account Codex auth fields"
    );
}

fn fake_chatgpt_auth(email: &str, user_id: &str, account_id: &str, marker: &str) -> Vec<u8> {
    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = encoder.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let claims = serde_json::json!({
        "sub": user_id,
        "email": email,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account_id,
            "chatgpt_user_id": user_id
        }
    });
    let payload = encoder.encode(serde_json::to_vec(&claims).unwrap());
    let id_token = format!("{header}.{payload}.signature-{marker}");
    serde_json::to_vec_pretty(&serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": id_token,
            "access_token": format!("fake-access-{marker}"),
            "refresh_token": format!("fake-refresh-{marker}"),
            "account_id": account_id
        },
        "last_refresh": "2026-08-02T00:00:00Z"
    }))
    .unwrap()
}

fn assert_codex_recognizes_chatgpt_login(codex: &Path, config_root: &Path) {
    let output = Command::new(codex)
        .args(["login", "status"])
        .env("CODEX_HOME", config_root)
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .env_remove("CODEX_ACCESS_TOKEN")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "codex login status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("Logged in using ChatGPT") || stderr.contains("Logged in using ChatGPT"),
        "unexpected codex login status output; stdout: {stdout}; stderr: {stderr}"
    );
}

fn start_mock_responses_server() -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut last_headers = String::new();
        for _ in 0..6 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).unwrap_or(0);
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                assert!(
                    request.len() < 256 * 1024,
                    "unexpectedly large HTTP headers"
                );
            }
            let headers = String::from_utf8_lossy(&request).into_owned();
            let authorization = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("authorization")
                        .then(|| value.trim().to_owned())
                })
                .unwrap_or_default();
            if !authorization.is_empty() {
                sender.send(authorization).unwrap();
                let _ = stream.write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                return;
            }
            last_headers = headers;
            let _ = stream.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            );
        }
        sender
            .send(format!("__NO_AUTHORIZATION__\n{last_headers}"))
            .unwrap();
    });
    (format!("http://{address}/v1"), receiver)
}
