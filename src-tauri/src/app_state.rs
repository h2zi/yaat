//! Shared application state and platform-adapter selection.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use yaat_contracts::{AppSettings, CliStatus, HistoryScope, Platform, ProviderProfile};

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
    usage_cancelled: Arc<AtomicBool>,
    history_cancelled: Arc<AtomicBool>,
    history_running: Arc<Mutex<HashSet<HistoryScope>>>,
    history_background_tokens: Arc<Mutex<HashMap<HistoryScope, Arc<AtomicBool>>>>,
    cli_probes: Mutex<HashMap<Platform, CliProbe>>,
    codex: CodexAdapter,
    claude: ClaudeAdapter,
    claude_desktop: ClaudeDesktopAdapter,
}

#[derive(Clone, Debug)]
pub struct CliProbe {
    pub platform: Platform,
    pub path: PathBuf,
    pub status: CliStatus,
    pub version: Option<String>,
    pub error: Option<String>,
}

impl AppState {
    pub fn open() -> AppResult<Self> {
        let app_data_dir = paths::app_data_dir()?;
        paths::ensure_private_directory(&app_data_dir)?;
        for child in ["profiles", "catalogs"] {
            paths::ensure_private_directory(&app_data_dir.join(child))?;
        }
        paths::ensure_private_directory(&paths::backups_dir()?)?;
        let database_path = paths::database_path()?;
        #[cfg(windows)]
        let database_existed = database_path.is_file();
        let repository = Repository::open(&database_path).map_err(AppError::from)?;
        #[cfg(windows)]
        if !database_existed {
            paths::ensure_private_file(&database_path)?;
        }
        #[cfg(unix)]
        paths::ensure_private_file(&database_path)?;
        #[cfg(unix)]
        for auxiliary in paths::database_auxiliary_paths(&database_path) {
            if auxiliary.exists() {
                paths::ensure_private_file(&auxiliary)?;
            }
        }
        Ok(Self {
            repository: Arc::new(repository),
            usage_cancelled: Arc::new(AtomicBool::new(false)),
            history_cancelled: Arc::new(AtomicBool::new(false)),
            history_running: Arc::new(Mutex::new(HashSet::new())),
            history_background_tokens: Arc::new(Mutex::new(HashMap::new())),
            cli_probes: Mutex::new(HashMap::new()),
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

    pub fn begin_queued_history(&self, scope: HistoryScope) -> Option<HistoryTaskGuard> {
        let mut running = self.history_running.lock().ok()?;
        if !running.insert(scope) {
            return None;
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut tokens = self.history_background_tokens.lock().ok()?;
        tokens.insert(scope, Arc::clone(&cancelled));
        Some(HistoryTaskGuard {
            scope,
            cancelled,
            running: Arc::clone(&self.history_running),
            tokens: Arc::clone(&self.history_background_tokens),
        })
    }

    pub fn cancel_queued_history(&self, scope: HistoryScope) {
        if let Ok(tokens) = self.history_background_tokens.lock()
            && let Some(cancelled) = tokens.get(&scope)
        {
            cancelled.store(true, Ordering::Release);
        }
    }

    pub fn adapter(&self, platform: Platform) -> &dyn PlatformAdapter {
        match platform {
            Platform::Codex => &self.codex,
            Platform::ClaudeCode => &self.claude,
            Platform::ClaudeDesktop => &self.claude_desktop,
        }
    }

    pub fn context(&self, platform: Platform, settings: &AppSettings) -> AdapterContext {
        Self::context_for(platform, settings)
            .expect("YAAT data root was resolved during application startup")
    }

    pub fn context_for(platform: Platform, settings: &AppSettings) -> AppResult<AdapterContext> {
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
        Ok(AdapterContext {
            data_root: paths::app_data_dir()?,
            explicit_cli_path: cli.map(PathBuf::from),
            explicit_config_root: root.map(PathBuf::from),
        })
    }

    pub fn cached_cli_probe(&self, platform: Platform, path: &Path) -> Option<CliProbe> {
        self.cli_probes
            .lock()
            .ok()?
            .get(&platform)
            .filter(|probe| probe.path == *path)
            .cloned()
    }

    pub fn cache_cli_probe(&self, probe: CliProbe) {
        if let Ok(mut probes) = self.cli_probes.lock() {
            probes.insert(probe.platform, probe);
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
            && !matches!(
                profile.secret_kind,
                yaat_contracts::SecretKind::ApiKey | yaat_contracts::SecretKind::BearerToken
            )
        {
            return Err(AppError::Validation(
                "Claude API profiles require an API key or bearer token".into(),
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

pub struct HistoryTaskGuard {
    scope: HistoryScope,
    cancelled: Arc<AtomicBool>,
    running: Arc<Mutex<HashSet<HistoryScope>>>,
    tokens: Arc<Mutex<HashMap<HistoryScope, Arc<AtomicBool>>>>,
}

impl HistoryTaskGuard {
    pub fn cancelled(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }
}

impl Drop for HistoryTaskGuard {
    fn drop(&mut self) {
        if let Ok(mut tokens) = self.tokens.lock() {
            tokens.remove(&self.scope);
        }
        if let Ok(mut running) = self.running.lock() {
            running.remove(&self.scope);
        }
    }
}
