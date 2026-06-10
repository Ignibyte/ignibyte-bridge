//! Session lifecycle: start/stop/status/list, metadata, and the PTY supervisor.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use nix::{
    errno::Errno,
    sys::{
        signal::{kill, Signal},
        stat::Mode,
    },
    unistd::{mkfifo, Pid},
};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};

use crate::{
    keys::encode_key,
    logs::{capture_output, forward_input, tail_file, tail_lines},
    paths::{
        create_private_file, ensure_bridge_dir, parse_command, path_with_local_bin, resolve_cwd,
        resolve_program_path, session_dir, sessions_root, validate_session_name, write_atomic,
    },
    CLEAN_LOG, INPUT_FIFO, METADATA, RAW_LOG, SCREEN_COLS, SCREEN_ROWS, SCREEN_SNAPSHOT,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub name: String,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    pub status: SessionStatus,
    pub supervisor_pid: Option<u32>,
    pub child_pid: Option<u32>,
    pub created_at_unix: u64,
    pub updated_at_unix: u64,
    pub exit_status: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    Running,
    Stopped,
}

pub fn start_session(name: &str, cwd: Option<PathBuf>, cmd: &str) -> Result<()> {
    let metadata = start_session_detached(name, cwd, cmd)?;
    print!("{}", format_started_session(&metadata));
    Ok(())
}

