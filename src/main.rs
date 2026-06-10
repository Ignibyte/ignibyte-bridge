//! Local persistent PTY session controller for AI coding agents.

use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::OpenOptionsExt,
    os::unix::fs::PermissionsExt,
    os::unix::net::{UnixListener, UnixStream},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use directories::BaseDirs;
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

const INPUT_FIFO: &str = "input.fifo";
const RAW_LOG: &str = "raw.log";
const CLEAN_LOG: &str = "clean.log";
const SCREEN_SNAPSHOT: &str = "screen.txt";
const METADATA: &str = "metadata.json";
const SCREEN_ROWS: u16 = 30;
const SCREEN_COLS: u16 = 120;

#[derive(Parser)]
#[command(author, version, about)]
struct Cli {
    /// Bypass a running daemon and execute the command directly.
    #[arg(long, global = true)]
    direct: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start a named interactive PTY session.
    Start {
        /// Unique session name.
        name: String,
        /// Working directory for the command.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Command to start, for example: "python3 -i" or "claude".
        #[arg(long)]
        cmd: String,
    },
    /// Send input to an already running session.
    Send {
        /// Session name.
        name: String,
        /// Text to send to the session.
        text: String,
        /// Do not append Enter/carriage return after the text.
        #[arg(long)]
        no_enter: bool,
    },
    /// Send terminal control keys to an already running session.
    Keys {
        /// Session name.
        name: String,
        /// Keys to send, for example: enter escape ctrl-c tab up down left right.
        keys: Vec<String>,
    },
    /// Read recent captured output from a session.
    Read {
        /// Session name.
        name: String,
        /// Number of recent lines to print.
        #[arg(long, default_value_t = 300)]
        tail: usize,
        /// Read raw terminal output instead of ANSI-stripped output.
        #[arg(long)]
        raw: bool,
    },
    /// Read the current rendered terminal screen snapshot.
    Screen {
        /// Session name.
        name: String,
        /// Number of recent screen rows to print.
        #[arg(long, default_value_t = 80)]
        tail: usize,
    },
    /// Show status for one session.
    Status {
        /// Session name.
        name: String,
    },
    /// List all known sessions.
    List,
    /// Stop a session.
    Stop {
        /// Session name.
        name: String,
    },
    /// Run the local Agent Bridge daemon.
    Daemon,
    /// Ask the running daemon to stop all sessions and exit.
    Shutdown,
    /// Diagnose local environment and command resolution.
    Doctor {
        /// Command to inspect, for example: "claude".
        #[arg(long, default_value = "claude")]
        cmd: String,
        /// Working directory to inspect from.
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Internal process that owns one PTY session.
    #[command(hide = true)]
    Supervisor {
        /// Session name.
        name: String,
        /// Working directory for the command.
        #[arg(long)]
        cwd: PathBuf,
        /// Command to start.
        #[arg(long)]
        cmd: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMetadata {
    name: String,
    cwd: PathBuf,
    command: Vec<String>,
    status: SessionStatus,
    supervisor_pid: Option<u32>,
    child_pid: Option<u32>,
    created_at_unix: u64,
    updated_at_unix: u64,
    exit_status: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionStatus {
    Starting,
    Running,
    Stopped,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum DaemonRequest {
    Start {
        name: String,
        cwd: Option<PathBuf>,
        cmd: String,
    },
    Send {
        name: String,
        text: String,
        no_enter: bool,
    },
    Keys {
        name: String,
        keys: Vec<String>,
    },
    Read {
        name: String,
        tail: usize,
        raw: bool,
    },
    Screen {
        name: String,
        tail: usize,
    },
    Status {
        name: String,
    },
    List,
    Stop {
        name: String,
    },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
struct DaemonResponse {
    ok: bool,
    output: String,
    error: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if !cli.direct {
        if let Some(request) = daemon_request_for_command(&cli.command) {
            if let Some(response) = try_send_daemon_request(&request)? {
                if response.ok {
                    print!("{}", response.output);
                    return Ok(());
                }

                bail!(
                    "{}",
                    response
                        .error
                        .unwrap_or_else(|| "daemon command failed".to_string())
                );
            }
        }
    }

    match cli.command {
        Commands::Start { name, cwd, cmd } => start_session(&name, cwd, &cmd),
        Commands::Send {
            name,
            text,
            no_enter,
        } => send_input(&name, &text, no_enter),
        Commands::Keys { name, keys } => send_keys(&name, &keys),
        Commands::Read { name, tail, raw } => read_output(&name, tail, raw),
        Commands::Screen { name, tail } => read_screen(&name, tail),
        Commands::Status { name } => print_status(&name),
        Commands::List => list_sessions(),
        Commands::Stop { name } => stop_session(&name),
        Commands::Daemon => run_daemon(),
        Commands::Shutdown => shutdown_daemon_direct(),
        Commands::Doctor { cmd, cwd } => doctor(&cmd, cwd),
        Commands::Supervisor { name, cwd, cmd } => run_supervisor(&name, &cwd, &cmd),
    }
}

fn daemon_request_for_command(command: &Commands) -> Option<DaemonRequest> {
    match command {
        Commands::Start { name, cwd, cmd } => Some(DaemonRequest::Start {
            name: name.clone(),
            cwd: cwd.clone(),
            cmd: cmd.clone(),
        }),
        Commands::Send {
            name,
            text,
            no_enter,
        } => Some(DaemonRequest::Send {
            name: name.clone(),
            text: text.clone(),
            no_enter: *no_enter,
        }),
        Commands::Keys { name, keys } => Some(DaemonRequest::Keys {
            name: name.clone(),
            keys: keys.clone(),
        }),
        Commands::Read { name, tail, raw } => Some(DaemonRequest::Read {
            name: name.clone(),
            tail: *tail,
            raw: *raw,
        }),
        Commands::Screen { name, tail } => Some(DaemonRequest::Screen {
            name: name.clone(),
            tail: *tail,
        }),
        Commands::Status { name } => Some(DaemonRequest::Status { name: name.clone() }),
        Commands::List => Some(DaemonRequest::List),
        Commands::Stop { name } => Some(DaemonRequest::Stop { name: name.clone() }),
        Commands::Shutdown => Some(DaemonRequest::Shutdown),
        Commands::Daemon | Commands::Doctor { .. } | Commands::Supervisor { .. } => None,
    }
}

fn try_send_daemon_request(request: &DaemonRequest) -> Result<Option<DaemonResponse>> {
    let path = socket_path()?;
    let mut stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to connect to daemon at {}", path.display()));
        }
    };

    serde_json::to_writer(&mut stream, request).context("failed to write daemon request")?;
    stream
        .write_all(b"\n")
        .context("failed to finish daemon request")?;
    stream.flush().context("failed to flush daemon request")?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("failed to read daemon response")?;
    if line.is_empty() {
        bail!("daemon closed connection without a response");
    }

    let response = serde_json::from_str(&line).context("failed to parse daemon response")?;
    Ok(Some(response))
}

fn shutdown_daemon_direct() -> Result<()> {
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

fn run_daemon() -> Result<()> {
    let root = bridge_root()?;
    fs::create_dir_all(&root).with_context(|| format!("failed to create {}", root.display()))?;

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

fn doctor(cmd: &str, cwd: Option<PathBuf>) -> Result<()> {
    let cwd = cwd
        .unwrap_or(std::env::current_dir().context("failed to read current directory")?)
        .canonicalize()
        .context("failed to canonicalize cwd")?;
    if !cwd.is_dir() {
        bail!("cwd is not a directory: {}", cwd.display());
    }

    let command_parts = parse_command(cmd)?;
    let child_path =
        path_with_local_bin().unwrap_or_else(|| std::env::var("PATH").unwrap_or_default());
    let resolved_program = resolve_program_path(&command_parts[0], &child_path)
        .unwrap_or_else(|| command_parts[0].clone());
    let executable_found = is_executable(Path::new(&resolved_program));

    println!("agent-bridge doctor");
    println!("sessions_root: {}", sessions_root()?.display());
    println!("cwd: {}", cwd.display());
    println!("input_command: {cmd}");
    println!("parsed_command: {}", command_parts.join(" "));
    println!("resolved_program: {resolved_program}");
    println!("executable_found: {executable_found}");
    println!("child_path:");
    for (index, entry) in child_path
        .split(':')
        .filter(|entry| !entry.is_empty())
        .enumerate()
    {
        println!("  {:>2}: {entry}", index + 1);
    }

    let mut warnings = Vec::new();
    if resolved_program.contains("/opt/homebrew/bin/claude") {
        warnings.push(
            "/opt/homebrew/bin/claude matched; this older Claude binary previously emitted zero PTY bytes in project directories."
                .to_string(),
        );
    }
    if command_is_claude(&command_parts[0]) && !resolved_program.contains("/.local/bin/claude") {
        warnings.push("known-good local Claude path was ~/.local/bin/claude in this environment; resolved path differs.".to_string());
    }
    if !executable_found {
        warnings.push("resolved program is not executable or could not be found.".to_string());
    }

    if command_is_claude(&command_parts[0]) || command_is_claude(&resolved_program) {
        println!();
        println!("claude_version_via_agent_bridge_path:");
        print_command_output(
            Command::new(&resolved_program)
                .arg("--version")
                .current_dir(&cwd)
                .env("PATH", &child_path)
                .output(),
        );

        println!();
        println!("claude_version_via_login_shell:");
        print_command_output(
            Command::new("zsh")
                .args(["-lic", "command -v claude; claude --version"])
                .current_dir(&cwd)
                .output(),
        );
    }

    println!();
    if warnings.is_empty() {
        println!("warnings: none");
    } else {
        println!("warnings:");
        for warning in warnings {
            println!("  - {warning}");
        }
    }

    Ok(())
}

fn start_session(name: &str, cwd: Option<PathBuf>, cmd: &str) -> Result<()> {
    let metadata = start_session_detached(name, cwd, cmd)?;
    print!("{}", format_started_session(&metadata));
    Ok(())
}

fn start_session_detached(name: &str, cwd: Option<PathBuf>, cmd: &str) -> Result<SessionMetadata> {
    validate_session_name(name)?;

    let cwd = cwd
        .unwrap_or(std::env::current_dir().context("failed to read current directory")?)
        .canonicalize()
        .context("failed to canonicalize cwd")?;
    if !cwd.is_dir() {
        bail!("cwd is not a directory: {}", cwd.display());
    }

    let session_dir = session_dir(name)?;
    fs::create_dir_all(&session_dir)
        .with_context(|| format!("failed to create {}", session_dir.display()))?;

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
    supervisor
        .arg("supervisor")
        .arg(name)
        .arg("--cwd")
        .arg(&cwd)
        .arg("--cmd")
        .arg(cmd)
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

fn start_session_in_daemon(name: &str, cwd: Option<PathBuf>, cmd: &str) -> Result<SessionMetadata> {
    validate_session_name(name)?;

    let cwd = cwd
        .unwrap_or(std::env::current_dir().context("failed to read current directory")?)
        .canonicalize()
        .context("failed to canonicalize cwd")?;
    if !cwd.is_dir() {
        bail!("cwd is not a directory: {}", cwd.display());
    }

    let session_dir = session_dir(name)?;
    fs::create_dir_all(&session_dir)
        .with_context(|| format!("failed to create {}", session_dir.display()))?;

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

fn format_started_session(metadata: &SessionMetadata) -> String {
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

fn initialize_session_files(name: &str, cwd: &Path, cmd: &str) -> Result<()> {
    let dir = session_dir(name)?;
    let input = dir.join(INPUT_FIFO);

    if input.exists() {
        fs::remove_file(&input).with_context(|| format!("failed to remove {}", input.display()))?;
    }
    mkfifo(&input, Mode::from_bits_truncate(0o600))
        .with_context(|| format!("failed to create {}", input.display()))?;

    File::create(dir.join(RAW_LOG)).context("failed to create raw log")?;
    File::create(dir.join(CLEAN_LOG)).context("failed to create clean log")?;
    File::create(dir.join(SCREEN_SNAPSHOT)).context("failed to create screen snapshot")?;

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

fn wait_for_running_metadata(name: &str) -> Result<SessionMetadata> {
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

fn run_supervisor(name: &str, cwd: &Path, cmd: &str) -> Result<()> {
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

fn supervise_pty(name: &str, cwd: &Path, cmd: &str) -> Result<String> {
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

    let input_fifo = session_dir.join(INPUT_FIFO);
    let _input_thread = thread::spawn(move || forward_input(&input_fifo, &mut input_writer));

    let exit_status = child.wait().context("failed while waiting for child")?;

    drop(pair.master);
    let _ = output_thread.join();

    Ok(format!("{exit_status:?}"))
}

fn capture_output(reader: &mut Box<dyn Read + Send>, session_dir: &Path) -> Result<()> {
    let mut raw_log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(session_dir.join(RAW_LOG))
        .context("failed to open raw log")?;
    let mut clean_log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(session_dir.join(CLEAN_LOG))
        .context("failed to open clean log")?;
    let mut terminal = vt100::Parser::new(SCREEN_ROWS, SCREEN_COLS, 2_000);
    let mut buffer = [0_u8; 8192];

    loop {
        let read = reader.read(&mut buffer).context("failed to read PTY")?;
        if read == 0 {
            return Ok(());
        }

        let chunk = &buffer[..read];
        raw_log
            .write_all(chunk)
            .context("failed to write raw log")?;
        raw_log.flush().context("failed to flush raw log")?;

        let clean = strip_ansi_escapes::strip(chunk);
        clean_log
            .write_all(&clean)
            .context("failed to write clean log")?;
        clean_log.flush().context("failed to flush clean log")?;

        terminal.process(chunk);
        fs::write(
            session_dir.join(SCREEN_SNAPSHOT),
            trim_screen_snapshot(&terminal.screen().contents()),
        )
        .context("failed to write screen snapshot")?;
    }
}

fn forward_input(input_fifo: &Path, writer: &mut Box<dyn Write + Send>) -> Result<()> {
    let mut fifo = OpenOptions::new()
        .read(true)
        .write(true)
        .open(input_fifo)
        .with_context(|| format!("failed to open {}", input_fifo.display()))?;
    let mut buffer = [0_u8; 8192];

    loop {
        let read = fifo
            .read(&mut buffer)
            .context("failed to read input FIFO")?;
        if read == 0 {
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        writer
            .write_all(&buffer[..read])
            .context("failed to write input to PTY")?;
        writer.flush().context("failed to flush PTY input")?;
    }
}

fn send_input(name: &str, text: &str, no_enter: bool) -> Result<()> {
    send_input_silent(name, text, no_enter)?;
    println!("sent input to '{name}'");

    Ok(())
}

fn send_input_silent(name: &str, text: &str, no_enter: bool) -> Result<()> {
    let mut bytes = text.as_bytes().to_vec();
    if !no_enter {
        bytes.push(b'\r');
    }

    write_session_bytes(name, &bytes)
}

fn send_keys(name: &str, keys: &[String]) -> Result<()> {
    send_keys_silent(name, keys)?;
    println!("sent keys to '{name}': {}", keys.join(" "));

    Ok(())
}

fn send_keys_silent(name: &str, keys: &[String]) -> Result<()> {
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

fn write_session_bytes(name: &str, bytes: &[u8]) -> Result<()> {
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

fn encode_key(key: &str) -> Result<&'static [u8]> {
    match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => Ok(b"\r"),
        "escape" | "esc" => Ok(b"\x1b"),
        "ctrl-c" | "control-c" | "c-c" => Ok(b"\x03"),
        "ctrl-d" | "control-d" | "c-d" => Ok(b"\x04"),
        "ctrl-z" | "control-z" | "c-z" => Ok(b"\x1a"),
        "tab" => Ok(b"\t"),
        "backspace" | "bs" => Ok(b"\x7f"),
        "delete" | "del" => Ok(b"\x1b[3~"),
        "up" | "arrow-up" => Ok(b"\x1b[A"),
        "down" | "arrow-down" => Ok(b"\x1b[B"),
        "right" | "arrow-right" => Ok(b"\x1b[C"),
        "left" | "arrow-left" => Ok(b"\x1b[D"),
        "home" => Ok(b"\x1b[H"),
        "end" => Ok(b"\x1b[F"),
        _ => bail!("unsupported key '{key}'"),
    }
}

fn read_output(name: &str, tail: usize, raw: bool) -> Result<()> {
    print!("{}", read_output_text(name, tail, raw)?);
    Ok(())
}

fn read_output_text(name: &str, tail: usize, raw: bool) -> Result<String> {
    validate_session_name(name)?;
    let path = session_dir(name)?.join(if raw { RAW_LOG } else { CLEAN_LOG });
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;

    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(tail);
    let mut output = String::new();
    for line in &lines[start..] {
        output.push_str(line);
        output.push('\n');
    }

    Ok(output)
}

fn read_screen(name: &str, tail: usize) -> Result<()> {
    print!("{}", read_screen_text(name, tail)?);
    Ok(())
}

fn read_screen_text(name: &str, tail: usize) -> Result<String> {
    validate_session_name(name)?;
    let path = session_dir(name)?.join(SCREEN_SNAPSHOT);
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;

    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(tail);
    let mut output = String::new();
    for line in &lines[start..] {
        output.push_str(line);
        output.push('\n');
    }

    Ok(output)
}

fn print_status(name: &str) -> Result<()> {
    print!("{}", status_text(name)?);
    Ok(())
}

fn status_text(name: &str) -> Result<String> {
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

fn list_sessions() -> Result<()> {
    print!("{}", list_sessions_text()?);
    Ok(())
}

fn list_sessions_text() -> Result<String> {
    let sessions_dir = sessions_root()?;
    fs::create_dir_all(&sessions_dir)
        .with_context(|| format!("failed to create {}", sessions_dir.display()))?;

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

fn stop_session(name: &str) -> Result<()> {
    stop_session_silent(name)?;
    println!("stopped session '{name}'");

    Ok(())
}

fn stop_session_silent(name: &str) -> Result<()> {
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

fn terminate_pid(pid: i32) -> Result<()> {
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

fn signal_pid_and_group(pid: i32, signal: Signal) -> Result<bool> {
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

fn trim_screen_snapshot(contents: &str) -> String {
    let mut lines = contents.lines().map(str::trim_end).collect::<Vec<_>>();

    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }

    if lines.is_empty() {
        String::new()
    } else {
        let mut snapshot = lines.join("\n");
        snapshot.push('\n');
        snapshot
    }
}

fn mark_stopped(name: &str, exit_status: Option<String>) -> Result<()> {
    let mut metadata = load_metadata(name)?;
    metadata.status = SessionStatus::Stopped;
    metadata.updated_at_unix = now_unix();
    metadata.exit_status = exit_status;
    save_metadata(&metadata)
}

fn load_metadata(name: &str) -> Result<SessionMetadata> {
    let path = session_dir(name)?.join(METADATA);
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
}

fn save_metadata(metadata: &SessionMetadata) -> Result<()> {
    let path = session_dir(&metadata.name)?.join(METADATA);
    let mut next = metadata.clone();
    next.updated_at_unix = now_unix();
    let contents = serde_json::to_string_pretty(&next).context("failed to serialize metadata")?;
    fs::write(&path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn parse_command(cmd: &str) -> Result<Vec<String>> {
    let parts = shell_words::split(cmd).context("failed to parse command")?;
    if parts.is_empty() {
        bail!("command cannot be empty");
    }
    Ok(parts)
}

fn validate_session_name(name: &str) -> Result<()> {
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

fn sessions_root() -> Result<PathBuf> {
    Ok(bridge_root()?.join("sessions"))
}

fn socket_path() -> Result<PathBuf> {
    Ok(bridge_root()?.join("agent-bridge.sock"))
}

fn bridge_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("AGENT_BRIDGE_HOME") {
        return Ok(PathBuf::from(root));
    }

    let base_dirs = BaseDirs::new().ok_or_else(|| anyhow!("failed to locate home directory"))?;
    Ok(base_dirs.home_dir().join(".agent-bridge"))
}

fn path_with_local_bin() -> Option<String> {
    let base_dirs = BaseDirs::new()?;
    let local_bin = base_dirs.home_dir().join(".local/bin");
    let current_path = std::env::var("PATH").unwrap_or_default();
    let local_bin = local_bin.to_string_lossy();

    if current_path.is_empty() {
        Some(local_bin.to_string())
    } else {
        let rest = current_path
            .split(':')
            .filter(|entry| !entry.is_empty() && *entry != local_bin)
            .collect::<Vec<_>>()
            .join(":");

        if rest.is_empty() {
            Some(local_bin.to_string())
        } else {
            Some(format!("{local_bin}:{rest}"))
        }
    }
}

fn resolve_program_path(program: &str, path: &str) -> Option<String> {
    if program.contains('/') {
        return Some(program.to_string());
    }

    path.split(':')
        .filter(|entry| !entry.is_empty())
        .map(|entry| Path::new(entry).join(program))
        .find(|candidate| is_executable(candidate))
        .map(|candidate| candidate.to_string_lossy().to_string())
}

fn command_is_claude(program: &str) -> bool {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "claude")
}

fn print_command_output(output: std::io::Result<std::process::Output>) {
    match output {
        Ok(output) => {
            println!("  status: {}", output.status);
            print_output_block("stdout", &output.stdout);
            print_output_block("stderr", &output.stderr);
        }
        Err(error) => println!("  failed: {error}"),
    }
}

fn print_output_block(label: &str, bytes: &[u8]) {
    let contents = String::from_utf8_lossy(bytes);
    let contents = contents.trim_end();
    if contents.is_empty() {
        println!("  {label}: <empty>");
    } else {
        println!("  {label}:");
        for line in contents.lines() {
            println!("    {line}");
        }
    }
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn session_dir(name: &str) -> Result<PathBuf> {
    validate_session_name(name)?;
    Ok(sessions_root()?.join(name))
}

fn process_alive(pid: i32) -> bool {
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
