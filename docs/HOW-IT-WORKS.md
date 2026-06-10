# How Agent Bridge Works

This document explains the internals: the process model, how a session runs end
to end, the input/output paths, and the concurrency model that keeps it correct
when short-lived CLI commands, a long-lived daemon, and detached supervisors all
touch the same on-disk state.

The code is a thin `src/main.rs` (CLI parsing + dispatch) over a library split
into focused modules:

| Module | Responsibility |
| --- | --- |
| `session` | Session lifecycle, the PTY supervisor, metadata, liveness, signalling |
| `daemon` | The Unix-socket daemon and its request dispatch |
| `protocol` | Wire types (`DaemonRequest`/`DaemonResponse`) and the socket client |
| `logs` | PTY output capture, input forwarding, bounded log tailing |
| `clean` | The streaming ANSI/terminal-control stripper for the clean log |
| `paths` | Storage roots, name validation, atomic/private file helpers, command resolution |
| `procinfo` | Process start-time tokens (PID-reuse-safe liveness) |
| `keys` | Key-name → byte-sequence encoding |
| `doctor` | Environment and command-resolution diagnostics |

## Process model

A session is a child process (Claude, a REPL, a shell …) running inside a
pseudo-terminal (PTY). Something must *own* that PTY for the session's lifetime —
hold the master fd, feed it input, and drain its output. Agent Bridge has two
owners depending on mode:

```
                 CLI command (short-lived: start/send/read/screen/status/stop)
                        │
            socket reachable? ──── no ──► DIRECT MODE
                        │ yes                  │
                        ▼                      ▼
                 DAEMON MODE          detached supervisor process
            ┌───────────────────┐     (setsid; survives the CLI
            │  agent-bridge     │      command that spawned it)
            │  daemon           │             │
            │  • owns sessions  │             │
            │    in threads     │             ▼
            └─────────┬─────────┘     owns one PTY child
                      ▼
              owns PTY children
```

- **Daemon mode** — `agent-bridge daemon` runs a Unix-socket server. Each session
  is owned by a thread inside the daemon process (so `supervisor_pid` is absent
  for daemon-owned sessions; the `child_pid` is the tracked process). CLI
  commands auto-route through the socket when it is reachable.
- **Direct mode** — with no daemon (or `--direct`), `start` spawns a copy of the
  binary as a hidden `supervisor` subcommand, detached with `setsid` so it
  outlives the short-lived CLI process. That supervisor owns the PTY.

Both write the same files under `AGENT_BRIDGE_HOME`, so read-side commands
(`read`/`screen`/`status`/`list`) work regardless of which started the session —
they just read files. Only `start`/`send`/`keys`/`stop`/`shutdown` need to reach
the owner (via the socket, or via the FIFO and signals).

The library function that owns a PTY for both modes is `session::supervise_pty`.

## Anatomy of a `start`

