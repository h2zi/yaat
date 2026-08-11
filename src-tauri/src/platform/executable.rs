// Deterministic CLI executable discovery for desktop launch environments.
//
// GUI applications do not reliably inherit the interactive shell's `PATH`.
// Resolve supported CLIs from the process path first, then from platform-native
// user installation directories built with OS path APIs.

use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use directories::BaseDirs;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CliProgram {
    Codex,
    ClaudeCode,
}

impl CliProgram {
    fn executable_name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex CLI",
            Self::ClaudeCode => "Claude Code CLI",
        }
    }
}

#[allow(
    dead_code,
    reason = "all variants are exercised by the platform CI matrix and path-construction tests"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostPlatform {
    Macos,
    Linux,
    Windows,
}

impl HostPlatform {
    const fn current() -> Self {
        #[cfg(target_os = "macos")]
        return Self::Macos;
        #[cfg(windows)]
        return Self::Windows;
        #[cfg(not(any(target_os = "macos", windows)))]
        return Self::Linux;
    }
}

/// Resolve a supported CLI without assuming that a desktop process inherited
/// the user's interactive shell path.
pub fn resolve(program: CliProgram, explicit: Option<&Path>) -> Result<PathBuf, String> {
    let base =
        BaseDirs::new().ok_or_else(|| "unable to resolve the user home directory".to_string())?;
    resolve_with(
        program,
        explicit,
        env::var_os("PATH").as_deref(),
        base.home_dir(),
        base.data_dir(),
        base.data_local_dir(),
        HostPlatform::current(),
    )
}

fn resolve_with(
    program: CliProgram,
    explicit: Option<&Path>,
    search_path: Option<&OsStr>,
    home: &Path,
    data_dir: &Path,
    data_local_dir: &Path,
    host: HostPlatform,
) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        let candidate = if path.components().count() == 1 {
            find_on_path(path.as_os_str(), search_path, home).map_err(|error| {
                format!(
                    "configured {} `{}` was not found: {error}",
                    program.display_name(),
                    path.display()
                )
            })?
        } else {
            if !path.is_absolute() {
                return Err(format!(
                    "configured {} path must be absolute",
                    program.display_name()
                ));
            }
            path.to_path_buf()
        };
        return validate_candidate(&candidate, program);
    }

    if let Ok(path) = find_on_path(program.executable_name(), search_path, home)
        && let Ok(path) = validate_candidate(&path, program)
    {
        return Ok(path);
    }

    let mut inspected = HashSet::new();
    for candidate in candidate_paths(program, home, data_dir, data_local_dir, host) {
        if inspected.insert(candidate.clone())
            && let Ok(path) = validate_candidate(&candidate, program)
        {
            return Ok(path);
        }
    }

    Err(format!(
        "{} was not found in PATH or a supported installation directory",
        program.display_name()
    ))
}

fn find_on_path(
    name: impl AsRef<OsStr>,
    search_path: Option<&OsStr>,
    cwd: &Path,
) -> Result<PathBuf, which::Error> {
    which::which_in(name, search_path, cwd)
}

fn validate_candidate(path: &Path, program: CliProgram) -> Result<PathBuf, String> {
    let metadata = fs::metadata(path).map_err(|error| {
        format!(
            "{} {} is unavailable: {error}",
            program.display_name(),
            path.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "{} {} is not a file",
            program.display_name(),
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "{} {} is not executable",
                program.display_name(),
                path.display()
            ));
        }
    }
    Ok(path.to_path_buf())
}