pub fn start_session_detached(
    name: &str,
    cwd: Option<PathBuf>,
    cmd: &str,
) -> Result<SessionMetadata> {
    validate_session_name(name)?;

    let cwd = resolve_cwd(cwd)?;

    let session_dir = session_dir(name)?;
    ensure_bridge_dir(&session_dir)?;

    if let Ok(metadata) = load_metadata(name) {
        if metadata.status == SessionStatus::Running
            && metadata
                .supervisor_pid
                .is_some_and(|pid| process_alive(pid as i32))
        {
            bail!("session '{name}' is already running");
        }
    }

    initialize_session_files(name, &cwd, cmd)?;

    let current_exe = std::env::current_exe().context("failed to locate current executable")?;
    let mut supervisor = Command::new(current_exe);
    // Pass the session name last, behind `--`, so it is always treated as a
    // positional argument even if it begins with characters clap would read as
    // a flag (defense in depth alongside validate_session_name).
    supervisor
        .arg("supervisor")
        .arg("--cwd")
        .arg(&cwd)
        .arg("--cmd")
        .arg(cmd)
        .arg("--")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // SAFETY: this runs in the child process after fork and before exec. It only
    // calls setsid so the supervisor survives the short-lived CLI invocation.
    unsafe {
        supervisor.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    supervisor
        .spawn()
        .context("failed to spawn session supervisor")?;

    wait_for_running_metadata(name)
}

pub fn format_started_session(metadata: &SessionMetadata) -> String {
    format!(
        "started session '{}' (supervisor pid: {}, child pid: {})\n",
        metadata.name,
        metadata
            .supervisor_pid
            .map_or_else(|| "daemon".to_string(), |pid| pid.to_string()),
        metadata
            .child_pid
            .map_or_else(|| "unknown".to_string(), |pid| pid.to_string())
    )
}

pub fn initialize_session_files(name: &str, cwd: &Path, cmd: &str) -> Result<()> {
    let dir = session_dir(name)?;
    let input = dir.join(INPUT_FIFO);

    if input.exists() {
        fs::remove_file(&input).with_context(|| format!("failed to remove {}", input.display()))?;
    }
    mkfifo(&input, Mode::from_bits_truncate(0o600))
        .with_context(|| format!("failed to create {}", input.display()))?;

    create_private_file(&dir.join(RAW_LOG)).context("failed to create raw log")?;
    create_private_file(&dir.join(CLEAN_LOG)).context("failed to create clean log")?;
    create_private_file(&dir.join(SCREEN_SNAPSHOT)).context("failed to create screen snapshot")?;

    let metadata = SessionMetadata {
        name: name.to_string(),
        cwd: cwd.to_path_buf(),
        command: parse_command(cmd)?,
        status: SessionStatus::Starting,
        supervisor_pid: None,
        child_pid: None,
        created_at_unix: now_unix(),
        updated_at_unix: now_unix(),
        exit_status: None,
    };
    save_metadata(&metadata)
}

pub fn wait_for_running_metadata(name: &str) -> Result<SessionMetadata> {
    for _ in 0..50 {
        thread::sleep(Duration::from_millis(100));
        let metadata = load_metadata(name)?;
        if metadata.status == SessionStatus::Running {
            return Ok(metadata);
        }
        if metadata.status == SessionStatus::Stopped {
            bail!(
                "session '{}' stopped while starting: {}",
                name,
                metadata
                    .exit_status
                    .unwrap_or_else(|| "unknown failure".to_string())
            );
        }
    }

    bail!("session '{name}' did not report running within 5 seconds");
}

pub fn run_supervisor(name: &str, cwd: &Path, cmd: &str) -> Result<()> {
    validate_session_name(name)?;

    let mut metadata = load_metadata(name)?;
    metadata.supervisor_pid = Some(std::process::id());
    metadata.updated_at_unix = now_unix();
    save_metadata(&metadata)?;

    match supervise_pty(name, cwd, cmd) {
        Ok(exit_status) => mark_stopped(name, Some(exit_status)),
        Err(error) => mark_stopped(name, Some(error.to_string())),
    }
}

pub fn supervise_pty(name: &str, cwd: &Path, cmd: &str) -> Result<String> {
    let mut command_parts = parse_command(cmd)?;
    let path = path_with_local_bin();
    if let Some(path) = &path {
        if let Some(resolved) = resolve_program_path(&command_parts[0], path) {
            command_parts[0] = resolved;
        }
    }

    let session_dir = session_dir(name)?;
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: SCREEN_ROWS,
            cols: SCREEN_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open PTY")?;

    let mut command = CommandBuilder::new(&command_parts[0]);
    for arg in &command_parts[1..] {
        command.arg(arg);
    }
    command.cwd(cwd);
    command.env("TERM", "xterm-256color");
    command.env_remove("CLAUDECODE");
    command.env_remove("CLAUDE_CODE_ENTRY_POINT");
    if let Some(path) = path {
        command.env("PATH", path);
    }

    let mut child = pair
        .slave
        .spawn_command(command)
        .context("failed to spawn command in PTY")?;
    drop(pair.slave);

    let mut metadata = load_metadata(name)?;
    metadata.command = command_parts;
    metadata.status = SessionStatus::Running;
    metadata.child_pid = child.process_id();
    metadata.updated_at_unix = now_unix();
    save_metadata(&metadata)?;

    let mut output_reader = pair
        .master
        .try_clone_reader()
        .context("failed to clone PTY reader")?;
    let mut input_writer = pair
        .master
        .take_writer()
        .context("failed to take PTY writer")?;

    let output_dir = session_dir.clone();
    let output_thread = thread::spawn(move || capture_output(&mut output_reader, &output_dir));

    let stop = Arc::new(AtomicBool::new(false));
    let input_fifo = session_dir.join(INPUT_FIFO);
    let input_stop = Arc::clone(&stop);
    let input_thread =
        thread::spawn(move || forward_input(&input_fifo, &mut input_writer, &input_stop));

    let exit_status = child.wait().context("failed while waiting for child")?;

    // Stop and join the input forwarder so its thread, FIFO fd, and dup'd PTY
    // master writer are released instead of leaking for the daemon's lifetime.
    stop.store(true, Ordering::Relaxed);
    let _ = input_thread.join();
    drop(pair.master);

    // Drain remaining output, but cap the wait: backgrounded grandchildren can
    // hold the PTY slave open so the reader never sees EOF. Returning anyway
    // lets the caller mark the session Stopped instead of blocking forever.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !output_thread.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }

    Ok(format!("{exit_status:?}"))
}

pub fn send_input(name: &str, text: &str, no_enter: bool) -> Result<()> {
    send_input_silent(name, text, no_enter)?;
    println!("sent input to '{name}'");

    Ok(())
}

pub fn send_input_silent(name: &str, text: &str, no_enter: bool) -> Result<()> {
    let mut bytes = text.as_bytes().to_vec();
    if !no_enter {
        bytes.push(b'\r');
    }

    write_session_bytes(name, &bytes)
}

pub fn send_keys(name: &str, keys: &[String]) -> Result<()> {
    send_keys_silent(name, keys)?;
    println!("sent keys to '{name}': {}", keys.join(" "));

    Ok(())
}

pub fn send_keys_silent(name: &str, keys: &[String]) -> Result<()> {
    if keys.is_empty() {
        bail!("at least one key is required");
    }

    let mut bytes = Vec::new();
    for key in keys {
        bytes.extend(encode_key(key)?);
    }

    write_session_bytes(name, &bytes)?;

    Ok(())
}

pub fn write_session_bytes(name: &str, bytes: &[u8]) -> Result<()> {
    validate_session_name(name)?;
    let metadata = load_metadata(name)?;
    if metadata.status != SessionStatus::Running {
        bail!("session '{name}' is not running");
    }

    let input_fifo = session_dir(name)?.join(INPUT_FIFO);
    let mut fifo = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&input_fifo)
        .with_context(|| format!("failed to open input FIFO for '{}'", name))?;

    fifo.write_all(bytes).context("failed to write input")?;
    fifo.flush().context("failed to flush input")?;

    Ok(())
}

