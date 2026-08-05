//! Shared application state and platform-adapter selection.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use yaat_contracts::{AppSettings, Platform, ProviderProfile};

use crate::db::Repository;
use crate::error::{AppError, AppResult};
use crate::platform::claude::ClaudeAdapter;
use crate::platform::claude_desktop::ClaudeDesktopAdapter;
use crate::platform::codex::CodexAdapter;
use crate::platform::{AdapterContext, PlatformAdapter};
use crate::usage::service::UsageRoot;
use crate::{paths, validation};

pub struct AppState {
    pub repository: Arc<Repository>,
    pub app_data_dir: PathBuf,
    pub helper_executable: PathBuf,
    usage_cancelled: Arc<AtomicBool>,
    history_cancelled: Arc<AtomicBool>,
    codex: CodexAdapter,
    claude: ClaudeAdapter,
    claude_desktop: ClaudeDesktopAdapter,
}

impl AppState {
    pub fn open(helper_executable: PathBuf) -> AppResult<Self> {
        if !helper_executable.is_absolute() {
            return Err(AppError::Internal(
                "YAAT executable path is not absolute".into(),
            ));
        }
        let app_data_dir = paths::app_data_dir()?;
        paths::ensure_private_directory(&app_data_dir)?;
        let repository = Repository::open(paths::database_path()?).map_err(AppError::from)?;
        Ok(Self {
            repository: Arc::new(repository),
            app_data_dir,
            helper_executable,
            usage_cancelled: Arc::new(AtomicBool::new(false)),
            history_cancelled: Arc::new(AtomicBool::new(false)),
            codex: CodexAdapter::new(),
            claude: ClaudeAdapter::new(),
            claude_desktop: ClaudeDesktopAdapter::new(),
        })
    }

    pub fn begin_usage_operation(&self) -> Arc<AtomicBool> {
        self.usage_cancelled.store(false, Ordering::Release);
        Arc::clone(&self.usage_cancelled)
    }

    pub fn cancel_usage_operation(&self) {
        self.usage_cancelled.store(true, Ordering::Release);
    }

    pub fn begin_history_operation(&self) -> Arc<AtomicBool> {
        self.history_cancelled.store(false, Ordering::Release);
        Arc::clone(&self.history_cancelled)
    }

    pub fn cancel_history_operation(&self) {
        self.history_cancelled.store(true, Ordering::Release);
    }

    pub fn adapter(&self, platform: Platform) -> &dyn PlatformAdapter {
        match platform {
            Platform::Codex => &self.codex,
            Platform::ClaudeCode => &self.claude,
            Platform::ClaudeDesktop => &self.claude_desktop,
        }
    }

    pub fn context(&self, platform: Platform, settings: &AppSettings) -> AdapterContext {
        let (cli, root) = match platform {
            Platform::Codex => (
                settings.codex_path.as_deref(),
                settings.codex_home.as_deref(),
            ),
            Platform::ClaudeCode => (
                settings.claude_path.as_deref(),
                settings.claude_config_dir.as_deref(),
            ),
            Platform::ClaudeDesktop => (settings.claude_desktop_path.as_deref(), None),
        };
        AdapterContext {
            app_data_dir: self.app_data_dir.clone(),
            helper_executable: self.helper_executable.clone(),
            explicit_cli_path: cli.map(PathBuf::from),
            explicit_config_root: root.map(PathBuf::from),
        }
    }

    pub fn config_root(&self, platform: Platform, settings: &AppSettings) -> AppResult<PathBuf> {
        let configured = match platform {
            Platform::Codex => settings.codex_home.as_deref(),
            Platform::ClaudeCode => settings.claude_config_dir.as_deref(),
            Platform::ClaudeDesktop => None,
        };
        match configured {
            Some(path) => Ok(PathBuf::from(path)),
            None => paths::default_config_root(platform),
        }
    }

    pub fn usage_roots(
        &self,
        platform: Platform,
        settings: &AppSettings,
        profiles: &[ProviderProfile],
    ) -> AppResult<Vec<UsageRoot>> {
        let mut roots = vec![UsageRoot {
            path: self.config_root(platform, settings)?,
        }];
        for profile in profiles
            .iter()
            .filter(|profile| profile.platform == platform)
        {
            roots.push(UsageRoot {
                path: paths::managed_profile_home(platform, &profile.id)?,
            });
        }
        Ok(roots)
    }

    pub fn validate_profile_for_platform(&self, profile: &ProviderProfile) -> AppResult<()> {
        paths::validate_identifier(&profile.id)?;
        validation::validate_name(&profile.name)?;
        if matches!(
            profile.platform,
            Platform::ClaudeCode | Platform::ClaudeDesktop
        ) && profile.kind != yaat_contracts::ProviderKind::OfficialSubscription
            && profile.secret_kind != yaat_contracts::SecretKind::ApiKey
        {
            return Err(AppError::Validation(
                "Claude API profiles require an API key; bearer-token protocol conversion is not supported"
                    .into(),
            ));
        }
        if profile.platform == Platform::ClaudeDesktop
            && profile.kind != yaat_contracts::ProviderKind::OfficialSubscription
        {
            crate::platform::claude_desktop::validate_direct_model(
                profile.model.as_deref(),
                profile.kind == yaat_contracts::ProviderKind::ThirdParty,
            )
            .map_err(AppError::Validation)?;
        }
        Ok(())
    }
}
