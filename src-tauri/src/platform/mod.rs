//! Platform adapter contracts for Codex, Claude Code, and Claude Desktop.

/// Claude Code integration.
pub mod claude;
/// Claude Desktop integration.
pub mod claude_desktop;
mod claude_desktop_credentials;
mod claude_desktop_safe_storage;
/// Codex integration.
pub mod codex;
mod codex_credentials;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use yaat_contracts::ProviderProfile;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::activation::{ConfigFormat, PatchOperation};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub env_remove: Vec<String>,
    pub cwd: Option<PathBuf>,
}

impl CommandSpec {
    pub fn into_command(self) -> std::process::Command {
        let mut command = std::process::Command::new(self.program);
        command.args(self.args);
        command.envs(self.env);
        for name in self.env_remove {
            command.env_remove(name);
        }
        if let Some(cwd) = self.cwd {
            command.current_dir(cwd);
        }
        command
    }
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

#[derive(Clone, Default, Zeroize, ZeroizeOnDrop)]
pub struct CredentialSnapshot {
    pub storage_kind: String,
    pub opaque_payload: Vec<u8>,
    pub account_label: Option<String>,
    /// Transient compatibility warning produced while reading native state.
    /// It is returned to the current operation and is never persisted.
    pub warning: Option<String>,
}

impl fmt::Debug for CredentialSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialSnapshot")
            .field("storage_kind", &self.storage_kind)
            .field(
                "opaque_payload",
                &format_args!("<redacted; {} bytes>", self.opaque_payload.len()),
            )
            .field("account_label", &self.account_label)
            .field("warning", &self.warning)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub enum CredentialState {
    Present(CredentialSnapshot),
    Absent,
}

impl CredentialState {
    pub fn warning(&self) -> Option<&str> {
        match self {
            Self::Present(snapshot) => snapshot.warning.as_deref(),
            Self::Absent => None,
        }
    }
}

/// A platform adapter's complete, explicit plan for the account-owned fields in
/// the user's default config. The activation layer is solely responsible for
/// applying this plan atomically and recording rollback state.
pub struct GlobalConfigPlan {
    pub path: PathBuf,
    pub format: ConfigFormat,
    pub operations: Vec<PatchOperation>,
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
        config_root: &std::path::Path,
    ) -> Result<CredentialSnapshot, String>;
    fn capture_credential_state(
        &self,
        context: &AdapterContext,
        config_root: &std::path::Path,
    ) -> Result<CredentialState, String> {
        self.capture_credentials(context, config_root)
            .map(CredentialState::Present)
    }
    fn restore_credentials(
        &self,
        context: &AdapterContext,
        config_root: &std::path::Path,
        snapshot: &CredentialSnapshot,
    ) -> Result<(), String>;
    fn restore_credential_state(
        &self,
        context: &AdapterContext,
        config_root: &std::path::Path,
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

#[cfg(test)]
mod tests {
    use super::CredentialSnapshot;

    #[test]
    fn credential_snapshot_debug_output_is_redacted() {
        let snapshot = CredentialSnapshot {
            storage_kind: "test".into(),
            opaque_payload: b"private-token".to_vec(),
            account_label: None,
            warning: None,
        };

        let output = format!("{snapshot:?}");
        assert!(output.contains("<redacted; 13 bytes>"));
        assert!(!output.contains("private-token"));
    }
}
