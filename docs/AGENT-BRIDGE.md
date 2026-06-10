# Agent Bridge

Agent Bridge is a local terminal-session controller for AI coding agents.

The long-term goal is not to replace iTerm or tmux. It is to provide the same
core capability they expose to agents today: a persistent real terminal session
that can run Claude Code, Codex CLI, Gemini CLI, Aider, OpenCode, dev servers,
test watchers, shells, and REPLs while another agent can programmatically send
input and read the terminal state.

## Product Thesis

Codex or Rusty should be able to act as a manager over persistent terminal
workers.

```text
Codex / Rusty / another manager agent
        |
        | CLI or MCP tools
        v
agent-bridge client
        |
        | local Unix socket
        v
agent-bridge daemon
        |
        | owns PTYs, terminal screen buffers, logs, metadata
        v
Claude Code / Codex CLI / dev server / REPL
```

The important distinction is that Claude Code is a persistent terminal
application. It is not a one-shot command. The bridge must behave like a real
terminal host: create a PTY, give the child process a controlling terminal,
track the rendered terminal screen, send keystrokes, and keep the process alive
until explicitly stopped.

## Current Prototype

The prototype lives in `agent-bridge/`.

Implemented:

- `daemon` listens on a local Unix socket and can own PTY sessions.
- `start` launches a named command in a real PTY.
- In direct mode, a detached supervisor process owns the PTY.
- When a daemon is reachable, normal CLI session commands route through the
  daemon socket.
- `--direct` bypasses daemon routing for debugging.
- `send` writes later input through a per-session FIFO.
- `keys` sends terminal control keys through the PTY.
- `read` returns recent captured log output.
- `screen` returns the current rendered terminal snapshot.
- `status`, `list`, and `stop` manage session metadata and processes.
- `shutdown` asks the daemon to stop running sessions and exit.
- `doctor` reports storage paths, command resolution, PATH ordering, and Claude
  version comparisons.
- Session files are stored under:

```text
~/.agent-bridge/sessions/{session-name}/raw.log
~/.agent-bridge/sessions/{session-name}/clean.log
~/.agent-bridge/sessions/{session-name}/screen.txt
~/.agent-bridge/sessions/{session-name}/metadata.json
~/.agent-bridge/sessions/{session-name}/input.fifo
```

`AGENT_BRIDGE_HOME` can override the storage root for tests:

```bash
AGENT_BRIDGE_HOME=/private/tmp/agent-bridge-test ./target/debug/agent-bridge list
```

Verified so far:

- `python3 -i` works as an interactive PTY session.
- `python3 -i` also works through `agent-bridge daemon` with start/send/screen,
  status/list, and stop.
- `shutdown` stops a daemon-owned Python session before exiting the daemon.
- `claude --version` works under the PTY and logs output.
- Interactive Claude works through `agent-bridge daemon`; `/help` can be sent
  and read back through `screen`.
- Interactive Claude works when Agent Bridge resolves the command to the same
  Claude binary used by the user's login shell.
- Claude `/help` works through `send`, and `keys escape` dismisses the help
  dialog.
- In the current macOS environment, `/Users/chadpeppers/.local/bin/claude`
  reported Claude Code `2.1.170` during the latest daemon smoke test and works
  through Agent Bridge.
- `/opt/homebrew/bin/claude` reports Claude Code `2.1.81` and can start but
  may emit zero PTY bytes in project directories.
- `doctor --cmd claude --cwd /Users/chadpeppers/Projects/rusty` confirms that
  Agent Bridge and the login shell currently resolve to the same working Claude
  binary.

That Claude result gives two design signals: command resolution must match the
user's normal terminal environment, and raw logs alone are not enough. The
prototype now captures a rendered terminal snapshot with `vt100`; the next
architecture step is making one durable daemon own all sessions.

## Why Logs Are Not Enough

Terminal apps do not only print append-only lines. They may:

- draw into an alternate screen,
- move the cursor,
- overwrite status lines,
- hide or show prompts,
- use bracketed paste,
- depend on terminal size,
- require non-text keys like Escape, Ctrl-C, arrows, Tab, and function keys.

iTerm and tmux work for agent control because they maintain a rendered terminal
buffer. Agents are reading the current screen, not only a byte stream.

Agent Bridge needs both:

- `raw.log`: exact PTY bytes for replay/debugging.
- `clean.log`: readable append-only text when a process behaves linearly.
- `screen`: current rendered terminal viewport, produced by a terminal parser.

## Target Architecture

### Daemon

`agent-bridge daemon` should be the durable owner of all sessions.

Responsibilities:

