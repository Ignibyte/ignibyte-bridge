//! Session lifecycle: start/stop/status/list, metadata, and the PTY supervisor.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    os::unix::io::FromRawFd,
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

use anyhow::{anyhow, bail, Context, Result};
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
    procinfo::process_start_time,
    paths::{
        child_path, create_private_file, ensure_bridge_dir, parse_command, resolve_cwd,
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
    /// Start-time token for the supervisor PID; pairs with the PID to survive
    /// PID reuse. `None` for sessions written before this field existed.
    #[serde(default)]
    pub supervisor_start_time: Option<u64>,
    /// Start-time token for the child PID.
    #[serde(default)]
    pub child_start_time: Option<u64>,
    /// Monotonically increasing per-start counter. A restart reuses the session
    /// name but bumps this, so a stale supervisor from an earlier run cannot
    /// clobber the terminal state of a newer run (see [`mark_stopped`]).
    #[serde(default)]
    pub generation: u64,
    /// PTY geometry for this run (rows, cols). Defaults applied at start.
    #[serde(default = "default_rows")]
    pub rows: u16,
    #[serde(default = "default_cols")]
    pub cols: u16,
    pub created_at_unix: u64,
    pub updated_at_unix: u64,
    pub exit_status: Option<String>,
    /// Numeric exit code once the process has exited, used to distinguish a
    /// clean fast exit (a command that ran to completion) from a start failure.
    #[serde(default)]
    pub exit_code: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    Running,
    Stopped,
}

/// Gap between the text and the Enter keystroke in `send`, so the carriage
/// return reaches the PTY as its own read (a submit), not as a newline appended
/// to pasted text. Comfortably above the input forwarder's 20ms poll.
const SEND_ENTER_DELAY_MS: u64 = 60;

fn default_rows() -> u16 {
    SCREEN_ROWS
}

fn default_cols() -> u16 {
    SCREEN_COLS
}

pub fn start_session(
    name: &str,
    cwd: Option<PathBuf>,
    cmd: &str,
    rows: Option<u16>,
    cols: Option<u16>,
) -> Result<()> {
    let metadata = start_session_detached(name, cwd, cmd, rows, cols)?;
    print!("{}", format_start_result(&metadata));
    Ok(())
}

pub fn start_session_detached(
    name: &str,
    cwd: Option<PathBuf>,
    cmd: &str,
    rows: Option<u16>,
    cols: Option<u16>,
) -> Result<SessionMetadata> {
    validate_session_name(name)?;

    let cwd = resolve_cwd(cwd)?;

    let session_dir = session_dir(name)?;
    ensure_bridge_dir(&session_dir)?;

    // Held until this function returns (past wait_for_running_metadata) so two
    // concurrent starts of the same name cannot both initialize and spawn.
    let _start_lock = acquire_start_lock(&session_dir, name)?;

    if let Ok(metadata) = load_metadata(name) {
        if session_is_active(&metadata) {
            bail!("session '{name}' is already running");
        }
    }

    initialize_session_files(name, &cwd, cmd, rows, cols)?;

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

/// Render the result of a start request: a normal "started" line for a running
/// session, or a "ran to completion" line for a command that finished before it
/// could be reported as running (e.g. `echo`).
pub fn format_start_result(metadata: &SessionMetadata) -> String {
    if metadata.status == SessionStatus::Stopped {
        match metadata.exit_code {
            Some(code) => format!(
                "session '{}' ran to completion (exit code {code})\n",
                metadata.name
            ),
            None => format!("session '{}' finished\n", metadata.name),
        }
    } else {
        format_started_session(metadata)
    }
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

/// Acquire an exclusive, non-blocking advisory lock on the per-session
/// `start.lock` file. The returned handle holds the lock until dropped; a
/// second concurrent start of the same name fails fast instead of racing.
/// flock works across both processes (direct mode) and threads (daemon).
pub fn acquire_start_lock(session_dir: &Path, name: &str) -> Result<fs::File> {
    use std::os::unix::io::AsRawFd;

    let lock = create_private_file(&session_dir.join("start.lock"))
        .context("failed to open session start lock")?;
    // SAFETY: the fd is valid and owned by `lock` for the duration of the call.
    let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK)) {
            bail!("session '{name}' is already starting");
        }
        return Err(error).context("failed to lock session start");
    }
    Ok(lock)
}

