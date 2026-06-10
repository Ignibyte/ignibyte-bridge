//! The Unix-socket daemon: request handling and daemon-owned sessions.

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::PermissionsExt,
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    thread,
    time::Duration,
};

use anyhow::{bail, Context, Result};

use crate::{
    paths::{
        bridge_root, create_private_file, ensure_bridge_dir, ensure_private_dir, resolve_cwd,
        session_dir, sessions_root, socket_path, validate_session_name,
    },
    protocol::{try_send_daemon_request, DaemonRequest, DaemonResponse},
    session::{
        acquire_start_lock, format_start_result, initialize_session_files, list_sessions_text,
        load_metadata, mark_stopped, read_output_text, read_screen_text, send_input_silent,
        send_keys_silent, session_is_active, status_text, stop_session_silent, supervise_pty,
        wait_for_running_metadata, SessionMetadata, SessionStatus,
    },
};

/// Maximum bytes accepted for a single request line before the daemon gives up,
/// so a client that never sends a newline cannot make read_line buffer without
/// bound.
const MAX_REQUEST_BYTES: u64 = 1 << 20;
/// How long a connected client has to send its request / receive its response
/// before the handler thread gives up, so an idle client cannot pin a thread.
const DAEMON_IO_TIMEOUT: Duration = Duration::from_secs(30);

pub fn run_daemon() -> Result<()> {
    let root = bridge_root()?;
    ensure_private_dir(&root)?;

    // Hold an exclusive lock for the daemon's lifetime so two daemons cannot
    // race the stale-socket check/remove/bind and unlink each other's socket.
    // flock is released automatically when the process exits, covering crashes.
    let _daemon_lock = acquire_daemon_lock(&root)?;

    let path = socket_path()?;
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            bail!("daemon already appears to be running at {}", path.display());
        }
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove stale socket {}", path.display()))?;
    }

    let listener =
        UnixListener::bind(&path).with_context(|| format!("failed to bind {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to chmod {}", path.display()))?;

    println!("agent-bridge daemon listening on {}", path.display());
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(move || {
                    if let Err(error) = handle_daemon_stream(stream) {
                        eprintln!("daemon request failed: {error:#}");
                    }
                });
            }
            Err(error) => eprintln!("daemon accept failed: {error}"),
        }
    }

    Ok(())
}

/// Acquire the daemon-lifetime lock on `<root>/daemon.lock`.
fn acquire_daemon_lock(root: &std::path::Path) -> Result<fs::File> {
    use std::os::unix::io::AsRawFd;

    let lock = create_private_file(&root.join("daemon.lock"))
        .context("failed to open daemon lock")?;
    // SAFETY: the fd is valid and owned by `lock` for the duration of the call.
    let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK)) {
            bail!("another agent-bridge daemon is already running or starting");
        }
        return Err(error).context("failed to lock daemon");
    }
    Ok(lock)
}

fn handle_daemon_stream(mut stream: UnixStream) -> Result<()> {
    // Bound how long an idle or slow client can hold this handler thread.
    stream
        .set_read_timeout(Some(DAEMON_IO_TIMEOUT))
        .context("failed to set read timeout")?;
    stream
        .set_write_timeout(Some(DAEMON_IO_TIMEOUT))
        .context("failed to set write timeout")?;

    let mut reader = BufReader::new(
        stream
            .try_clone()
            .context("failed to clone daemon stream")?
            .take(MAX_REQUEST_BYTES),
    );
    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .context("failed to read daemon request")?;

    // A zero-byte read is a bare connect-and-close (e.g. the startup liveness
    // probe); answer nothing and move on rather than logging a spurious error.
    if read == 0 {
        return Ok(());
    }

    let request: DaemonRequest = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(error) => {
            // Reply with the parse error instead of dropping the connection, so
            // a client (or a version-skewed build) sees why, not a bare EOF.
            let response = DaemonResponse {
                ok: false,
                output: String::new(),
                error: Some(format!("invalid daemon request: {error}")),
            };
            write_daemon_response(&mut stream, &response)?;
            return Ok(());
        }
    };

    if matches!(request, DaemonRequest::Shutdown) {
        // Acknowledge first, then stop sessions, so the client is never held for
        // the (potentially multi-second, serial) per-session termination and
        // never times out waiting for the ack.
        let response = DaemonResponse {
            ok: true,
            output: "daemon shutting down\n".to_string(),
            error: None,
        };
        write_daemon_response(&mut stream, &response)?;
        if let Err(error) = shutdown_sessions_for_daemon() {
            eprintln!("daemon shutdown: {error:#}");
        }
        // Remove the socket so a clean restart does not depend on the
        // stale-socket cleanup path. The daemon lock releases on exit.
        if let Ok(path) = socket_path() {
            let _ = fs::remove_file(path);
        }
        std::process::exit(0);
    }

    let response = handle_daemon_request(request);
    write_daemon_response(&mut stream, &response)?;

    Ok(())
}

