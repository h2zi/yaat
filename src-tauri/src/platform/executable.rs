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
        return validate_candidate_for_host(&candidate, program, host);
    }

    // The desktop app's relocated cache is a normal user executable. Resolve
    // it before PATH because Microsoft Store app execution aliases may point
    // back into the protected WindowsApps package.
    if host == HostPlatform::Windows && program == CliProgram::Codex {
        for candidate in windows_codex_desktop_cli_candidates(data_local_dir) {
            if let Ok(path) = validate_candidate_for_host(&candidate, program, host) {
                return Ok(path);
            }
        }
    }

    if let Ok(path) = find_on_path(program.executable_name(), search_path, home)
        && let Ok(path) = validate_candidate_for_host(&path, program, host)
    {
        return Ok(path);
    }

    let mut inspected = HashSet::new();
    for candidate in candidate_paths(program, home, data_dir, data_local_dir, host) {
        if inspected.insert(candidate.clone())
            && let Ok(path) = validate_candidate_for_host(&candidate, program, host)
        {
            return Ok(path);
        }
    }

    if host == HostPlatform::Windows
        && program == CliProgram::Codex
        && windows_codex_msix_package_present(data_local_dir)
    {
        return Err(
            "An OpenAI desktop app is installed from Microsoft Store, but no runnable Codex CLI cache was found; open ChatGPT/Codex once to initialize Codex, or configure a standalone Codex CLI path"
                .into(),
        );
    }

    Err(format!(
        "{} was not found in PATH or a supported installation directory",
        program.display_name()
    ))
}

fn validate_candidate_for_host(
    path: &Path,
    program: CliProgram,
    host: HostPlatform,
) -> Result<PathBuf, String> {
    if host == HostPlatform::Windows
        && program == CliProgram::Codex
        && path.components().any(|component| {
            component
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case("WindowsApps")
        })
    {
        return Err(format!(
            "Codex executable {} is inside the protected WindowsApps package; use the desktop CLI cache or a standalone CLI instead",
            path.display()
        ));
    }
    validate_candidate(path, program)
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
    let ordinary_paths = directories
        .into_iter()
        .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
        .collect::<Vec<_>>();

    // The Microsoft Store alias can resolve to the protected MSIX package and
    // fail when executed outside the app container. Prefer the desktop app's
    // relocated, user-executable CLI cache before ordinary Windows candidates.
    let mut paths = if host == HostPlatform::Windows && program == CliProgram::Codex {
        windows_codex_desktop_cli_candidates(data_local_dir)
    } else {
        Vec::new()
    };
    paths.extend(ordinary_paths);

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

fn windows_codex_desktop_cli_candidates(data_local_dir: &Path) -> Vec<PathBuf> {
    let mut bin_directories = Vec::new();
    for product in ["Codex", "ChatGPT"] {
        bin_directories.push(data_local_dir.join("OpenAI").join(product).join("bin"));
    }
    for package_root in windows_codex_msix_package_roots(data_local_dir) {
        for product in ["Codex", "ChatGPT"] {
            bin_directories.push(
                package_root
                    .join("LocalCache")
                    .join("Local")
                    .join("OpenAI")
                    .join(product)
                    .join("bin"),
            );
        }
    }

    let mut candidates = Vec::new();
    for bin in bin_directories {
        candidates.push(bin.join("codex.exe"));
        let Ok(entries) = fs::read_dir(&bin) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                candidates.push(path.join("codex.exe"));
            }
        }
    }
    candidates.sort_by(|left, right| {
        let left_modified = fs::metadata(left)
            .and_then(|metadata| metadata.modified())
            .ok();
        let right_modified = fs::metadata(right)
            .and_then(|metadata| metadata.modified())
            .ok();
        right_modified
            .cmp(&left_modified)
            .then_with(|| right.cmp(left))
    });
    candidates
}

fn windows_codex_msix_package_present(data_local_dir: &Path) -> bool {
    !windows_codex_msix_package_roots(data_local_dir).is_empty()
}