/// Initialize a session's files and metadata for a new run, returning the run's
/// generation number (used by the supervisor to detect a later restart).
pub fn initialize_session_files(
    name: &str,
    cwd: &Path,
    cmd: &str,
    rows: Option<u16>,
    cols: Option<u16>,
) -> Result<u64> {
    let dir = session_dir(name)?;

    // Parse the command before destroying any prior logs, so a start that fails
    // to parse does not wipe a previous session's transcript.
    let command = parse_command(cmd)?;

    // Each run bumps the generation so a stale supervisor cannot mark a newer
    // run Stopped.
    let generation = load_metadata(name)
        .map(|prev| prev.generation.wrapping_add(1))
        .unwrap_or(1);

    let input = dir.join(INPUT_FIFO);
    if input.exists() {
        fs::remove_file(&input).with_context(|| format!("failed to remove {}", input.display()))?;
    }
    mkfifo(&input, Mode::from_bits_truncate(0o600))
        .with_context(|| format!("failed to create {}", input.display()))?;

    // Restarting a name preserves the previous run's logs as a single `.prev`
    // generation rather than silently discarding them.
    rotate_previous_log(&dir.join(RAW_LOG))?;
    rotate_previous_log(&dir.join(CLEAN_LOG))?;
    create_private_file(&dir.join(RAW_LOG)).context("failed to create raw log")?;
    create_private_file(&dir.join(CLEAN_LOG)).context("failed to create clean log")?;
    create_private_file(&dir.join(SCREEN_SNAPSHOT)).context("failed to create screen snapshot")?;

    let metadata = SessionMetadata {
        name: name.to_string(),
        cwd: cwd.to_path_buf(),
        command,
        status: SessionStatus::Starting,
        supervisor_pid: None,
        child_pid: None,
        supervisor_start_time: None,
        child_start_time: None,
        generation,
        rows: rows.filter(|r| *r > 0).unwrap_or(SCREEN_ROWS),
        cols: cols.filter(|c| *c > 0).unwrap_or(SCREEN_COLS),
        created_at_unix: now_unix(),
        updated_at_unix: now_unix(),
        exit_status: None,
        exit_code: None,
    };
    save_metadata(&metadata)?;
    Ok(generation)
}

/// Preserve a non-empty log as a single `.prev` generation before it is
/// recreated, so restarting a session name does not silently discard the
/// previous run's transcript.
fn rotate_previous_log(path: &Path) -> Result<()> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.len() > 0 => {
            let mut prev = path.as_os_str().to_owned();
            prev.push(".prev");
            fs::rename(path, prev)
                .with_context(|| format!("failed to rotate {}", path.display()))?;
        }
        _ => {}
    }
    Ok(())
}