fn candidate_paths(
    program: CliProgram,
    home: &Path,
    data_dir: &Path,
    data_local_dir: &Path,
    host: HostPlatform,
) -> Vec<PathBuf> {
    let mut directories = vec![
        home.join(".local").join("bin"),
        home.join(".cargo").join("bin"),
        home.join(".bun").join("bin"),
        home.join(".npm-global").join("bin"),
    ];

    match host {
        HostPlatform::Macos => {
            directories.extend([
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/local/bin"),
            ]);
        }
        HostPlatform::Linux => {
            directories.extend([
                PathBuf::from("/usr/local/bin"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/snap/bin"),
            ]);
        }
        HostPlatform::Windows => {
            directories.extend([
                data_dir.join("npm"),
                data_local_dir.join("Programs").join("nodejs"),
                data_local_dir.join("Microsoft").join("WindowsApps"),
            ]);
            if let Some(program_files) =
                env::var_os("ProgramFiles").filter(|value| !value.is_empty())
            {
                directories.push(PathBuf::from(program_files).join("nodejs"));
            }
        }
    }

    let names = executable_names(program, host);
    let mut paths = directories
        .into_iter()
        .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
        .collect::<Vec<_>>();

    if host == HostPlatform::Macos && program == CliProgram::Codex {
        for application_root in [PathBuf::from("/Applications"), home.join("Applications")] {
            paths.push(
                application_root
                    .join("ChatGPT.app")
                    .join("Contents")
                    .join("Resources")
                    .join("codex"),
            );
            paths.push(
                application_root
                    .join("Codex.app")
                    .join("Contents")
                    .join("Resources")
                    .join("codex"),
            );
        }
    }
    paths
}

fn executable_names(program: CliProgram, host: HostPlatform) -> Vec<&'static str> {
    let name = program.executable_name();
    if host == HostPlatform::Windows {
        match program {
            CliProgram::Codex => vec!["codex.exe", "codex.cmd", "codex.bat", name],
            CliProgram::ClaudeCode => vec!["claude.exe", "claude.cmd", "claude.bat", name],
        }
    } else {
        vec![name]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"test executable").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn gui_path_falls_back_to_user_local_bin() {
        let temp = tempfile::tempdir().unwrap();
        let empty_path = temp.path().join("empty-path");
        fs::create_dir(&empty_path).unwrap();
        let name = executable_names(CliProgram::Codex, HostPlatform::current())[0];
        let expected = temp.path().join(".local").join("bin").join(name);
        executable(&expected);

        let resolved = resolve_with(
            CliProgram::Codex,
            None,
            Some(empty_path.as_os_str()),
            temp.path(),
            &temp.path().join("data"),
            &temp.path().join("local-data"),
            HostPlatform::current(),
        )
        .unwrap();

        assert_eq!(resolved, expected);
    }

    #[test]
    fn invalid_explicit_path_does_not_silently_fall_back() {
        let temp = tempfile::tempdir().unwrap();
        let fallback = temp.path().join(".local/bin/codex");
        executable(&fallback);
        let missing = temp.path().join("configured/codex");

        let error = resolve_with(
            CliProgram::Codex,
            Some(&missing),
            None,
            temp.path(),
            &temp.path().join("data"),
            &temp.path().join("local-data"),
            HostPlatform::current(),
        )
        .unwrap_err();

        assert!(error.contains(&missing.display().to_string()));
    }

    #[test]
    fn windows_candidates_use_native_path_components_and_wrappers() {
        let home = Path::new(r"C:\Users\Example User\用户");
        let data = Path::new(r"C:\Users\Example User\用户\AppData\Roaming");
        let local = Path::new(r"C:\Users\Example User\用户\AppData\Local");
        let candidates = candidate_paths(
            CliProgram::ClaudeCode,
            home,
            data,
            local,
            HostPlatform::Windows,
        );

        assert!(candidates.contains(&home.join(".local").join("bin").join("claude.exe")));
        assert!(candidates.contains(&data.join("npm").join("claude.cmd")));
        assert!(
            candidates.contains(
                &local
                    .join("Microsoft")
                    .join("WindowsApps")
                    .join("claude.exe")
            )
        );
    }

    #[test]
    fn macos_codex_candidates_include_desktop_bundle_cli() {
        let candidates = candidate_paths(
            CliProgram::Codex,
            Path::new("/Users/Example User"),
            Path::new("/Users/Example User/Library/Application Support"),
            Path::new("/Users/Example User/Library/Application Support"),
            HostPlatform::Macos,
        );

        assert!(candidates.contains(&PathBuf::from(
            "/Applications/ChatGPT.app/Contents/Resources/codex"
        )));
        assert!(candidates.contains(&PathBuf::from(
            "/Users/Example User/Applications/Codex.app/Contents/Resources/codex"
        )));
    }
}
