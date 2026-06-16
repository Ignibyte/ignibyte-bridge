//! Local persistent PTY session controller for AI coding agents.
//!
//! Ignibyte Bridge starts real terminal programs (Claude Code, REPLs, dev servers,
//! shells) inside a pseudo-terminal, keeps them running, and lets a manager
//! program drive them with keystrokes and read back their rendered screen — the
//! capability tmux/iTerm give a human, exposed as a CLI.
//!
//! # Modules
//!
//! - [`session`] — session lifecycle (start/stop/status/list), metadata, and the
//!   PTY supervisor that owns a child process.
//! - [`daemon`] — the Unix-socket daemon that owns sessions in long-lived threads
//!   and the request dispatch over them.
//! - [`protocol`] — the line-delimited JSON wire types and socket client shared
//!   by the CLI and the daemon.
//! - [`logs`] — PTY output capture, input forwarding, and bounded log tailing.
//! - [`clean`] — the streaming ANSI/terminal-control stripper for the clean log.
//! - [`paths`] — storage roots, name validation, atomic/private file helpers, and
//!   command/`PATH` resolution.
//! - [`procinfo`] — process start-time tokens that make liveness checks robust to
//!   PID reuse.
//! - [`keys`] — terminal key-name to byte-sequence encoding.
//! - [`doctor`] — environment and command-resolution diagnostics.
//!
//! # Execution modes
//!
//! A session is owned either by a long-lived [`daemon`] (when its socket is
//! reachable) or, in direct mode, by a detached `setsid` supervisor process.
//! Both write the same on-disk state under `IGNIBYTE_BRIDGE_HOME`, so read-side
//! commands work regardless of which started the session.

pub mod clean;
pub mod daemon;
pub mod doctor;
pub mod keys;
pub mod logs;
pub mod paths;
pub mod procinfo;
pub mod protocol;
pub mod session;

pub(crate) const INPUT_FIFO: &str = "input.fifo";
pub(crate) const RAW_LOG: &str = "raw.log";
pub(crate) const CLEAN_LOG: &str = "clean.log";
pub(crate) const SCREEN_SNAPSHOT: &str = "screen.txt";
pub(crate) const METADATA: &str = "metadata.json";
/// Default PTY geometry when `start` is given no `--rows`/`--cols`. Roomier than
/// a minimal terminal so TUIs such as Claude Code render with usable width.
pub(crate) const SCREEN_ROWS: u16 = 40;
pub(crate) const SCREEN_COLS: u16 = 140;