pub fn wait_for_running_metadata(name: &str) -> Result<SessionMetadata> {
    for _ in 0..50 {
        thread::sleep(Duration::from_millis(100));
        let metadata = load_metadata(name)?;
        if metadata.status == SessionStatus::Running {
            return Ok(metadata);
        }
        if metadata.status == SessionStatus::Stopped {
            // A command that simply ran fast and exited cleanly is a success,
            // not a start failure.
            if metadata.exit_code == Some(0) {
                return Ok(metadata);
            }
            bail!(
                "session '{}' stopped while starting: {}",
                name,
                metadata
                    .exit_status
                    .unwrap_or_else(|| "unknown failure".to_string())
            );
        }
    }

    // Timed out. Converge without disturbing a session that raced into a good
    // state at the boundary: only act if it is still Starting, and only signal
    // a PID we can positively identify as ours (never a recycled one).
    if let Ok(metadata) = load_metadata(name) {
        match metadata.status {
            SessionStatus::Running => return Ok(metadata),
            SessionStatus::Stopped if metadata.exit_code == Some(0) => return Ok(metadata),
            SessionStatus::Stopped => bail!(
                "session '{}' stopped while starting: {}",
                name,
                metadata
                    .exit_status
                    .unwrap_or_else(|| "unknown failure".to_string())
            ),
            SessionStatus::Starting => {
                if pid_is_ours(metadata.supervisor_pid, metadata.supervisor_start_time) {
                    let _ = terminate_pid(metadata.supervisor_pid.unwrap() as i32);
                }
                if pid_is_ours(metadata.child_pid, metadata.child_start_time) {
                    let _ = terminate_pid(metadata.child_pid.unwrap() as i32);
                }
                let _ = mark_stopped(
                    name,
                    Some("start timed out".to_string()),
                    None,
                    Some(metadata.generation),
                );
            }
        }
    }
    bail!("session '{name}' did not report running within 5 seconds");
}

pub fn run_supervisor(name: &str, cwd: &Path, cmd: &str) -> Result<()> {
    validate_session_name(name)?;

    let generation = {
        let _status_lock = acquire_status_lock(&session_dir(name)?)?;
        let mut metadata = load_metadata(name)?;
        let supervisor_pid = std::process::id();
        metadata.supervisor_pid = Some(supervisor_pid);
        metadata.supervisor_start_time = process_start_time(supervisor_pid);
        metadata.updated_at_unix = now_unix();
        save_metadata(&metadata)?;
        metadata.generation
    };

    // Direct mode: the supervisor process inherited the client's environment,
    // so its own PATH is already the user's.
    match supervise_pty(name, cwd, cmd, None, generation) {
        Ok((exit_status, exit_code)) => {
            mark_stopped(name, Some(exit_status), exit_code, Some(generation))
        }
        Err(error) => mark_stopped(name, Some(error.to_string()), None, Some(generation)),
    }
}

