//! Launches managed CLI profiles in a visible terminal. Platform adapters build
//! argument-safe command specifications; this module only translates them to the
//! host terminal without putting credentials in command-line arguments.

#[cfg(any(target_os = "macos", test))]
use std::borrow::Cow;
#[cfg(any(all(unix, not(target_os = "macos")), windows))]
use std::process::Command;
use std::process::Stdio;
#[cfg(target_os = "macos")]
use std::time::Duration;

use crate::error::{AppError, AppResult};
use crate::platform::CommandSpec;

pub fn spawn(spec: CommandSpec) -> AppResult<()> {
    let mut command = spec.into_command();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| AppError::Command(error.to_string()))
}

#[cfg(target_os = "macos")]
pub fn spawn_terminal(spec: CommandSpec) -> AppResult<()> {
    let shell_command = render_unix_shell_command(&spec)?;
    let script = format!(
        "tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell",
        escape_applescript_string(&shell_command)
    );
    let (status, _, stderr) = crate::process::run_with_timeout(
        std::path::Path::new("/usr/bin/osascript"),
        &["-e", &script],
        Duration::from_secs(10),
    )
    .map_err(|error| AppError::Command(format!("failed to open Terminal: {error}")))?;
    if status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&stderr);
    let detail = detail.trim();
    Err(AppError::Command(if detail.is_empty() {
        format!("Terminal automation failed with {status}")
    } else {
        format!("Terminal automation failed: {detail}")
    }))
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn spawn_terminal(spec: CommandSpec) -> AppResult<()> {
    let mut command = Command::new("x-terminal-emulator");
    command.arg("-e").arg(&spec.program).args(&spec.args);
    command.envs(&spec.env);
    for name in &spec.env_remove {
        command.env_remove(name);
    }
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| {
            AppError::Command(format!(
                "failed to open x-terminal-emulator; install a desktop terminal or configure one as the system default: {error}"
            ))
        })
}

#[cfg(windows)]
pub fn spawn_terminal(spec: CommandSpec) -> AppResult<()> {
    let mut command = Command::new("wt.exe");
    if let Some(cwd) = &spec.cwd {
        command.arg("-d").arg(cwd);
    }
    command.args([
        "powershell.exe",
        "-NoLogo",
        "-NoProfile",
        "-NoExit",
        "-Command",
        &render_powershell_command(&spec)?,
    ]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| {
            AppError::Command(format!("failed to open Windows Terminal (wt.exe): {error}"))
        })
}

#[cfg(any(windows, test))]
fn render_powershell_command(spec: &CommandSpec) -> AppResult<String> {
    let mut statements = Vec::new();
    for name in &spec.env_remove {
        statements.push(format!(
            "Remove-Item -LiteralPath {} -ErrorAction SilentlyContinue",
            powershell_quote(&format!("Env:{name}"))
        ));
    }
    for (name, value) in &spec.env {
        statements.push(format!(
            "Set-Item -LiteralPath {} -Value {}",
            powershell_quote(&format!("Env:{name}")),
            powershell_quote(value)
        ));
    }
    let program = spec
        .program
        .to_str()
        .ok_or_else(|| AppError::Validation("CLI path is not valid UTF-8".into()))?;
    let mut invocation = format!("& {}", powershell_quote(program));
    for arg in &spec.args {
        invocation.push(' ');
        invocation.push_str(&powershell_quote(arg));
    }
    statements.push(invocation);
    Ok(statements.join("; "))
}

#[cfg(any(windows, test))]
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(any(target_os = "macos", test))]
fn render_unix_shell_command(spec: &CommandSpec) -> AppResult<String> {
    let mut parts = Vec::new();
    if let Some(cwd) = &spec.cwd {
        parts.push("cd --".to_owned());
        parts.push(quote(cwd.to_str().ok_or_else(|| {
            AppError::Validation("launch path is not valid UTF-8".into())
        })?));
        parts.push("&&".to_owned());
    }
    parts.push("env".to_owned());
    for name in &spec.env_remove {
        parts.push("-u".to_owned());
        parts.push(quote(name));
    }
    for (name, value) in &spec.env {
        parts.push(quote(&format!("{name}={value}")));
    }
    parts.push(quote(spec.program.to_str().ok_or_else(|| {
        AppError::Validation("CLI path is not valid UTF-8".into())
    })?));
    parts.extend(spec.args.iter().map(|value| quote(value)));
    Ok(parts.join(" "))
}

#[cfg(any(target_os = "macos", test))]
fn quote(value: &str) -> String {
    shell_escape::unix::escape(Cow::Borrowed(value)).into_owned()
}

#[cfg(any(target_os = "macos", test))]
fn escape_applescript_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn terminal_shell_command_quotes_every_dynamic_value() {
        let spec = CommandSpec {
            program: PathBuf::from("/Applications/Codex CLI/bin/codex"),
            args: vec!["a b".into(), "$(touch /tmp/nope)".into()],
            env: BTreeMap::from([("CODEX_HOME".into(), "/tmp/profile one".into())]),
            env_remove: vec!["OPENAI_API_KEY".into()],
            cwd: Some(PathBuf::from("/tmp/project one")),
        };
        assert_eq!(
            render_unix_shell_command(&spec).unwrap(),
            "cd -- '/tmp/project one' && env -u OPENAI_API_KEY 'CODEX_HOME=/tmp/profile one' '/Applications/Codex CLI/bin/codex' 'a b' '$(touch /tmp/nope)'"
        );
    }

    #[test]
    fn applescript_escaping_does_not_change_shell_boundaries() {
        assert_eq!(escape_applescript_string("a\\b\"c"), "a\\\\b\\\"c");
    }

    #[test]
    fn powershell_command_quotes_environment_and_arguments() {
        let spec = CommandSpec {
            program: PathBuf::from(r"C:\Program Files\Codex\codex.exe"),
            args: vec!["project's name".into()],
            env: BTreeMap::from([("CODEX_HOME".into(), r"C:\Users\A B\.codex".into())]),
            env_remove: vec!["OPENAI_API_KEY".into()],
            cwd: None,
        };
        let command = render_powershell_command(&spec).unwrap();
        assert!(command.contains("'Env:OPENAI_API_KEY'"));
        assert!(command.contains("'C:\\Users\\A B\\.codex'"));
        assert!(command.contains("'project''s name'"));
    }
}
