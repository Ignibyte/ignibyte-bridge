# Agent Bridge

Local persistent PTY session controller for AI coding agents.

Agent Bridge lets a manager program — another AI agent, a script, or you — start
real terminal programs (Claude Code, Codex CLI, REPLs, dev servers, shells),
keep them running, drive them with keystrokes, and read back what's on their
screen. It gives an agent the same thing tmux or iTerm give a human: a live
terminal it can watch and type into, exposed through a small CLI.

> Status: working prototype. The full design and roadmap live in
> [`docs/AGENT-BRIDGE.md`](docs/AGENT-BRIDGE.md). A complete adversarial code
> review and its resolutions are in
> [`docs/CODE-REVIEW-2026-06-10.md`](docs/CODE-REVIEW-2026-06-10.md).

## Why not just capture stdout?

Terminal programs aren't append-only text streams. They draw into an alternate
screen, move the cursor, repaint status lines, and expect real keys (Escape,
Ctrl-C, arrows, Tab). So Agent Bridge keeps three views of every session:

- **`raw.log`** — exact PTY bytes, for replay and debugging.
- **`clean.log`** — ANSI-stripped readable text, for line-oriented `read`.
- **`screen.txt`** — the current rendered screen, like tmux `capture-pane`,
  produced by a `vt100` terminal parser.

## Architecture

```text
manager agent / you
        │  CLI  (agent-bridge start | send | keys | read | screen | ...)
        ▼
agent-bridge client ──► running daemon?
        │ yes                         │ no
        ▼                             ▼
Unix-socket daemon            detached supervisor process (setsid)
  owns sessions in threads      owns one session
        │                             │
        └──────────┬──────────────────┘
                   ▼
        PTY child (claude / python3 -i / sh / …)
        + raw.log, clean.log, screen.txt, metadata.json under
          ~/.agent-bridge/sessions/<name>/
```

There are two execution modes:

- **Daemon mode** — run `agent-bridge daemon` once; it owns sessions and
  outlives short-lived client commands. Client commands automatically route
  through its Unix socket when it's reachable. This is the durable setup.
- **Direct mode** — with no daemon (or `--direct`), `start` spawns a detached
  `setsid` supervisor that owns the session. Useful for one-offs and debugging.

Both modes share the same on-disk state, so `read`/`screen`/`status` work
regardless of which started the session.

## Install

Requires a Rust toolchain (2021 edition). macOS and Linux.

```bash
cargo build --release
# binary at target/release/agent-bridge
```

The examples below use `agent-bridge` for the built binary.

## Quickstart

Daemon mode, end to end (note the single `export` so the daemon and every
client share one home and socket):

```bash
export AGENT_BRIDGE_HOME=/tmp/agent-bridge-demo

# Start the daemon in its own terminal (it runs in the foreground):
agent-bridge daemon

# In another terminal (same AGENT_BRIDGE_HOME):
export AGENT_BRIDGE_HOME=/tmp/agent-bridge-demo
agent-bridge start py --cmd "python3 -i"
agent-bridge send  py "print(2 + 3)"
agent-bridge read  py --tail 20      # -> 5
agent-bridge screen py --tail 40     # current rendered screen
agent-bridge stop  py
agent-bridge shutdown                # stops sessions and exits the daemon
```

Direct mode (no daemon needed):

```bash
agent-bridge --direct start sh --cmd sh
agent-bridge --direct send  sh "echo hello"
agent-bridge --direct read  sh --tail 10
agent-bridge --direct stop  sh
```

## Commands

