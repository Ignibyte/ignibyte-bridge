//! Storage roots, session-name validation, and command/PATH resolution.

use std::{
    ffi::OsString,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, bail, Context, Result};
use directories::BaseDirs;

pub fn validate_session_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("session name cannot be empty");
    }

    let valid = name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid {
        bail!("session name may only contain letters, numbers, '.', '-', and '_'");
    }

    Ok(())
}

pub fn sessions_root() -> Result<PathBuf> {
    Ok(bridge_root()?.join("sessions"))
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(bridge_root()?.join("agent-bridge.sock"))
}

pub fn bridge_root() -> Result<PathBuf> {
    bridge_root_from(std::env::var_os("AGENT_BRIDGE_HOME"))
}

pub fn bridge_root_from(override_root: Option<OsString>) -> Result<PathBuf> {
    if let Some(root) = override_root {
        return Ok(PathBuf::from(root));
    }

    let base_dirs = BaseDirs::new().ok_or_else(|| anyhow!("failed to locate home directory"))?;
    Ok(base_dirs.home_dir().join(".agent-bridge"))
}

pub fn session_dir(name: &str) -> Result<PathBuf> {
    validate_session_name(name)?;
    Ok(sessions_root()?.join(name))
}

pub fn parse_command(cmd: &str) -> Result<Vec<String>> {
    let parts = shell_words::split(cmd).context("failed to parse command")?;
    if parts.is_empty() {
        bail!("command cannot be empty");
    }
    Ok(parts)
}

pub fn path_with_local_bin() -> Option<String> {
    let base_dirs = BaseDirs::new()?;
    let current_path = std::env::var("PATH").unwrap_or_default();
    Some(path_with_local_bin_from(
        &current_path,
        base_dirs.home_dir(),
    ))
}

pub fn path_with_local_bin_from(current_path: &str, home_dir: &Path) -> String {
    let local_bin = home_dir.join(".local/bin");
    let local_bin = local_bin.to_string_lossy();

    if current_path.is_empty() {
        local_bin.to_string()
    } else {
        let rest = current_path
            .split(':')
            .filter(|entry| !entry.is_empty() && *entry != local_bin)
            .collect::<Vec<_>>()
            .join(":");

        if rest.is_empty() {
            local_bin.to_string()
        } else {
            format!("{local_bin}:{rest}")
        }
    }
}

pub fn resolve_program_path(program: &str, path: &str) -> Option<String> {
    if program.contains('/') {
        return Some(program.to_string());
    }

    path.split(':')
        .filter(|entry| !entry.is_empty())
        .map(|entry| Path::new(entry).join(program))
        .find(|candidate| is_executable(candidate))
        .map(|candidate| candidate.to_string_lossy().to_string())
}

pub fn command_is_claude(program: &str) -> bool {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "claude")
}

pub fn is_executable(path: &Path) -> bool {
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
