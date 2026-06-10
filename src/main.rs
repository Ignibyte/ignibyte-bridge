//! Local persistent PTY session controller for AI coding agents.

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use agent_bridge::{
    daemon::{run_daemon, shutdown_daemon_direct},
    doctor::doctor,
    protocol::{try_send_daemon_request, DaemonRequest},
    session::{
        list_sessions, print_status, read_output, read_screen, run_supervisor, send_input,
        send_keys, start_session, stop_session,
    },
};

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