| Command | Purpose |
| --- | --- |
| `start <name> --cmd "<cmd>" [--cwd <dir>]` | Start a named PTY session. `--cwd` defaults to the client's current directory. |
| `send <name> <text> [--no-enter]` | Send text, with a trailing Enter unless `--no-enter`. |
| `keys <name> <key>...` | Send control keys: `enter escape ctrl-c ctrl-d ctrl-z tab backspace delete up down left right home end`. |
| `read <name> [--tail N] [--raw]` | Print the last `N` lines of `clean.log` (or `raw.log` with `--raw`). |
| `screen <name> [--tail N]` | Print the current rendered terminal screen. |
| `status <name>` | Show one session's status, pids, and liveness. |
| `list` | List all known sessions. |
| `stop <name>` | Stop a session (SIGTERM then SIGKILL to its process group). |
| `daemon` | Run the local daemon (foreground). |
| `shutdown` | Ask the daemon to stop all sessions and exit. |
| `doctor [--cmd <cmd>] [--cwd <dir>]` | Diagnose command resolution, PATH, and (for `claude`) version. |

Global flag: `--direct` bypasses the daemon and runs the command in-process.

To send text that begins with a dash, it's forwarded as-is
(`agent-bridge send py "-1 + 2"`); to send a literal flag like `--help`, use the
`--` separator: `agent-bridge send py -- --help`.

## Storage layout

All state lives under `AGENT_BRIDGE_HOME` (default `~/.agent-bridge`):

```text
~/.agent-bridge/
  agent-bridge.sock        # daemon control socket (0600)
  daemon.lock              # daemon singleton lock
  sessions/
    <name>/
      metadata.json        # status, pids, command, cwd (0600)
      raw.log              # exact PTY bytes (0600)
      clean.log            # ANSI-stripped text (0600)
      screen.txt           # current rendered screen (0600)
      input.fifo           # input channel (0600)
      start.lock           # per-session start lock
      raw.log.prev         # previous run's logs, kept on restart
```

## Environment

- **`AGENT_BRIDGE_HOME`** — storage root. Must be an **absolute** path. Use a
  per-user location; do not point it at a shared directory like `/tmp/shared`
  (see Security). For an isolated scratch instance, a private temp dir works:
  `AGENT_BRIDGE_HOME="$(mktemp -d)" agent-bridge list`.
- **`PATH`** — used to resolve bare command names. `~/.local/bin` is prepended
  so `claude` resolves the way your login shell resolves it. In daemon mode the
  client forwards its own `PATH` so sessions run with your environment, not the
  daemon's.

## Security

Agent Bridge is local-only and single-user by design:

- The control socket and FIFO are `0600`; session directories are `0700` and all
  log/metadata files are `0600`, so other local users can't read transcripts or
  inject input.
- **Logs capture everything typed and printed in a session**, including any
  secrets (API keys, passwords at prompts). Treat `~/.agent-bridge` as
  sensitive; it is not encrypted.
- `AGENT_BRIDGE_HOME` is validated (absolute path, no symlinked or
  foreign-owned components) and files are created with `O_NOFOLLOW`. Even so,
  keep it under a directory only you can write.
- The daemon protocol is unauthenticated beyond the socket's `0600` permission:
  any process running as you can drive it.

## Limitations

This is a prototype. Not yet implemented (see the roadmap in the design doc):
terminal resize, bracketed paste, idle/approval detection, an MCP adapter, log
rotation/size caps, daemon auto-start, and recovery of daemon-owned sessions
across a daemon restart.

## Troubleshooting

- **A command won't start / "stopped while starting"** — run
  `agent-bridge doctor --cmd "<cmd>" --cwd <dir>` to see how the command
  resolves, the effective `PATH`, and (for `claude`) whether the resolved binary
  matches your login shell.
- **Commands hang against a daemon** — the daemon may be suspended; the client
  times out after 60s with a clear message. Restart the daemon, or use
  `--direct`.
- **Leftover test processes** — detached supervisors run under `setsid`; if a
  crash leaves one, `pkill -f 'agent-bridge supervisor'`.

## Development

```bash
cargo build
cargo clippy --all-targets
cargo test                 # unit + CLI + daemon integration tests
cargo llvm-cov --html      # coverage report (requires cargo-llvm-cov)
```

The crate is a thin `main.rs` over a library split into focused modules
(`session`, `daemon`, `logs`, `clean`, `paths`, `protocol`, `keys`, `procinfo`,
`doctor`); run `cargo doc --open` for the API docs.