- listen on a local Unix socket,
- create and own PTYs,
- maintain session metadata,
- capture raw bytes,
- feed bytes into a terminal parser,
- expose current screen snapshots,
- write logs,
- send input and special keys,
- stop processes and process groups,
- recover metadata on restart where possible.

The daemon should be independent of any MCP client. MCP clients may start and
stop frequently; terminal sessions should not.

### Client CLI

The normal commands should become thin clients over the daemon:

```bash
agent-bridge start claude --cwd /path/to/repo --cmd claude
agent-bridge send claude "Review the failing tests."
agent-bridge keys claude enter
agent-bridge screen claude --tail 80
agent-bridge read claude --tail 300
agent-bridge status claude
agent-bridge list
agent-bridge stop claude
agent-bridge shutdown
```

If the daemon is not running, the client can either fail with a clear message or
start it automatically. For early development, explicit `agent-bridge daemon`
is simpler.

### Local Protocol

Prefer a Unix domain socket first, not HTTP.

Reasons:

- local-only by default,
- no port conflicts,
- easier permission model,
- natural fit for a desktop/service process,
- works well as a backend for both CLI and MCP tools.

HTTP can be added later as another adapter if a browser UI or remote control
becomes useful.

### MCP Adapter

MCP is a good interface for Codex/Rusty to control sessions, but MCP should not
own session state.

`agent-bridge mcp` should be a thin MCP server that forwards tool calls to the
daemon over the Unix socket.

Candidate MCP tools:

| Tool | Purpose |
| ---- | ------- |
| `session_start` | Start a named PTY session. |
| `session_send` | Send text, optionally pressing Enter. |
| `session_keys` | Send terminal keys such as Escape, Ctrl-C, Tab, arrows. |
| `session_screen` | Return the current rendered terminal screen. |
| `session_read_log` | Return recent raw or cleaned log output. |
| `session_status` | Return process/session status. |
| `session_list` | List known sessions. |
| `session_stop` | Stop a session. |

This gives Codex a stable tool surface without tying terminal process lifetime
to the lifetime of the MCP server process.

## Claude Code Compatibility Requirements

Claude Code is the primary target.

Required behavior:

- spawn `claude` inside a real PTY,
- ensure it has a controlling terminal,
- preserve normal user environment and auth state,
- set a useful terminal type such as `xterm-256color`,
- set and update PTY size,
- capture alternate-screen rendering,
- send regular text and control keys,
- support slash commands like `/help`,
- support prompt cancellation with Escape or Ctrl-C,
- detect when Claude is waiting for input,
- detect common approval prompts,
- cleanly terminate Claude and child MCP processes.

Useful startup environment details from the existing Rusty Claude runner:

- remove `CLAUDECODE`,
- remove `CLAUDE_CODE_ENTRY_POINT`,
- include `~/.local/bin` in `PATH`.

The current prototype unsets those variables and maintains a `vt100` rendered
screen. Remaining compatibility work is around terminal resize, bracketed paste,
idle/busy detection, approval detection, and daemon-owned session lifetime.

Agent Bridge should also resolve bare command names using the PATH it gives to
the child process. On macOS, `~/.local/bin` must be placed before Homebrew paths
so `claude` resolves to the same binary that iTerm/login shells use.

## Implementation Phases

### Phase 0: Prototype Baseline

Status: mostly done.

Scope:

- standalone Rust CLI project,
- named PTY sessions,
- raw and clean logs,
- metadata,
- start/send/read/status/list/stop.

Acceptance tests:

- `python3 -i` accepts input and returns output.
- `claude --version` prints the version under the PTY.
- stop reports failures honestly when the process cannot be signaled.

### Phase 1: Terminal Screen Model

Goal: make `screen` work like tmux `capture-pane`.

Scope:

- add a terminal parser such as `vt100` or `termwiz`,
- feed every PTY byte into the parser,
- store the current viewport and scrollback-like text,
- add `screen` command,
- keep `read` as log-tail behavior,
- normalize line wrapping and trailing blanks for agent readability.

Acceptance tests:

- shell prompt appears in `screen`.
- Python REPL prompt and result appear in `screen`.
- an alternate-screen test app is readable through `screen`.
- interactive `claude` initial UI appears in `screen`.
- `/help` sent to Claude changes the captured screen.
- Claude rich redraws remain readable without duplicated or overlaid text.

Current status: implemented with `vt100::Parser`.

### Phase 2: Key and Input Semantics

Goal: send input as terminal keystrokes, not only text lines.

Scope:

