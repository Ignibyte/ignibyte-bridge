# Agent Bridge — Prototype Handoff

**Date:** 2026-06-12 · **Version:** 0.1.0 · **Repo:** `github.com/chadmandoo/pty_agent` ·
**Status:** working, verified, ready to merge into a host project.

This prototype is complete and in daily-usable shape. This document captures everything the merge
needs: what the code is, the state it was verified in, the invariants the hardening pass
established (do not regress these), how to integrate the crate, and what was deliberately left
unbuilt. The intended reader has never seen this repository.

## What this is

Agent Bridge starts real terminal programs (Claude Code, REPLs, dev servers, shells) inside a
pseudo-terminal, keeps them running, and lets a manager program drive them with keystrokes and
read back their rendered screen — the capability tmux/iTerm give a human, exposed as a CLI.
The long-term aim is a daemon-backed bridge that a manager agent drives over MCP; until the MCP
adapter exists, the manager shells out to the CLI, which is the intended interim mode.

Companion documents (bring all of them in the merge):

- `docs/AGENT-BRIDGE.md` — design document and roadmap.
- `docs/HOW-IT-WORKS.md` — internals guide plus a Claude Code driving walkthrough.
- `docs/CODE-REVIEW-2026-06-10.md` — adversarial review (35 confirmed findings) with a
  resolution log mapping every finding to its fix commit.
- `docs/CODE-REVIEW-2026-06-10-fixes.md` — re-review of the fixes (10 further findings, all fixed).
- `docs/review/findings-2026-06-10.json` — machine-readable findings.

## State at handoff

- Builds clean on stable Rust (edition 2021). **73 tests green**: 47 unit, 19 CLI integration
  (`tests/cli.rs`), 7 daemon integration (`tests/daemon.rs`); ~83% line coverage measured with
  `cargo llvm-cov`.
- Adversarially reviewed twice (2026-06-10): a 44-agent review produced 35 confirmed findings
  (3 high / 19 medium / 13 low), all fixed one-commit-per-finding; the fix diff was then
  re-reviewed, surfacing 10 concurrency findings, also all fixed. Baseline commit is `d230009`.
