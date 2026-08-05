//! Application paths and filesystem-safe identifier validation.

use std::path::{Path, PathBuf};

use directories::BaseDirs;
use yaat_contracts::Platform;

use crate::error::{AppError, AppResult};

pub const APP_DIRECTORY_NAME: &str = "yet.another.account.tool";

pub fn app_data_dir() -> AppResult<PathBuf> {
    if let Some(path) = std::env::var_os("YAAT_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    let base = BaseDirs::new()
        .ok_or_else(|| AppError::Io("unable to resolve the local data directory".into()))?;
    Ok(base.data_local_dir().join(APP_DIRECTORY_NAME))
}

pub fn default_config_root(platform: Platform) -> AppResult<PathBuf> {
    let env_name = match platform {
        Platform::Codex => "CODEX_HOME",
        Platform::ClaudeCode => "CLAUDE_CONFIG_DIR",
        Platform::ClaudeDesktop => "CLAUDE_USER_DATA_DIR",
    };
    if let Some(path) = std::env::var_os(env_name).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let base = BaseDirs::new()
        .ok_or_else(|| AppError::Io("unable to resolve the user home directory".into()))?;
    match platform {
        Platform::Codex => Ok(base.home_dir().join(".codex")),
        Platform::ClaudeCode => Ok(base.home_dir().join(".claude")),
        Platform::ClaudeDesktop => {
            #[cfg(target_os = "macos")]
            return Ok(base.data_dir().join("Claude"));
            #[cfg(target_os = "windows")]
            return Ok(base.data_local_dir().join("Claude"));
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            return Ok(base.data_local_dir().join("Claude"));
        }
    }
}

pub fn managed_profile_home(platform: Platform, profile_id: &str) -> AppResult<PathBuf> {
    validate_identifier(profile_id)?;
    Ok(app_data_dir()?
        .join("profiles")
        .join(platform.as_str())
        .join(profile_id)
        .join("home"))
}

pub fn database_path() -> AppResult<PathBuf> {
    Ok(app_data_dir()?.join("yaat.sqlite3"))
}

pub fn ensure_private_directory(path: &Path) -> AppResult<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn validate_identifier(value: &str) -> AppResult<()> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if valid {
        Ok(())
    } else {
        Err(AppError::Validation("invalid identifier".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_rejects_path_traversal() {
        assert!(validate_identifier("../secret").is_err());
        assert!(validate_identifier("good-id_01").is_ok());
    }
}