pub fn supervise_pty(
    name: &str,
    cwd: &Path,
    cmd: &str,
    client_path: Option<&str>,
    generation: u64,
) -> Result<(String, Option<u32>)> {
    let mut command_parts = parse_command(cmd)?;
    let path = child_path(client_path);
    if let Some(path) = &path {
        if let Some(resolved) = resolve_program_path(&command_parts[0], path) {
            command_parts[0] = resolved;
        }
    }

    let session_dir = session_dir(name)?;
    // Geometry was recorded by initialize_session_files for this run.
    let (rows, cols) = load_metadata(name)
        .map(|m| (m.rows, m.cols))
        .unwrap_or((SCREEN_ROWS, SCREEN_COLS));
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
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

    let master = pair.master;
    let outcome = (|| -> Result<(String, Option<u32>)> {
        // Record the child PID (paired with its start-time token) as early as
        // possible, while still Starting, so a concurrent stop/shutdown can
        // identify and signal the child instead of orphaning it. Pair the PID
        // with its token: if the token can't be read the child has already
        // exited, so drop the PID rather than leave it unguarded by identity.
        {
            let _status_lock = acquire_status_lock(&session_dir)?;
            let mut metadata = load_metadata(name)?;
            check_run_live(&metadata, generation, name)?;
            let child_pid = child.process_id();
            match child_pid.and_then(process_start_time) {
                Some(token) => {
                    metadata.child_pid = child_pid;
                    metadata.child_start_time = Some(token);
                }
                None => {
                    metadata.child_pid = None;
                    metadata.child_start_time = None;
                }
            }
            metadata.command = command_parts;
            metadata.updated_at_unix = now_unix();
            save_metadata(&metadata)?;
        }

        // Open the FIFO reader before publishing the session as Running, so a
        // send that races the start always finds a reader (no ENXIO window).
        // O_RDWR keeps a write end open so the read side never sees EOF;
        // O_NONBLOCK lets forward_input poll the stop flag.
        let input_fifo_path = session_dir.join(INPUT_FIFO);
        let input_fifo = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&input_fifo_path)
            .with_context(|| format!("failed to open {}", input_fifo_path.display()))?;

        // Promote to Running under the status lock, re-checking that a stop or
        // restart did not intervene during startup; otherwise honor it (the
        // child is reaped by the outer guard).
        {
            let _status_lock = acquire_status_lock(&session_dir)?;
            let mut metadata = load_metadata(name)?;
            check_run_live(&metadata, generation, name)?;
            metadata.status = SessionStatus::Running;
            metadata.updated_at_unix = now_unix();
            save_metadata(&metadata)?;
        }

        // Own a dup of the master fd for the capture thread so it can be polled
        // (and thus stopped + joined) rather than blocked forever on read.
        let master_fd = master
            .as_raw_fd()
            .context("PTY master has no file descriptor")?;
        // SAFETY: master_fd is a valid open descriptor; dup returns a new owned
        // descriptor that File takes ownership of below.
        let reader_fd = unsafe { libc::dup(master_fd) };
        if reader_fd < 0 {
            return Err(std::io::Error::last_os_error()).context("failed to dup PTY master");
        }
        // SAFETY: reader_fd is freshly dup'd and owned exclusively by this File.
        let mut output_reader = unsafe { std::fs::File::from_raw_fd(reader_fd) };
        let mut input_writer = master.take_writer().context("failed to take PTY writer")?;

        let output_stop = Arc::new(AtomicBool::new(false));
        let output_dir = session_dir.clone();
        let capture_stop = Arc::clone(&output_stop);
        let output_thread = thread::spawn(move || {
            capture_output(&mut output_reader, &output_dir, &capture_stop, rows, cols)
        });

        let input_stop = Arc::new(AtomicBool::new(false));
        let forward_stop = Arc::clone(&input_stop);
        let input_thread =
            thread::spawn(move || forward_input(input_fifo, &mut input_writer, &forward_stop));

        let exit_status = child.wait().context("failed while waiting for child")?;
        let exit_code = Some(exit_status.exit_code());

        // Stop and join the input forwarder so its thread, FIFO fd, and dup'd
        // PTY master writer are released instead of leaking for the daemon's
        // lifetime.
        input_stop.store(true, Ordering::Relaxed);
        let _ = input_thread.join();
        drop(master);

        // Drain remaining output, but cap the wait: backgrounded grandchildren
        // can hold the PTY slave open so the reader never sees EOF. After the
        // cap, signal the capture thread to stop and join it unconditionally so
        // it (and its dup'd master fd) is never leaked.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !output_thread.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        output_stop.store(true, Ordering::Relaxed);
        let _ = output_thread.join();

        Ok((format!("{exit_status:?}"), exit_code))
    })();

    // On any failure after the child was spawned, reap it so it never lingers
    // as an unreaped zombie in the daemon (which would wedge stop/shutdown).
    if outcome.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    outcome
}

pub fn send_input(name: &str, text: &str, no_enter: bool) -> Result<()> {
    send_input_silent(name, text, no_enter)?;
    println!("sent input to '{name}'");

    Ok(())
}

pub fn send_input_silent(name: &str, text: &str, no_enter: bool) -> Result<()> {
    write_session_bytes(name, text.as_bytes())?;

    if !no_enter {
        // Deliver Enter as a separate write after a short gap so the PTY reader
        // sees it on its own. Interactive TUIs such as Claude Code treat a
        // carriage return arriving in the same chunk as the text as a newline
        // within pasted input (not a submit); a lone CR is an Enter keystroke.
        thread::sleep(Duration::from_millis(SEND_ENTER_DELAY_MS));
        write_session_bytes(name, b"\r")?;
    }

    Ok(())
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
    // O_NONBLOCK on open makes a missing reader fail fast with ENXIO instead of
    // blocking forever; we then clear it so the write itself blocks until fully
    // delivered rather than returning WouldBlock after a partial prefix.
    let mut fifo = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&input_fifo)
        .map_err(|error| {
            if error.raw_os_error() == Some(libc::ENXIO) {
                anyhow!("session '{name}' has no input reader; it may still be starting or its owner exited")
            } else {
                anyhow::Error::new(error)
                    .context(format!("failed to open input FIFO for '{name}'"))
            }
        })?;

    clear_nonblocking(&fifo)?;

    fifo.write_all(bytes).context("failed to write input")?;
    fifo.flush().context("failed to flush input")?;

    Ok(())
}