fn windows_codex_msix_package_roots(data_local_dir: &Path) -> Vec<PathBuf> {
    let packages = data_local_dir.join("Packages");
    let Ok(entries) = fs::read_dir(packages) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let file_name = entry.file_name();
            let file_name = file_name.to_str()?;
            let file_name = file_name.to_ascii_lowercase();
            let supported = [
                "openai.codex_",
                "openai.chatgpt_",
                "openai.chatgpt-desktop_",
            ]
            .iter()
            .any(|prefix| file_name.starts_with(prefix));
            (supported && entry.path().is_dir()).then(|| entry.path())
        })
        .collect()
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
    fn windows_rejects_an_explicit_codex_binary_inside_windowsapps() {
        let temp = tempfile::tempdir().unwrap();
        let protected = temp
            .path()
            .join("Program Files")
            .join("WindowsApps")
            .join("OpenAI.Codex_1.0.0.0_x64__publisher")
            .join("app")
            .join("resources")
            .join("codex.exe");
        executable(&protected);

        let error = resolve_with(
            CliProgram::Codex,
            Some(&protected),
            None,
            temp.path(),
            &temp.path().join("data"),
            &temp.path().join("local-data"),
            HostPlatform::Windows,
        )
        .unwrap_err();

        assert!(error.contains("protected WindowsApps package"));
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
    fn windows_candidates_include_the_desktop_apps_relocated_cli() {
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().join("AppData").join("Local");
        let relocated = local
            .join("OpenAI")
            .join("Codex")
            .join("bin")
            .join("current-version-hash")
            .join("codex.exe");
        executable(&relocated);

        let candidates = candidate_paths(
            CliProgram::Codex,
            temp.path(),
            &temp.path().join("data"),
            &local,
            HostPlatform::Windows,
        );

        assert!(candidates.contains(&relocated));
    }

    #[test]
    fn windows_candidates_include_the_msix_local_cache_cli() {
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().join("AppData").join("Local");
        let relocated = local
            .join("Packages")
            .join("OpenAI.Codex_2p2nqsd0c76g0")
            .join("LocalCache")
            .join("Local")
            .join("OpenAI")
            .join("Codex")
            .join("bin")
            .join("current-version-hash")
            .join("codex.exe");
        executable(&relocated);

        let resolved = resolve_with(
            CliProgram::Codex,
            None,
            None,
            temp.path(),
            &temp.path().join("data"),
            &local,
            HostPlatform::Windows,
        )
        .unwrap();

        assert_eq!(resolved, relocated);
    }

    #[test]
    fn windows_prefers_the_relocated_desktop_cli_over_a_path_alias() {
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().join("AppData").join("Local");
        let relocated = local
            .join("OpenAI")
            .join("Codex")
            .join("bin")
            .join("current-version-hash")
            .join("codex.exe");
        executable(&relocated);
        let path_directory = temp.path().join("path");
        let path_alias = path_directory.join("codex");
        executable(&path_alias);

        let resolved = resolve_with(
            CliProgram::Codex,
            None,
            Some(path_directory.as_os_str()),
            temp.path(),
            &temp.path().join("data"),
            &local,
            HostPlatform::Windows,
        )
        .unwrap();

        assert_eq!(resolved, relocated);
    }

    #[test]
    fn windows_msix_without_cli_cache_has_an_actionable_error() {
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().join("AppData").join("Local");
        fs::create_dir_all(local.join("Packages").join("OpenAI.Codex_2p2nqsd0c76g0")).unwrap();

        let error = resolve_with(
            CliProgram::Codex,
            None,
            None,
            temp.path(),
            &temp.path().join("data"),
            &local,
            HostPlatform::Windows,
        )
        .unwrap_err();

        assert!(error.contains("OpenAI desktop app is installed from Microsoft Store"));
        assert!(error.contains("open ChatGPT/Codex once"));
    }

    #[test]
    fn windows_recognizes_the_chatgpt_desktop_msix_package_name() {
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().join("AppData").join("Local");
        let relocated = local
            .join("Packages")
            .join("OpenAI.ChatGPT-Desktop_2p2nqsd0c76g0")
            .join("LocalCache")
            .join("Local")
            .join("OpenAI")
            .join("ChatGPT")
            .join("bin")
            .join("desktop-version-hash")
            .join("codex.exe");
        executable(&relocated);

        let resolved = resolve_with(
            CliProgram::Codex,
            None,
            None,
            temp.path(),
            &temp.path().join("data"),
            &local,
            HostPlatform::Windows,
        )
        .unwrap();

        assert_eq!(resolved, relocated);
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