fn write_daemon_response(stream: &mut UnixStream, response: &DaemonResponse) -> Result<()> {
    serde_json::to_writer(&mut *stream, response).context("failed to write daemon response")?;
    stream
        .write_all(b"\n")
        .context("failed to finish daemon response")?;
    stream.flush().context("failed to flush daemon response")?;
    Ok(())
}

fn handle_daemon_request(request: DaemonRequest) -> DaemonResponse {
    let result = match request {
        DaemonRequest::Start {
            name,
            cwd,
            cmd,
            path,
            rows,
            cols,
        } => start_session_in_daemon(&name, cwd, &cmd, path, rows, cols)
            .map(|metadata| format_start_result(&metadata)),
        DaemonRequest::Send {
            name,
            text,
            no_enter,
        } => {
            send_input_silent(&name, &text, no_enter).map(|()| format!("sent input to '{name}'\n"))
        }
        DaemonRequest::Keys { name, keys } => {
            let summary = keys.join(" ");
            send_keys_silent(&name, &keys).map(|()| format!("sent keys to '{name}': {summary}\n"))
        }
        DaemonRequest::Read { name, tail, raw } => read_output_text(&name, tail, raw),
        DaemonRequest::Screen { name, tail } => read_screen_text(&name, tail),
        DaemonRequest::Status { name } => status_text(&name),
        DaemonRequest::List => list_sessions_text(),
        DaemonRequest::Stop { name } => {
            stop_session_silent(&name).map(|()| format!("stopped session '{name}'\n"))
        }
        DaemonRequest::Shutdown => shutdown_sessions_for_daemon().map(|mut output| {
            output.push_str("daemon shutting down\n");
            output
        }),
    };

    match result {
        Ok(output) => DaemonResponse {
            ok: true,
            output,
            error: None,
        },
        // `{:#}` includes the full anyhow cause chain so the client sees the
        // root cause (e.g. "no such session", ENXIO) not just the outer context.
        Err(error) => DaemonResponse {
            ok: false,
            output: String::new(),
            error: Some(format!("{error:#}")),
        },
    }
}

fn start_session_in_daemon(
    name: &str,
    cwd: Option<PathBuf>,
    cmd: &str,
    client_path: Option<String>,
    rows: Option<u16>,
    cols: Option<u16>,
) -> Result<SessionMetadata> {
    validate_session_name(name)?;

    let cwd = resolve_cwd(cwd)?;

    let session_dir = session_dir(name)?;
    ensure_bridge_dir(&session_dir)?;

    let _start_lock = acquire_start_lock(&session_dir, name)?;

    if let Ok(metadata) = load_metadata(name) {
        if session_is_active(&metadata) {
            bail!("session '{name}' is already running");
        }
    }

    let generation = initialize_session_files(name, &cwd, cmd, rows, cols)?;

    let thread_name = name.to_string();
    let thread_cwd = cwd;
    let thread_cmd = cmd.to_string();
    thread::spawn(move || {
        let (exit_status, exit_code) = match supervise_pty(
            &thread_name,
            &thread_cwd,
            &thread_cmd,
            client_path.as_deref(),
            generation,
        ) {
            Ok((exit_status, exit_code)) => (exit_status, exit_code),
            Err(error) => (error.to_string(), None),
        };

        if let Err(error) =
            mark_stopped(&thread_name, Some(exit_status), exit_code, Some(generation))
        {
            eprintln!("failed to mark session '{thread_name}' stopped: {error:#}");
        }
    });

    wait_for_running_metadata(name)
}

fn shutdown_sessions_for_daemon() -> Result<String> {
    let sessions_dir = sessions_root()?;
    if !sessions_dir.exists() {
        return Ok(String::new());
    }

    let mut sessions = Vec::new();
    for entry in fs::read_dir(&sessions_dir)
        .with_context(|| format!("failed to read {}", sessions_dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Ok(metadata) = load_metadata(&name) {
                // Include Starting sessions, not only Running ones, so a session
                // mid-startup is not orphaned when the daemon exits.
                if matches!(
                    metadata.status,
                    SessionStatus::Running | SessionStatus::Starting
                ) {
                    sessions.push(metadata.name);
                }
            }
        }
    }

    sessions.sort();

    let mut output = String::new();
    let mut errors = Vec::new();
    for name in sessions {
        match stop_session_silent(&name) {
            Ok(()) => output.push_str(&format!("stopped session '{name}'\n")),
            Err(error) => errors.push(format!("{name}: {error}")),
        }
    }

    if !errors.is_empty() {
        bail!("failed to stop running sessions: {}", errors.join("; "));
    }

    Ok(output)
}

pub fn shutdown_daemon_direct() -> Result<()> {
    match try_send_daemon_request(&DaemonRequest::Shutdown)? {
        Some(response) if response.ok => {
            print!("{}", response.output);
            Ok(())
        }
        Some(response) => bail!(
            "{}",
            response
                .error
                .unwrap_or_else(|| "daemon shutdown failed".to_string())
        ),
        None => {
            println!("daemon not running");
            Ok(())
        }
    }
}