/// Clear `O_NONBLOCK` on an open file descriptor so subsequent writes block.
fn clear_nonblocking(file: &fs::File) -> Result<()> {
    use std::os::unix::io::AsRawFd;

    let fd = file.as_raw_fd();
    // SAFETY: `fd` is a valid descriptor owned by `file` for the call's duration.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error()).context("failed to read FIFO flags");
    }
    // SAFETY: same as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error()).context("failed to set FIFO blocking");
    }
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

/// Activity derived from `raw.log` without any extra writes: the Unix time of
/// the last captured output (the log's mtime), seconds idle since then, and the
/// total bytes captured (the log's size). Returns `None` if the log is missing.
/// Lets a caller tell whether a session is still producing output (busy) or has
/// gone quiet (e.g. an agent waiting for it to finish).
fn session_activity(name: &str) -> Option<(u64, u64, u64)> {
    let path = session_dir(name).ok()?.join(RAW_LOG);
    let metadata = fs::metadata(path).ok()?;
    let len = metadata.len();
    let last_output_unix = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    let idle_seconds = now_unix().saturating_sub(last_output_unix);
    Some((last_output_unix, idle_seconds, len))
}

pub fn print_status(name: &str) -> Result<()> {
    print!("{}", status_text(name)?);
    Ok(())
}

