//! Storage roots, session-name validation, and command/PATH resolution.

use std::{
    ffi::OsString,
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
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
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            bail!(
                "AGENT_BRIDGE_HOME must be an absolute path, got {}",
                root.display()
            );
        }
        return Ok(root);
    }

    let base_dirs = BaseDirs::new().ok_or_else(|| anyhow!("failed to locate home directory"))?;
    Ok(base_dirs.home_dir().join(".agent-bridge"))
}

/// Create `path` (and missing parents) as a private directory and verify it is
/// safe to use: a real directory (not a symlink someone planted), owned by the
/// current user, with owner-only permissions. The post-creation verification
/// closes pre-creation/symlink races in shared locations like /tmp.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("failed to create {}", path.display()))?;

    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to use {}: it is a symlink (possible tampering)",
            path.display()
        );
    }
    if !metadata.is_dir() {
        bail!("refusing to use {}: not a directory", path.display());
    }
    // SAFETY: geteuid has no failure modes or memory effects.
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != euid {
        bail!(
            "refusing to use {}: owned by uid {} instead of current uid {}",
            path.display(),
            metadata.uid(),
            euid
        );
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to restrict permissions on {}", path.display()))?;
    }

    Ok(())
}

/// Validate and privately create the bridge root, then `dir`, validating every
/// intermediate component down from the root. Catches a symlinked or
/// foreign-owned component anywhere in the chain, not just the leaf.
pub fn ensure_bridge_dir(dir: &Path) -> Result<()> {
    let root = bridge_root()?;
    ensure_private_dir(&root)?;

    let relative = dir.strip_prefix(&root).with_context(|| {
        format!(
            "{} is not inside the bridge root {}",
            dir.display(),
            root.display()
        )
    })?;

    let mut current = root;
    for component in relative.components() {
        current = current.join(component);
        ensure_private_dir(&current)?;
    }

    Ok(())
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `contents` to `path` atomically: a private temp file in the same
/// directory is written then `rename`d over the destination. Concurrent
/// readers always observe either the complete old or the complete new file,
/// never a truncated one, even if the writer is killed mid-write.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent directory: {}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("path has no file name: {}", path.display()))?;
    let unique = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{file_name}.tmp.{}.{unique}", std::process::id()));

    let write_result = (|| -> Result<()> {
        let mut file = create_private_file(&tmp)?;
        file.write_all(contents)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        file.flush()
            .with_context(|| format!("failed to flush {}", tmp.display()))?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }

    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(error)
            .with_context(|| format!("failed to replace {}", path.display()));
    }

    Ok(())
}

/// Create (or truncate) a file readable and writable only by the owner,
/// repairing the mode if the file already exists with looser permissions.
/// `O_NOFOLLOW` rejects a planted symlink in place of the file.
pub fn create_private_file(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;

    let mode = file
        .metadata()
        .with_context(|| format!("failed to inspect {}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to restrict permissions on {}", path.display()))?;
    }

    Ok(file)
}

pub fn session_dir(name: &str) -> Result<PathBuf> {
    validate_session_name(name)?;
    Ok(sessions_root()?.join(name))
}

/// Resolve a session working directory to a canonical absolute path. Resolving
/// in the client (before a request is routed to the daemon) keeps `--cwd`
/// relative to the user's shell rather than the daemon's working directory.
pub fn resolve_cwd(cwd: Option<PathBuf>) -> Result<PathBuf> {
    let cwd = match cwd {
        Some(path) => path,
        None => std::env::current_dir().context("failed to read current directory")?,
    };
    let canonical = cwd
        .canonicalize()
        .with_context(|| format!("failed to canonicalize cwd {}", cwd.display()))?;
    if !canonical.is_dir() {
        bail!("cwd is not a directory: {}", canonical.display());
    }
    Ok(canonical)
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
