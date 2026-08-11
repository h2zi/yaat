//! Application paths and filesystem-safe identifier validation.

use std::path::{Path, PathBuf};

use directories::BaseDirs;
use yaat_contracts::Platform;

use crate::error::{AppError, AppResult};

pub const APP_DIRECTORY_NAME: &str = ".yaat";

pub fn app_data_dir() -> AppResult<PathBuf> {
    if let Some(path) = std::env::var_os("YAAT_DATA_DIR") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(AppError::Validation(
                "YAAT_DATA_DIR must be an absolute path".into(),
            ));
        }
        return Ok(path);
    }
    let base = BaseDirs::new()
        .ok_or_else(|| AppError::Io("unable to resolve the local data directory".into()))?;
    Ok(base.home_dir().join(APP_DIRECTORY_NAME))
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
    managed_profile_home_at(&app_data_dir()?, platform, profile_id)
}

pub fn managed_profile_home_at(
    data_root: &Path,
    platform: Platform,
    profile_id: &str,
) -> AppResult<PathBuf> {
    validate_identifier(profile_id)?;
    Ok(data_root
        .join("profiles")
        .join(platform.as_str())
        .join(profile_id)
        .join("home"))
}

pub fn database_path() -> AppResult<PathBuf> {
    Ok(app_data_dir()?.join("yaat.sqlite3"))
}

pub fn database_auxiliary_paths(database: &Path) -> [PathBuf; 2] {
    let mut wal = database.as_os_str().to_os_string();
    wal.push("-wal");
    let mut shm = database.as_os_str().to_os_string();
    shm.push("-shm");
    [PathBuf::from(wal), PathBuf::from(shm)]
}

pub fn codex_catalog_path_at(data_root: &Path, profile_id: &str) -> AppResult<PathBuf> {
    validate_identifier(profile_id)?;
    Ok(data_root
        .join("catalogs")
        .join(Platform::Codex.as_str())
        .join(format!("{profile_id}.json")))
}

pub fn backups_dir() -> AppResult<PathBuf> {
    Ok(app_data_dir()?.join("backups"))
}

pub fn ensure_private_directory(path: &Path) -> AppResult<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    restrict_windows_acl(path, true)?;
    Ok(())
}

pub fn ensure_private_file(path: &Path) -> AppResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    restrict_windows_acl(path, false)?;
    Ok(())
}

#[cfg(windows)]
fn restrict_windows_acl(path: &Path, directory: bool) -> AppResult<()> {
    use std::process::Command;

    let identity = Command::new("whoami.exe")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .map_err(AppError::io)?;
    if !identity.status.success() {
        return Err(AppError::Io(
            "unable to resolve the current Windows user SID".into(),
        ));
    }
    let output = String::from_utf8(identity.stdout)
        .map_err(|_| AppError::Io("Windows user SID output is not UTF-8".into()))?;
    let sid = output
        .trim()
        .rsplit_once(',')
        .map(|(_, sid)| sid.trim().trim_matches('"'))
        .filter(|sid| sid.starts_with("S-1-"))
        .ok_or_else(|| AppError::Io("unable to parse the current Windows user SID".into()))?;
    let grant = if directory {
        format!("*{sid}:(OI)(CI)F")
    } else {
        format!("*{sid}:F")
    };
    let status = Command::new("icacls.exe")
        .arg(path)
        .args(["/inheritance:r", "/grant:r", &grant])
        .status()
        .map_err(AppError::io)?;
    if !status.success() {
        return Err(AppError::Io(format!(
            "unable to restrict permissions for {}",
            path.display()
        )));
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

    #[test]
    fn layout_is_home_relative_and_platform_neutral() {
        let unix_home = Path::new("/home/Example User/用户/.yaat");
        assert_eq!(
            managed_profile_home_at(unix_home, Platform::Codex, "profile-1").unwrap(),
            unix_home.join("profiles/codex/profile-1/home")
        );
        assert_eq!(
            database_auxiliary_paths(&unix_home.join("yaat.sqlite3")),
            [
                unix_home.join("yaat.sqlite3-wal"),
                unix_home.join("yaat.sqlite3-shm"),
            ]
        );
        assert_eq!(
            codex_catalog_path_at(unix_home, "profile-1").unwrap(),
            unix_home.join("catalogs/codex/profile-1.json")
        );

        let windows_home = PathBuf::from(r"C:\Users\Example User\用户\.yaat");
        assert_eq!(
            managed_profile_home_at(&windows_home, Platform::ClaudeCode, "profile_2").unwrap(),
            windows_home
                .join("profiles")
                .join("claude_code")
                .join("profile_2")
                .join("home")
        );
    }
}