- Verified on the installed **release** binary (2026-06-12): `agent-bridge doctor` clean
  (correctly resolves `claude` 2.1.175 via both the bridge's child PATH and a login shell), and a
  full live lifecycle — `start` a `python3 -i` session, `send` an expression, `read` the result,
  `stop` — works end to end. An earlier daemon-backed `python3 -i` session at 45×180 was verified
  the same way, including the idle signal climbing while quiet and resetting on output.
- The verification above was performed on a binary installed with `cargo install --path .`;
  build artifacts and the installed binary were removed afterwards to hand the repo over clean.
  Install the same way from wherever the source lands.

## Inventory

Crate shape: a library (`src/lib.rs`) with a thin binary (`src/main.rs`, clap CLI). ~2,800 lines
of source, ~750 lines of tests.

| Module | Lines | Purpose |
|---|---|---|
| `session` | 1080 | Session lifecycle (start/stop/status/list), metadata, and the PTY supervisor that owns a child process |
| `paths` | 450 | Storage roots, name validation, atomic/private file helpers, command/`PATH` resolution |
| `daemon` | 333 | Unix-socket daemon owning sessions in long-lived threads; request dispatch |
| `logs` | 277 | PTY output capture, input forwarding, bounded log tailing |
| `clean` | 228 | Streaming ANSI/terminal-control stripper feeding the clean log |
| `protocol` | 209 | Line-delimited JSON wire types and the socket client shared by CLI and daemon |
| `doctor` | 144 | Environment and command-resolution diagnostics |
| `procinfo` | 78 | Process start-time tokens making liveness checks robust to PID reuse |
| `keys` | 53 | Terminal key-name → byte-sequence encoding |

Runtime dependencies: `anyhow`, `clap` (derive), `directories`, `libc`, `nix` (fs, signal),
`portable-pty`, `serde`/`serde_json`, `shell-words`, `vt100`. Dev: `assert_cmd`, `predicates`,
`tempfile`, `libc`.

**Platform:** Unix only (Unix domain sockets, `setsid`, process start-time inspection). Developed
and verified on macOS; Linux is intended and unverified.

## Behavioral contract

CLI commands: `start`, `send`, `keys`, `read`, `screen`, `status`, `list`, `stop`, `daemon`,
`shutdown`, `doctor`, plus a global `--direct` flag that bypasses a running daemon.

- `start <name> --cmd "<program ...>" [--cwd DIR] [--rows N] [--cols N]` — geometry defaults to
  40×140 (roomy enough for TUIs like Claude Code) and is fixed for the life of the run.
- There is no `restart` subcommand: `start` on an existing stopped name **is** the restart — it
  bumps the metadata `generation`, rotates the previous run's logs to `*.prev`, and starts fresh.
- `send` writes the text and the Enter keypress as **separate PTY writes** — Claude Code treats
  text-plus-newline in one write as a paste and does not submit. Keep this two-write behavior.
- `status`/`list` report an activity signal (`idle_seconds`, `last_output_unix`, `output_bytes`)
  derived from `raw.log` mtime/size — zero extra writes on the hot path.
- Sessions run daemon-owned when the daemon socket is reachable, otherwise direct mode under a
  detached `setsid` supervisor. Both write identical on-disk state, so read-side commands work
  regardless of which mode started the session. The daemon is optional.

Storage (default `~/.agent-bridge`, overridable via `AGENT_BRIDGE_HOME`, which must be an
absolute path): `daemon.sock` at the root and per-session directories
`sessions/<name>/` containing `metadata.json`, `raw.log`, `clean.log`, `screen.txt`,
`input.fifo`, and the `start.lock`/`status.lock` files.

Wire protocol (`protocol.rs`): line-delimited JSON over the Unix socket; requests are a
snake_case `command`-tagged enum; newer optional fields use `#[serde(default)]` so older clients
stay compatible; the client applies a 60-second I/O timeout so a wedged daemon yields a
diagnostic instead of a hang.

## Invariants — do not regress these

Each of these closed a confirmed (often subtle) bug; the review documents hold the full
reasoning. Any refactor during or after the merge must preserve them:

1. **All session artifacts are owner-only** — directories 0700, files 0600.
2. **Metadata and screen snapshots are written via `paths::write_atomic`** (write-temp-then-rename),
   never in place.
3. **Liveness = PID + process-start-time token** (`procinfo`), so PID reuse can't make a dead
   session look alive. A recorded child PID is always paired with its token; if the token can't
   be read, drop the PID rather than record it bare.
4. **`clean.rs` is a stateful streaming ANSI stripper** — preserves tabs, survives escape
   sequences split across chunk boundaries, and treats a fresh ESC as cancel/restart of any
   in-flight sequence. Do not replace it with the `strip-ansi-escapes` crate (that's what it
   fixed).
5. **Every status transition** (promote-to-Running, mark-Stopped from supervisor exit, `stop`, or
   timeout) happens under the per-session `status.lock` and is guarded by the run's `generation`
   stamp — a stale supervisor from a previous run cannot clobber a restarted run's state.
6. **`start.lock` and `status.lock` are deliberately separate locks.** The start handler holds
   `start.lock` (acquired non-blocking) across the whole startup sequence; merging the two locks
   self-deadlocks the supervisor.
7. **The capture thread is poll-driven and always joined** — never detached/leaked, so shutdown
   is clean and output is never lost mid-write.
8. **The daemon runs each session's start with the client's PATH** (sent in the request), not the
   daemon's own environment, so commands resolve as the user expects.

The integration tests encode most of these; if a merge-time refactor breaks one, the suite should
catch it — keep the tests with the code.

## Merge guidance

- **What moves:** `src/`, `tests/` (including the `tests/common/mod.rs` harness — `TestHome`,
  `wait_until`, `DaemonGuard`, `SessionGuard`), `docs/`, and the README content. The crate drops
  into a Cargo workspace as a member (it is already lib + bin), or as a path dependency if the
  host wants to wrap the library directly.
- **Tests are hermetic:** `TestHome` points `AGENT_BRIDGE_HOME` at a tempdir, so the suite never
  touches `~/.agent-bridge` and parallel runs don't collide. Preserve that property.
- **History and the audit trail:** the review documents reference short commit SHAs (baseline
  `d230009`, one commit per fix tagged with finding IDs like `m1`/`r7`). Merging with history
  preserved (`git subtree add` without `--squash`, or `git merge --allow-unrelated-histories`)
  keeps those SHAs resolvable. A squash or `filter-repo` rewrite breaks them — acceptable, but
  then treat the SHAs in the review docs as informational labels only.
- **Naming:** the binary name (`agent-bridge`), storage root (`~/.agent-bridge`), and env var
  (`AGENT_BRIDGE_HOME`) form the public surface. If the host project rebrands the tool, change
  all three together and migrate or document the storage root move.
- **License:** none chosen yet. Decide at merge time; the prototype has no third-party code
  beyond the crates.io dependencies listed above.

## Deferred work (open roadmap)

None of these block CLI use; they are the backlog the host project inherits, roughly in the order
they were expected to matter (details in `docs/AGENT-BRIDGE.md`):

1. **MCP adapter** — the stated end goal: expose start/send/read/screen/status as MCP tools so a
   manager agent drives the bridge natively instead of shelling out.
2. **Terminal resize after start** — geometry is fixed per run; needs TIOCSWINSZ + SIGWINCH
   propagation to resize a live session.
3. **Approval-prompt detection** — surface "the child is waiting on a y/n or permission prompt"
   as a structured signal instead of making the manager infer it from the screen.
4. **Log rotation / size caps** — `raw.log`/`clean.log` grow unbounded within a run; rotation
   currently happens only on restart.
5. **Daemon auto-start** — the daemon must be launched manually today (the CLI falls back to
   direct mode, so this is a convenience, not a gap).
6. **Daemon-owned session recovery** — sessions owned by a daemon do not survive a daemon
   restart; direct-mode sessions are unaffected.

## Post-merge verification recipe

Run this after the code lands in the host project; all of it should pass before anything builds
on top:

```bash
cargo test                          # expect 73 passing (47 unit / 19 CLI / 7 daemon)
agent-bridge doctor                 # expect warnings: none, claude resolved
agent-bridge start smoke --cmd 'python3 -i'
agent-bridge send smoke 'print(21*2)'
agent-bridge read smoke             # expect 42 in the output
agent-bridge stop smoke
```

Then the end-to-end check that matters: the Claude Code walkthrough in `docs/HOW-IT-WORKS.md`.
