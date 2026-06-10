//! The Unix-socket daemon: request handling and daemon-owned sessions.

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    thread,
};

use anyhow::{bail, Context, Result};

use crate::{
    paths::{
        bridge_root, ensure_bridge_dir, ensure_private_dir, resolve_cwd, session_dir,
        sessions_root, socket_path, validate_session_name,
    },
    protocol::{try_send_daemon_request, DaemonRequest, DaemonResponse},
    session::{
        format_started_session, initialize_session_files, list_sessions_text, load_metadata,
        mark_stopped, process_alive, read_output_text, read_screen_text, send_input_silent,
        send_keys_silent, status_text, stop_session_silent, supervise_pty,
        wait_for_running_metadata, SessionMetadata, SessionStatus,
    },
};

pub fn run_daemon() -> Result<()> {
    let root = bridge_root()?;
    ensure_private_dir(&root)?;

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

fn handle_daemon_stream(mut stream: UnixStream) -> Result<()> {
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .context("failed to clone daemon stream")?,
    );
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("failed to read daemon request")?;
    if line.is_empty() {
        bail!("empty daemon request");
    }

    let request: DaemonRequest =
        serde_json::from_str(&line).context("failed to parse daemon request")?;
    let shutdown_requested = matches!(request, DaemonRequest::Shutdown);
    let response = handle_daemon_request(request);
    serde_json::to_writer(&mut stream, &response).context("failed to write daemon response")?;
    stream
        .write_all(b"\n")
        .context("failed to finish daemon response")?;
    stream.flush().context("failed to flush daemon response")?;

    if shutdown_requested && response.ok {
        std::process::exit(0);
    }

    Ok(())
}

fn handle_daemon_request(request: DaemonRequest) -> DaemonResponse {
    let result = match request {
        DaemonRequest::Start { name, cwd, cmd } => start_session_in_daemon(&name, cwd, &cmd)
            .map(|metadata| format_started_session(&metadata)),
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
        Err(error) => DaemonResponse {
            ok: false,
            output: String::new(),
            error: Some(error.to_string()),
        },
    }
}

fn start_session_in_daemon(name: &str, cwd: Option<PathBuf>, cmd: &str) -> Result<SessionMetadata> {
    validate_session_name(name)?;

    let cwd = resolve_cwd(cwd)?;

    let session_dir = session_dir(name)?;
    ensure_bridge_dir(&session_dir)?;

    if let Ok(metadata) = load_metadata(name) {
        if metadata.status == SessionStatus::Running
            && metadata
                .child_pid
                .is_some_and(|pid| process_alive(pid as i32))
        {
            bail!("session '{name}' is already running");
        }
    }

    initialize_session_files(name, &cwd, cmd)?;

    let thread_name = name.to_string();
    let thread_cwd = cwd;
    let thread_cmd = cmd.to_string();
    thread::spawn(move || {
        let exit_status = match supervise_pty(&thread_name, &thread_cwd, &thread_cmd) {
            Ok(exit_status) => exit_status,
            Err(error) => error.to_string(),
        };

        if let Err(error) = mark_stopped(&thread_name, Some(exit_status)) {
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
                if metadata.status == SessionStatus::Running {
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
