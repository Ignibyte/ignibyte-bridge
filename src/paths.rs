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
    if name.len() > 128 {
        bail!("session name must be 128 characters or fewer");
    }

    let valid = name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid {
        bail!("session name may only contain letters, numbers, '.', '-', and '_'");
    }

    // '.' and '..' escape the per-session directory (session_dir("..") is the
    // bridge root itself); a leading '-' is parsed as a flag when the name is
    // passed to the supervisor's argv.
    if name == "." || name == ".." {
        bail!("session name may not be '.' or '..'");
    }
    if name.starts_with('-') {
        bail!("session name may not start with '-'");
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

/// Compute the PATH a session's child should run with. `client_path`, when set
/// (daemon mode, forwarded from the client), is used as the base instead of the
/// current process's PATH (the daemon's); `~/.local/bin` is prepended either
/// way so bare command names resolve like the user's login shell.
pub fn child_path(client_path: Option<&str>) -> Option<String> {
    match client_path {
        Some(path) => {
            let home = BaseDirs::new()?;
            Some(path_with_local_bin_from(path, home.home_dir()))
        }
        None => path_with_local_bin(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn validate_session_name_accepts_safe_names() {
        for name in ["a", "claude-main", "py_3", "v1.2", "ABC.def-123"] {
            assert!(validate_session_name(name).is_ok(), "{name} should be valid");
        }
    }

    #[test]
    fn validate_session_name_rejects_traversal_and_flags() {
        for name in ["", ".", "..", "-foo", "a/b", "a b", "naïve", "a\0b"] {
            assert!(
                validate_session_name(name).is_err(),
                "{name:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_session_name_rejects_overlong() {
        assert!(validate_session_name(&"a".repeat(129)).is_err());
        assert!(validate_session_name(&"a".repeat(128)).is_ok());
    }

    #[test]
    fn bridge_root_from_requires_absolute_override() {
        let abs = bridge_root_from(Some(OsString::from("/tmp/ab-x"))).unwrap();
        assert_eq!(abs, PathBuf::from("/tmp/ab-x"));
        assert!(bridge_root_from(Some(OsString::from("relative/dir"))).is_err());
    }

    #[test]
    fn path_with_local_bin_prepends_and_dedupes() {
        let home = Path::new("/home/user");
        let result = path_with_local_bin_from("/usr/bin:/bin", home);
        assert_eq!(result, "/home/user/.local/bin:/usr/bin:/bin");

        // An existing ~/.local/bin entry is not duplicated.
        let result = path_with_local_bin_from("/home/user/.local/bin:/usr/bin", home);
        assert_eq!(result, "/home/user/.local/bin:/usr/bin");

        // Empty PATH yields just the local bin.
        assert_eq!(path_with_local_bin_from("", home), "/home/user/.local/bin");
    }

    #[test]
    fn resolve_program_path_passes_through_explicit_paths() {
        assert_eq!(
            resolve_program_path("/usr/bin/env", "/bin"),
            Some("/usr/bin/env".to_string())
        );
    }

    #[test]
    fn resolve_program_path_searches_path_for_executables() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("mytool");
        fs::write(&bin, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

        let path = format!("/nonexistent:{}", dir.path().display());
        assert_eq!(
            resolve_program_path("mytool", &path),
            Some(bin.to_string_lossy().to_string())
        );
        // A non-executable file of the same name is skipped.
        let plain = dir.path().join("plain");
        fs::write(&plain, "x").unwrap();
        assert_eq!(resolve_program_path("plain", &path), None);
    }

    #[test]
    fn command_is_claude_matches_basename_only() {
        assert!(command_is_claude("claude"));
        assert!(command_is_claude("/home/user/.local/bin/claude"));
        assert!(!command_is_claude("claude-extra"));
        assert!(!command_is_claude("/opt/claudette"));
    }

    #[test]
    fn write_atomic_replaces_content_with_private_perms() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("meta.json");
        write_atomic(&target, b"first").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"first");
        write_atomic(&target, b"second").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"second");

        let mode = fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "atomic write must be owner-only");

        // No temp files are left behind.
        let leftovers = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn create_private_file_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        create_private_file(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn ensure_private_dir_creates_owner_only_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        ensure_private_dir(&nested).unwrap();
        let mode = fs::metadata(&nested).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        // Second call on an existing dir is fine.
        ensure_private_dir(&nested).unwrap();
    }

    #[test]
    fn resolve_cwd_canonicalizes_and_rejects_files() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve_cwd(Some(dir.path().to_path_buf())).unwrap();
        assert!(resolved.is_absolute());
        assert!(resolved.is_dir());

        let file = dir.path().join("afile");
        fs::write(&file, "x").unwrap();
        assert!(resolve_cwd(Some(file)).is_err());
    }

    #[test]
    fn parse_command_splits_and_rejects_empty() {
        assert_eq!(parse_command("python3 -i").unwrap(), vec!["python3", "-i"]);
        assert_eq!(
            parse_command("sh -c 'echo hi'").unwrap(),
            vec!["sh", "-c", "echo hi"]
        );
        assert!(parse_command("   ").is_err());
    }
}