pub fn read_output(name: &str, tail: usize, raw: bool) -> Result<()> {
    print!("{}", read_output_text(name, tail, raw)?);
    Ok(())
}

pub fn read_output_text(name: &str, tail: usize, raw: bool) -> Result<String> {
    validate_session_name(name)?;
    let path = session_dir(name)?.join(if raw { RAW_LOG } else { CLEAN_LOG });
    tail_file(&path, tail)
}

pub fn read_screen(name: &str, tail: usize) -> Result<()> {
    print!("{}", read_screen_text(name, tail)?);
    Ok(())
}

pub fn read_screen_text(name: &str, tail: usize) -> Result<String> {
    validate_session_name(name)?;
    let path = session_dir(name)?.join(SCREEN_SNAPSHOT);
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;

    Ok(tail_lines(&contents, tail))
}

pub fn print_status(name: &str) -> Result<()> {
    print!("{}", status_text(name)?);
    Ok(())
}

pub fn status_text(name: &str) -> Result<String> {
    validate_session_name(name)?;
    let metadata = load_metadata(name)?;
    let supervisor_alive = metadata
        .supervisor_pid
        .is_some_and(|pid| process_alive(pid as i32));
    let child_alive = metadata
        .child_pid
        .is_some_and(|pid| process_alive(pid as i32));

    let mut output = String::new();
    output.push_str(&format!("name: {}\n", metadata.name));
    output.push_str(&format!("status: {:?}\n", metadata.status));
    output.push_str(&format!("cwd: {}\n", metadata.cwd.display()));
    output.push_str(&format!("command: {}\n", metadata.command.join(" ")));
    output.push_str(&format!("supervisor_pid: {:?}\n", metadata.supervisor_pid));
    output.push_str(&format!("supervisor_alive: {supervisor_alive}\n"));
    output.push_str(&format!("child_pid: {:?}\n", metadata.child_pid));
    output.push_str(&format!("child_alive: {child_alive}\n"));
    if let Some(exit_status) = metadata.exit_status {
        output.push_str(&format!("exit_status: {exit_status}\n"));
    }

    Ok(output)
}

pub fn list_sessions() -> Result<()> {
    print!("{}", list_sessions_text()?);
    Ok(())
}