- add `keys` command,
- support Enter, Escape, Ctrl-C, Ctrl-D, Tab, arrows, Backspace,
- support bracketed paste for multi-line prompts,
- allow `send --no-enter`,
- encode key sequences centrally.

Acceptance tests:

- Escape/Ctrl-C can cancel a Claude prompt.
- Tab and arrows work in shell/REPL contexts.
- multi-line text can be pasted without corrupting the terminal.

Current status: `keys` supports Enter, Escape, Ctrl-C, Ctrl-D, Ctrl-Z, Tab,
Backspace, Delete, arrows, Home, and End.

### Phase 3: Daemon and Unix Socket

Goal: replace per-session hidden supervisors with one durable service.

Status: started. The daemon accepts line-delimited JSON requests over a local
Unix socket and can own PTY sessions directly. CLI commands opportunistically
route through the daemon when the socket is reachable, and fall back to direct
execution when it is not.

Scope:

- `agent-bridge daemon`,
- Unix socket command protocol,
- session registry in daemon memory,
- client CLI forwards commands to daemon,
- metadata persisted under `~/.agent-bridge/sessions`,
- daemon recovers stopped/running metadata on restart.

Acceptance tests:

- daemon can manage multiple sessions concurrently,
- CLI process exits without killing sessions,
- shutdown stops running sessions and exits the daemon,
- daemon restart does not corrupt existing logs,
- list/status remain accurate.

### Phase 4: MCP Server Adapter

Goal: make Codex/Rusty call Agent Bridge through MCP tools.

Scope:

- `agent-bridge mcp`,
- JSON schema for session tools,
- tool calls forward to the daemon,
- screen/log responses are capped and agent-readable,
- errors distinguish missing session, stopped session, and busy session.

Acceptance tests:

- an MCP client can start Claude,
- send a prompt or slash command,
- read screen output,
- stop the session.

### Phase 5: Agent Workflow Features

Goal: make the bridge useful for manager-agent workflows.

Scope:

- idle/busy detection,
- approval prompt detection,
- per-session git status snapshots,
- changed-file summaries,
- optional worktree-per-session creation,
- structured event log,
- session labels and notes,
- replay/debug tooling.

Acceptance tests:

- manager can tell whether Claude is still working or waiting,
- manager can see changed files after a session,
- manager can safely stop or hand off a session.

## Command Surface

Initial service-era CLI:

```bash
agent-bridge daemon
agent-bridge doctor --cmd claude --cwd /path/to/repo
agent-bridge start claude --cwd /path/to/repo --cmd claude
agent-bridge send claude "Review the failing tests." --enter
agent-bridge keys claude ctrl-c
agent-bridge screen claude --tail 120
agent-bridge read claude --tail 300
agent-bridge status claude
agent-bridge list
agent-bridge stop claude
```

Possible future commands:

```bash
agent-bridge attach claude
agent-bridge git claude status
agent-bridge git claude diff
agent-bridge events claude
agent-bridge mcp
```

## Storage Layout

Target layout:

```text
~/.agent-bridge/
  config.toml
  daemon.sock
  sessions/
    claude-main/
      metadata.json
      raw.log
      clean.log
      events.jsonl
      screen.txt
      git-before.txt
      git-after.txt
```

`screen.txt` is a convenience/debug artifact. The authoritative current screen
can live in daemon memory and be served over the socket.

## Open Questions

- Should the daemon auto-start when a client command runs?
- Should sessions survive daemon restart, or is restart allowed to stop PTY
  ownership?
- Which terminal parser gives the best balance of correctness and simplicity:
  `vt100`, `termwiz`, or another parser?
- How much scrollback should be retained in memory?
- Should MCP be bundled into the daemon binary as `agent-bridge mcp`, or run as
  a separate small binary?
- Should the bridge create per-agent git worktrees by default, or only on
  request?

## Immediate Next Step

Implement Phase 3 before adding HTTP or broader workflow features.

Concretely:

1. Add `agent-bridge daemon`.
2. Add a local Unix socket protocol.
3. Convert CLI commands into thin clients that talk to the daemon.
4. Keep the existing supervisor model as a fallback until the daemon path is
   stable.
5. Re-run the Claude interactive smoke test:

```bash
agent-bridge start claude-smoke --cwd /Users/chadpeppers/Projects/rusty --cmd claude
agent-bridge screen claude-smoke --tail 80
agent-bridge send claude-smoke "/help"
agent-bridge screen claude-smoke --tail 120
agent-bridge stop claude-smoke
```

The phase is done when the bridge can read Claude's visible terminal state the
way iTerm/tmux can and keep that session owned by a durable service.