1. **Resolve & validate** — the client canonicalizes `--cwd` to an absolute path
   (so a daemon session uses *your* directory, not the daemon's) and validates
   the session name (rejecting `.`, `..`, leading `-`, and traversal).
2. **Private storage** — `paths::ensure_bridge_dir` creates the bridge root and
   the per-session directory `0700`, validating every path component is a real,
   owner-owned directory (no symlink swaps).
3. **Start lock** — `acquire_start_lock` takes a non-blocking `flock` on
   `<session>/start.lock`, held through step 7, so two concurrent starts of the
   same name cannot both proceed.
4. **Liveness check** — if a prior run is still active (`session_is_active`:
   status `Running` *and* a live, identity-matched PID), the start is refused.
5. **Initialize files** — `initialize_session_files` creates the FIFO and the
   `raw.log`/`clean.log`/`screen.txt` files (`0600`), rotates any prior non-empty
   logs to `*.prev`, and writes `metadata.json` with status `Starting`, the
   recorded geometry, and a freshly **incremented generation** (see below).
6. **Spawn the owner** — direct mode forks a `setsid` supervisor process; daemon
   mode spawns a thread. Both call `supervise_pty`.
7. **Wait for Running** — `wait_for_running_metadata` polls the metadata until the
   owner publishes `Running` (or reports a clean fast exit as success, or
   converges to a clean `Stopped` on timeout).

Inside `supervise_pty`:

- Open the PTY at the recorded geometry and spawn the command (`portable-pty`).
- **Record the child PID early** (still `Starting`), paired with its start-time
  token, under the status lock — so a racing `stop`/`shutdown` can identify and
  signal the child instead of orphaning it.
- Open the FIFO reader, **then** promote to `Running` under the status lock,
  re-checking that no `stop`/restart intervened. Opening the reader before
  publishing `Running` closes the window where a `send` could hit the FIFO with
  no reader (ENXIO).
- Spawn the capture and input-forwarder threads, then `child.wait()`.
- On exit: stop and join the forwarder, drain output (bounded), and join the
  capture thread — nothing is leaked.

## The input path

```
send "text"  ──►  input.fifo  ──►  forward_input thread  ──►  PTY master  ──►  child stdin
   (writer)        (per session)     (in the owner)
```

- `write_session_bytes` opens the FIFO `O_WRONLY|O_NONBLOCK` (so a missing reader
  fails fast with a clear "no input reader" error instead of blocking), then
  clears `O_NONBLOCK` so the write itself blocks until fully delivered — large
  pastes are never truncated mid-write.
- The forwarder holds the FIFO open `O_RDWR` (so it never sees EOF when a sender
  disconnects) with `O_NONBLOCK`, polling a stop flag so it can be joined when
  the child exits.
- **`send` auto-submit** — `send` writes the text, waits ~60ms, then writes the
  carriage return as a *separate* write. A CR arriving in the same PTY read as
  the text is treated by editors like Claude Code as a newline within pasted
  input; a lone CR is a submit keystroke. `--no-enter` sends only the text.
- `keys` encodes named keys (`enter`, `escape`, `ctrl-c`, arrows, …) to their
  byte sequences and writes them the same way.

## The output path

```
child stdout ──► PTY master ──► capture_output thread ──► raw.log    (exact bytes, append)
                                                       ├─► clean.log  (AnsiCleaner, append)
                                                       └─► screen.txt (vt100 render, atomic rewrite)
```

The capture thread owns a `dup` of the master fd and uses `poll` with a short
timeout so it can observe a stop flag (and thus be joined) even when a
backgrounded grandchild holds the PTY slave open and no EOF ever arrives.

- **`raw.log`** — every PTY byte, appended verbatim. The source of truth for
  replay; `read --raw` tails it with lossy UTF-8 so binary output never breaks it.
- **`clean.log`** — `clean::AnsiCleaner`, a stateful escape-sequence stripper that
  carries parser state across read chunks (so a sequence split over two reads is
  still removed), preserves tabs, normalizes CR→LF, and treats ESC as
  cancel/restart. This is what `read` shows by default.
- **`screen.txt`** — the current rendered viewport from a `vt100` parser, written
  atomically (temp file + rename) so a concurrent `screen` never sees a torn
  snapshot. This is what `screen` shows — use it for full-screen TUIs.

A write failure on any sink (e.g. a full disk) is logged once and the loop keeps
draining the PTY, so the child can never block on an undrained master and freeze
the session.

## Concurrency model

All session state is on the filesystem and is touched by up to three kinds of
unsynchronized actors (CLI processes, the daemon's per-connection threads,
detached supervisors). Correctness rests on a few primitives:

- **Atomic writes** — `metadata.json` and `screen.txt` are written via
  `paths::write_atomic` (write a private temp file in the same dir, then
  `rename` over the target). `rename(2)` is atomic, so a reader always sees a
  complete old or new file, and a writer killed mid-write leaves at most a stray
  temp file.
- **Locks** (advisory `flock`, separate files so they never deadlock each other):
  - `daemon.lock` — held for the daemon's lifetime; a second daemon refuses.
  - `start.lock` — held (non-blocking) by a `start` from before file
    initialization through "Running", serializing concurrent starts of one name.
  - `status.lock` — held (blocking) around every **status transition**
    (promote-to-Running in the supervisor; mark-Stopped from the supervisor,
    `stop`, or a start timeout). The start handler deliberately does *not* take
    this lock, so the supervisor can promote while a start is in flight.
- **Generation** — `initialize_session_files` stamps each run with an
  incrementing `generation`. Every terminal write is generation-guarded: a stale
  supervisor from an earlier run cannot mark a freshly-restarted run `Stopped`.
  The promote-to-Running re-checks status under the lock, so a `stop` that lands
  during startup is honored (the child is reaped) rather than resurrected.

## Liveness and PID reuse

A recorded PID is not a stable identity — after an unclean shutdown the kernel
can recycle it. `procinfo::process_start_time` returns a per-process start-time
token (via `proc_pidinfo` on macOS, `/proc/<pid>/stat` on Linux). `pid_is_ours`
treats a PID as the session's only if it is both alive *and* its start-time
matches the recorded token. So:

- `start` is not falsely blocked by stale `Running` metadata pointing at a reused
  PID; `status`/`list` do not report an impostor as alive.
- `stop` only signals a PID it can positively identify as ours — it never
  SIGTERM/SIGKILLs a stranger's process (or its process group).

When the recorded token can't be read at publish time (the child already exited),
the PID is dropped rather than recorded unguarded.

## Activity / idle signal

`status` reports `last_output_unix`, `idle_seconds`, and `output_bytes`; `list`
shows an `idle=` column. These are derived from `raw.log`'s mtime and size — no
extra writes, no contention. Because the capture thread appends to `raw.log` on
every output chunk, the log's mtime *is* the last-output time: an `idle_seconds`
that stops climbing means the program has gone quiet (e.g. Claude finished
responding and is waiting for input).

## Stopping and cleanup

`stop` (and daemon `shutdown`, which acks first then terminates) sends SIGTERM,
waits, then SIGKILL — to both the PID and its process group (`kill(-pid)`), so a
child's own children are cleaned up too. Daemon `shutdown` removes the socket and
releases the daemon lock on exit. In direct mode, when the supervisor dies it
closes the PTY master, and the kernel delivers SIGHUP to the session leader,
reaping a child that ignored the group signal.

## Files on disk

```
$AGENT_BRIDGE_HOME/
  agent-bridge.sock        # daemon control socket (0600)
  daemon.lock              # daemon singleton lock
  sessions/<name>/
    metadata.json          # status, pids+tokens, generation, geometry, exit (0600, atomic)
    raw.log                # exact PTY bytes (0600, append)
    clean.log              # ANSI-stripped text (0600, append)
    screen.txt             # current rendered screen (0600, atomic)
    input.fifo             # input channel (0600)
    start.lock             # per-session start lock
    status.lock            # per-session status-transition lock
    raw.log.prev           # previous run's logs, kept on restart
```

See [`AGENT-BRIDGE.md`](AGENT-BRIDGE.md) for the design rationale and roadmap, and
the [`CODE-REVIEW-2026-06-10.md`](CODE-REVIEW-2026-06-10.md) /
[`-fixes.md`](CODE-REVIEW-2026-06-10-fixes.md) reports for the audit trail behind
these invariants.