pub fn status_text(name: &str) -> Result<String> {
    validate_session_name(name)?;
    let metadata = load_metadata(name)?;
    let supervisor_alive = pid_is_ours(metadata.supervisor_pid, metadata.supervisor_start_time);
    let child_alive = pid_is_ours(metadata.child_pid, metadata.child_start_time);

    let mut output = String::new();
    output.push_str(&format!("name: {}\n", metadata.name));
    output.push_str(&format!("status: {:?}\n", metadata.status));
    output.push_str(&format!("cwd: {}\n", metadata.cwd.display()));
    output.push_str(&format!("command: {}\n", metadata.command.join(" ")));
    output.push_str(&format!("geometry: {}x{}\n", metadata.rows, metadata.cols));
    output.push_str(&format!("supervisor_pid: {:?}\n", metadata.supervisor_pid));
    output.push_str(&format!("supervisor_alive: {supervisor_alive}\n"));
    output.push_str(&format!("child_pid: {:?}\n", metadata.child_pid));
    output.push_str(&format!("child_alive: {child_alive}\n"));
    if let Some((last_output_unix, idle_seconds, output_bytes)) = session_activity(name) {
        output.push_str(&format!("last_output_unix: {last_output_unix}\n"));
        output.push_str(&format!("idle_seconds: {idle_seconds}\n"));
        output.push_str(&format!("output_bytes: {output_bytes}\n"));
    }
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
        let child_alive = pid_is_ours(metadata.child_pid, metadata.child_start_time);
        let idle = session_activity(&metadata.name)
            .map(|(_, idle_seconds, _)| format!("{idle_seconds}s"))
            .unwrap_or_else(|| "-".to_string());
        output.push_str(&format!(
            "{}\t{:?}\tchild_alive={}\tidle={}\tcmd={}",
            metadata.name,
            metadata.status,
            child_alive,
            idle,
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

    // Already stopped: nothing to signal. Return without re-signaling possibly
    // recycled PIDs or overwriting the recorded exit status.
    if metadata.status == SessionStatus::Stopped {
        return Ok(());
    }

    let mut errors = Vec::new();

    // Only signal a PID we can still positively identify as ours, so a PID that
    // was recycled after an unclean shutdown is never killed (nor its group).
    if pid_is_ours(metadata.child_pid, metadata.child_start_time) {
        let child_pid = metadata.child_pid.expect("pid_is_ours implies Some");
        if let Err(error) = terminate_pid(child_pid as i32) {
            errors.push(format!("child pid {child_pid}: {error}"));
        }
    }

    if pid_is_ours(metadata.supervisor_pid, metadata.supervisor_start_time) {
        let supervisor_pid = metadata.supervisor_pid.expect("pid_is_ours implies Some");
        if let Err(error) = terminate_pid(supervisor_pid as i32) {
            errors.push(format!("supervisor pid {supervisor_pid}: {error}"));
        }
    }

    if !errors.is_empty() {
        bail!("failed to stop session '{}': {}", name, errors.join("; "));
    }

    // Generation-guarded: if a restart superseded this run between our read and
    // here, do not mark the newer run Stopped. The supervisor's promote-to-
    // Running re-checks status under the same lock, so it cannot resurrect a
    // session we mark Stopped first.
    mark_stopped(
        name,
        Some("stopped by user".to_string()),
        None,
        Some(metadata.generation),
    )?;

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

/// Acquire a blocking exclusive lock that serializes session status transitions
/// (promote-to-Running in the supervisor, and mark-Stopped from the supervisor,
/// `stop`, or a start timeout). Distinct from the start lock — the start handler
/// holds that one (non-blocking) across the whole startup, so the supervisor
/// must use a separate lock it can actually acquire while a start is in flight.
/// Bail if this supervisor's run was stopped during startup or superseded by a
/// later restart (a newer generation), so it neither resurrects a stopped
/// session nor hijacks a newer run. Call under the status lock.
fn check_run_live(metadata: &SessionMetadata, generation: u64, name: &str) -> Result<()> {
    if metadata.generation != generation {
        bail!("session '{name}' was superseded by a newer start");
    }
    if metadata.status == SessionStatus::Stopped {
        bail!("session '{name}' was stopped during startup");
    }
    Ok(())
}

fn acquire_status_lock(session_dir: &Path) -> Result<fs::File> {
    use std::os::unix::io::AsRawFd;

    let lock = create_private_file(&session_dir.join("status.lock"))
        .context("failed to open session status lock")?;
    // SAFETY: the fd is valid and owned by `lock` for the duration of the call.
    let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to lock session status");
    }
    Ok(lock)
}

/// Transition a session to Stopped under the status lock.
///
/// `generation` guards against a stale supervisor from an earlier run clobbering
/// a newer run of the same name: when `Some(g)`, the write is skipped unless the
/// on-disk generation still equals `g`. An already-Stopped session keeps its
/// first-recorded exit info (first writer wins).
pub fn mark_stopped(
    name: &str,
    exit_status: Option<String>,
    exit_code: Option<u32>,
    generation: Option<u64>,
) -> Result<()> {
    let dir = session_dir(name)?;
    let _status_lock = acquire_status_lock(&dir)?;

    let mut metadata = load_metadata(name)?;
    if let Some(generation) = generation {
        if metadata.generation != generation {
            return Ok(());
        }
    }
    if metadata.status == SessionStatus::Stopped {
        return Ok(());
    }
    metadata.status = SessionStatus::Stopped;
    metadata.updated_at_unix = now_unix();
    metadata.exit_status = exit_status;
    metadata.exit_code = exit_code;
    save_metadata(&metadata)
}

pub fn load_metadata(name: &str) -> Result<SessionMetadata> {
    let path = session_dir(name)?.join(METADATA);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("no such session '{name}'");
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
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

/// True if `pid` is alive AND, when a start-time token was recorded, still
/// carries that token — so a recycled PID (after an unclean shutdown) is not
/// mistaken for this session's process. With no recorded token (older metadata
/// or an unsupported platform) this falls back to a bare liveness probe.
pub fn pid_is_ours(pid: Option<u32>, recorded_start_time: Option<u64>) -> bool {
    let Some(pid) = pid else {
        return false;
    };
    if !process_alive(pid as i32) {
        return false;
    }
    match recorded_start_time {
        Some(token) => process_start_time(pid) == Some(token),
        None => true,
    }
}

/// True if the session is still owned by a live process of ours (supervisor in
/// direct mode, child in daemon mode). Shared by both start paths so direct and
/// daemon mode agree on whether a name is in use.
pub fn session_is_active(metadata: &SessionMetadata) -> bool {
    metadata.status == SessionStatus::Running
        && (pid_is_ours(metadata.supervisor_pid, metadata.supervisor_start_time)
            || pid_is_ours(metadata.child_pid, metadata.child_start_time))
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_metadata() -> SessionMetadata {
        SessionMetadata {
            name: "demo".to_string(),
            cwd: PathBuf::from("/tmp"),
            command: vec!["cat".to_string()],
            status: SessionStatus::Running,
            supervisor_pid: Some(123),
            child_pid: Some(124),
            supervisor_start_time: Some(42),
            child_start_time: Some(43),
            generation: 1,
            rows: 40,
            cols: 140,
            created_at_unix: 1000,
            updated_at_unix: 1001,
            exit_status: None,
            exit_code: None,
        }
    }

    #[test]
    fn metadata_round_trips_through_json() {
        let metadata = sample_metadata();
        let json = serde_json::to_string(&metadata).unwrap();
        let back: SessionMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, metadata.name);
        assert_eq!(back.status, SessionStatus::Running);
        assert_eq!(back.child_start_time, Some(43));
        assert_eq!(back.exit_code, None);
    }

    #[test]
    fn metadata_tolerates_legacy_json_without_new_fields() {
        // A file written before start-time/exit-code fields existed.
        let legacy = r#"{
            "name":"old","cwd":"/tmp","command":["cat"],"status":"running",
            "supervisor_pid":1,"child_pid":2,
            "created_at_unix":1,"updated_at_unix":1,"exit_status":null
        }"#;
        let metadata: SessionMetadata = serde_json::from_str(legacy).unwrap();
        assert_eq!(metadata.supervisor_start_time, None);
        assert_eq!(metadata.exit_code, None);
    }

    #[test]
    fn status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&SessionStatus::Starting).unwrap(),
            "\"starting\""
        );
    }

    #[test]
    fn pid_is_ours_requires_liveness_and_matching_token() {
        let own = std::process::id();
        let token = process_start_time(own);
        // Live process with the correct token is ours.
        assert!(pid_is_ours(Some(own), token));
        // Live process with a wrong token is an impostor (PID reuse).
        assert!(!pid_is_ours(Some(own), Some(u64::MAX)));
        // No recorded token falls back to a bare liveness probe.
        assert!(pid_is_ours(Some(own), None));
        // No PID at all is never ours.
        assert!(!pid_is_ours(None, token));
    }

    #[test]
    fn session_is_active_tracks_status_and_identity() {
        let own = std::process::id();
        let token = process_start_time(own);

        let mut metadata = sample_metadata();
        metadata.supervisor_pid = Some(own);
        metadata.supervisor_start_time = token;
        metadata.child_pid = None;
        metadata.child_start_time = None;
        assert!(session_is_active(&metadata));

        // Stopped is never active even with a live PID.
        metadata.status = SessionStatus::Stopped;
        assert!(!session_is_active(&metadata));

        // Running but with a reused PID (wrong token) is not active.
        metadata.status = SessionStatus::Running;
        metadata.supervisor_start_time = Some(u64::MAX);
        assert!(!session_is_active(&metadata));
    }

    #[test]
    fn format_start_result_distinguishes_running_from_finished() {
        let mut metadata = sample_metadata();
        assert!(format_start_result(&metadata).contains("started session 'demo'"));

        metadata.status = SessionStatus::Stopped;
        metadata.exit_code = Some(0);
        let finished = format_start_result(&metadata);
        assert!(finished.contains("ran to completion"));
        assert!(finished.contains("exit code 0"));
    }
}