pub fn list_sessions_text() -> Result<String> {
    let sessions_dir = sessions_root()?;
    ensure_bridge_dir(&sessions_dir)?;

    let mut sessions = Vec::new();
    for entry in fs::read_dir(&sessions_dir)
        .with_context(|| format!("failed to read {}", sessions_dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Ok(metadata) = load_metadata(&name) {
                sessions.push(metadata);
            }
        }
    }

    sessions.sort_by(|left, right| left.name.cmp(&right.name));

    if sessions.is_empty() {
        return Ok("no sessions\n".to_string());
    }

    let mut output = String::new();
    for metadata in sessions {
        let child_alive = metadata
            .child_pid
            .is_some_and(|pid| process_alive(pid as i32));
        output.push_str(&format!(
            "{}\t{:?}\tchild_alive={}\tcmd={}",
            metadata.name,
            metadata.status,
            child_alive,
            metadata.command.join(" ")
        ));
        output.push('\n');
    }

    Ok(output)
}

pub fn stop_session(name: &str) -> Result<()> {
    stop_session_silent(name)?;
    println!("stopped session '{name}'");

    Ok(())
}

pub fn stop_session_silent(name: &str) -> Result<()> {
    validate_session_name(name)?;
    let metadata = load_metadata(name)?;
    let mut errors = Vec::new();

    if let Some(child_pid) = metadata.child_pid {
        if let Err(error) = terminate_pid(child_pid as i32) {
            errors.push(format!("child pid {child_pid}: {error}"));
        }
    }

    if let Some(supervisor_pid) = metadata.supervisor_pid {
        if let Err(error) = terminate_pid(supervisor_pid as i32) {
            errors.push(format!("supervisor pid {supervisor_pid}: {error}"));
        }
    }

    if !errors.is_empty() {
        bail!("failed to stop session '{}': {}", name, errors.join("; "));
    }

    mark_stopped(name, Some("stopped by user".to_string()))?;

    Ok(())
}

pub fn terminate_pid(pid: i32) -> Result<()> {
    if pid <= 0 {
        return Ok(());
    }

    let term_sent = signal_pid_and_group(pid, Signal::SIGTERM)?;

    for _ in 0..20 {
        if !process_alive(pid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }

    let kill_sent = signal_pid_and_group(pid, Signal::SIGKILL)?;
    if !term_sent && !kill_sent {
        return Ok(());
    }

    for _ in 0..20 {
        if !process_alive(pid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }

    bail!("process did not exit after SIGTERM and SIGKILL");
}

pub fn signal_pid_and_group(pid: i32, signal: Signal) -> Result<bool> {
    let mut sent = false;
    let mut errors = Vec::new();

    for target in [Pid::from_raw(-pid), Pid::from_raw(pid)] {
        match kill(target, signal) {
            Ok(()) => sent = true,
            Err(Errno::ESRCH) => {}
            Err(error) => errors.push(format!("{target}: {error}")),
        }
    }

    if !sent && !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }

    Ok(sent)
}

pub fn mark_stopped(name: &str, exit_status: Option<String>) -> Result<()> {
    let mut metadata = load_metadata(name)?;
    metadata.status = SessionStatus::Stopped;
    metadata.updated_at_unix = now_unix();
    metadata.exit_status = exit_status;
    save_metadata(&metadata)
}

pub fn load_metadata(name: &str) -> Result<SessionMetadata> {
    let path = session_dir(name)?.join(METADATA);
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
}

pub fn save_metadata(metadata: &SessionMetadata) -> Result<()> {
    let path = session_dir(&metadata.name)?.join(METADATA);
    let mut next = metadata.clone();
    next.updated_at_unix = now_unix();
    let contents = serde_json::to_string_pretty(&next).context("failed to serialize metadata")?;
    write_atomic(&path, contents.as_bytes())
}

pub fn process_alive(pid: i32) -> bool {
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
