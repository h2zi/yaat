//! Bounded child-process execution and client-liveness checks.

use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use yaat_contracts::Platform;

use crate::error::{AppError, AppResult};

const MAX_COMMAND_OUTPUT_BYTES: u64 = 64 * 1024;

/// Prevent background probes and helper commands from creating a transient
/// console window when YAAT is running as a Windows GUI application.
pub fn configure_background(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

pub fn run_with_timeout(
    program: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), String> {
    let mut command = Command::new(program);
    configure_background(&mut command);
    let mut child = command
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
                return Err(format!(
                    "{} did not answer within {} seconds",
                    program.display(),
                    timeout.as_secs()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "failed while waiting for {}: {error}",
                    program.display()
                ));
            }
        }
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.by_ref()
            .take(MAX_COMMAND_OUTPUT_BYTES + 1)
            .read_to_end(&mut stdout)
            .map_err(|error| format!("failed to read version output: {error}"))?;
    }
    if let Some(mut pipe) = child.stderr.take() {
        pipe.by_ref()
            .take(MAX_COMMAND_OUTPUT_BYTES + 1)
            .read_to_end(&mut stderr)
            .map_err(|error| format!("failed to read version error output: {error}"))?;
    }
    if stdout.len() as u64 > MAX_COMMAND_OUTPUT_BYTES
        || stderr.len() as u64 > MAX_COMMAND_OUTPUT_BYTES
    {
        return Err(format!(
            "{} returned too much version output",
            program.display()
        ));
    }
    Ok((status, stdout, stderr))
}

pub fn ensure_client_is_stopped(platform: Platform) -> AppResult<()> {
    if is_client_running(platform)? {
        return Err(AppError::ConfigConflict(format!(
            "{} is running; close all sessions before changing the global credential slot",
            display_name(platform)
        )));
    }
    Ok(())
}

#[cfg(unix)]
pub fn is_client_running(platform: Platform) -> AppResult<bool> {
    let process_name = match platform {
        Platform::Codex => "codex",
        Platform::ClaudeCode => "claude",
        Platform::ClaudeDesktop => "Claude",
    };
    let status = Command::new("pgrep")
        .args(["-x", process_name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(status) if status.success() => Ok(true),
        Ok(status) if status.code() == Some(1) => Ok(false),
        Ok(status) => Err(AppError::Command(format!(
            "process check failed with {status}"
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
pub fn is_client_running(platform: Platform) -> AppResult<bool> {
    let image_name = match platform {
        Platform::Codex => "codex.exe",
        Platform::ClaudeCode => "claude.exe",
        Platform::ClaudeDesktop => "Claude.exe",
    };
    let mut command = Command::new("tasklist");
    configure_background(&mut command);
    let output = command
        .args([
            "/FI",
            &format!("IMAGENAME eq {image_name}"),
            "/FO",
            "CSV",
            "/NH",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(AppError::Command("tasklist failed".into()));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .to_ascii_lowercase()
        .contains(&image_name.to_ascii_lowercase()))
}

pub fn ensure_codex_history_clients_stopped() -> AppResult<()> {
    if is_process_running(
        &["codex", "Codex", "ChatGPT"],
        &["codex.exe", "Codex.exe", "ChatGPT.exe"],
    )? {
        return Err(AppError::ConfigConflict(
            "Codex CLI or Codex Desktop is running; close it before writing unified session history"
                .into(),
        ));
    }
    Ok(())
}

pub fn ensure_claude_desktop_is_stopped() -> AppResult<()> {
    if is_process_running(&["Claude"], &["Claude.exe"])? {
        return Err(AppError::ConfigConflict(
            "Claude Desktop is running; close it before changing accounts or Code session history"
                .into(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn is_process_running(unix_names: &[&str], _windows_names: &[&str]) -> AppResult<bool> {
    for process_name in unix_names {
        let status = Command::new("pgrep")
            .args(["-x", process_name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match status {
            Ok(status) if status.success() => return Ok(true),
            Ok(status) if status.code() == Some(1) => {}
            Ok(status) => {
                return Err(AppError::Command(format!(
                    "process check failed with {status}"
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

#[cfg(windows)]
fn is_process_running(_unix_names: &[&str], windows_names: &[&str]) -> AppResult<bool> {
    let mut command = Command::new("tasklist");
    configure_background(&mut command);
    let output = command
        .args(["/FO", "CSV", "/NH"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(AppError::Command("tasklist failed".into()));
    }
    let stdout = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    Ok(windows_names
        .iter()
        .any(|name| stdout.contains(&name.to_ascii_lowercase())))
}

fn display_name(platform: Platform) -> &'static str {
    match platform {
        Platform::Codex => "Codex",
        Platform::ClaudeCode => "Claude Code",
        Platform::ClaudeDesktop => "Claude Desktop",
    }
}
