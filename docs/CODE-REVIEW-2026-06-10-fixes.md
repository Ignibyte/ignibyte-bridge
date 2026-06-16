# Adversarial Re-Review of the Fixes — ignibyte-bridge

- **Date:** 2026-06-10
- **Scope:** the `d230009..HEAD` diff — the restructure and the 33 code fixes from the [first review](CODE-REVIEW-2026-06-10.md), reviewed for regressions, incomplete/bypassable fixes, new bugs in the new code, fix-on-fix interactions, and cross-platform (Linux) correctness.
- **Verdicts:** 0 critical, 0 high, 4 medium, 6 low — 10 confirmed, 3 refuted, 0 uncertain. All confirmed findings are now fixed.

## Why a second pass

The first review audited the original Codex monolith. This pass audits *my own fixes* — fixes are where regressions and fresh bugs hide, especially the ~600 lines of new concurrency-sensitive code (per-session locks, atomic writes, the `supervise_pty` restructure, the `AnsiCleaner` state machine, `procinfo`, daemon timeouts). 21 agents across 6 lenses → merge → adversarial verification, same refute-by-default bar as the first pass.

Outcome: the concurrency fixes were sound in the common case but introduced several **TOCTOU races at the boundaries** — multiple unsynchronized writers to session status with no run-identity guard. The cluster (r1/r3/r4/r6/r8) was fixed as a unit with a per-run `generation` stamp and a per-session status lock.

## Confirmed findings (all fixed)

| ID | Severity | Kind | Finding | Fixed in |
|----|----------|------|---------|----------|
| r1 | medium | incomplete-fix | Start-timeout cleanup block terminates/marks without re-checking status or identity: kills a healthy session at the 5s boundary, orphans it, or signals a recycled PID | `dddffc8` |
| r3 | medium | incomplete-fix | Daemon shutdown can orphan a session whose child was just spawned but not yet recorded (process::exit races the supervisor thread) | `dddffc8` |
| r4 | medium | fix-interaction | Old supervisor thread's terminal mark_stopped clobbers a freshly-restarted daemon session, bricking its control plane | `dddffc8` |
| r6 | medium | incomplete-fix | PID-reuse defense (m5) is bypassed for the child when its start-time token fails to record at publish time | `dddffc8` |
| r10 | low | fix-interaction | Daemon shutdown stops sessions serially inside the client connection and can exceed the client's 60s timeout | `2b052ac` |
| r11 | low | incomplete-fix | child_path drops the client's forwarded PATH entirely if BaseDirs::new() fails, reverting a daemon session to the daemon's PATH | `2b052ac` |
| r12 | low | portability | doctor's login-shell comparison hardcodes zsh, ignoring the user's actual login shell so the mismatch warning is wrong or never fires off zsh | `2b052ac` |
| r7 | low | regression | AnsiCleaner does not treat ESC as cancel/restart, leaking control fragments into the clean log | `6ba762d` |
| r8 | low | incomplete-fix | stop during startup can be silently undone: supervise_pty re-promotes a just-stopped session to Running | `dddffc8` |
| r9 | low | fix-interaction | Output-capture thread (and its log/PTY fds) leaks for the daemon's lifetime when the 2s drain cap expires | `c9d2e92` |

### r1 (medium, incomplete-fix) — Start-timeout cleanup block terminates/marks without re-checking status or identity: kills a healthy session at the 5s boundary, orphans it, or signals a recycled PID

- **Anchor:** wait_for_running_metadata timeout branch, src/session.rs:267-278 (terminate_pid at 271/274, mark_stopped at 277) vs supervise_pty publish at src/session.rs:360-368 and the daemon owner thread at src/daemon.rs:236-246
- **Fixed in:** `dddffc8`

**Problem.** wait_for_running_metadata's timeout branch (src/session.rs:267-278), added by the m11/m12/m24 fix, runs after the 50-iteration (~5s) poll loop: it reloads metadata and UNCONDITIONALLY calls terminate_pid(supervisor_pid) (line 271), terminate_pid(child_pid) (line 274), then mark_stopped(name, Some("start timed out"), None) (line 277), without (a) re-checking that status is still Starting and without (b) any pid_is_ours guard — unlike stop_session_silent (session.rs:618,625), which the m5/c2 fix specifically gated. Three concrete failures stem from this one block:

1. KILL-HEALTHY-AT-BOUNDARY (both modes). A slow-starting command (cold-start claude on a loaded host / slow openpty/spawn/atomic-write) has supervise_pty publish status=Running with a live, genuinely-ours child_pid (session.rs:364-368) in the window after the loop's final read but before the cleanup reload. The reload sees Running with a live child; terminate_pid SIGTERM/SIGKILLs it (and its process group via -pid), mark_stopped flips the just-started healthy session to Stopped, and the caller bails 'did not report running within 5 seconds'. In daemon mode the owner thread (daemon.rs:236-246) runs supervise_pty + mark_stopped WITHOUT the start lock while the handler holds it, so the two mark_stopped writes also race into a lost update on exit_status/exit_code. Baseline (d230009) only bail!ed here and killed nothing, so a slow-but-alive start survived and stayed controllable — the fix turned 'slow but alive' into 'actively destroyed'.

2. ORPHAN-AFTER-MARK-STOPPED (daemon mode). supervisor_pid is None in daemon mode, so the block can only terminate child_pid. If the cleanup's load_metadata observes the pre-publish snapshot (Starting, child_pid=None) microseconds before the supervisor thread publishes Running, it terminates nothing, then mark_stopped writes Stopped. The supervisor thread has already passed its single Stopped-check (session.rs:360), so the child now runs unmanaged while metadata says Stopped: status reports Stopped but ps shows the child + its capture/input threads alive, stop is a no-op (stop_session_silent early-returns on Stopped, session.rs:610), read keeps growing, and the orphan only self-heals when the child exits on its own.

3. RECYCLED-PID KILL (both modes). When the start genuinely timed out (still Starting, supervisor/child died), the recorded supervisor_pid/child_pid may have been recycled by the kernel to an unrelated process before the loop ends. terminate_pid -> signal_pid_and_group signals both pid and -pid (session.rs:674) with SIGTERM then SIGKILL, killing a stranger's process and its entire process group. This is exactly the PID-reuse kill that m5/c2/process_start_time tokens were introduced to prevent, but this path ignores the recorded start-time tokens entirely.

**Resolution.** In wait_for_running_metadata's timeout branch (src/session.rs:267-278), converge cooperatively and guard identity instead of acting unconditionally on a stale snapshot:

    if let Ok(metadata) = load_metadata(name) {
        match metadata.status {
            // Raced to Running right at the boundary: it is healthy, report success.
            SessionStatus::Running => return Ok(metadata),
            // Raced to Stopped: apply the same clean-exit-vs-failure logic the loop uses.
            SessionStatus::Stopped => {
                if metadata.exit_code == Some(0) {
                    return Ok(metadata);
                }
                bail!(
                    "session '{}' stopped while starting: {}",
                    name,
                    metadata.exit_status.unwrap_or_else(|| "unknown failure".to_string())
                );
            }
            // Still Starting: only now terminate, and never signal a PID that is not
            // positively ours (defends against PID reuse, like stop_session_silent).
            SessionStatus::Starting => {
                if pid_is_ours(metadata.supervisor_pid, metadata.supervisor_start_time) {
                    let _ = terminate_pid(metadata.supervisor_pid.unwrap() as i32);
                }
                if pid_is_ours(metadata.child_pid, metadata.child_start_time) {
                    let _ = terminate_pid(metadata.child_pid.unwrap() as i32);
                }
                let _ = mark_stopped(name, Some("start timed out".to_string()), None);
            }
        }
    }
    bail!("session '{name}' did not report running within 5 seconds");

This (a) returns success if status already raced to Running, eliminating both the kill-healthy and orphan races; (b) re-checks status==Starting before any terminate/mark; (c) gates each terminate_pid behind pid_is_ours so a recycled PID is never signaled. A more complete fix would make the supervisor (process/thread) the sole writer of terminal state under the per-session lock so the waiter never writes Stopped at all, removing the double-mark_stopped lost-update — but the re-check plus pid_is_ours gating above is the minimal correctness fix and mirrors the guard stop_session_silent already uses.

### r3 (medium, incomplete-fix) — Daemon shutdown can orphan a session whose child was just spawned but not yet recorded (process::exit races the supervisor thread)

- **Anchor:** shutdown_sessions_for_daemon (src/daemon.rs:251-292) + process::exit (src/daemon.rs:146) vs the unrecorded-child window in supervise_pty (src/session.rs:336-368)
- **Fixed in:** `dddffc8`

**Problem.** shutdown_sessions_for_daemon now includes Starting sessions (daemon.rs:267-271, the m24 'don't orphan mid-startup' fix) and calls stop_session_silent on each, then handle_daemon_stream does std::process::exit(0) (daemon.rs:146). But in daemon mode the supervisor is a thread, so supervisor_pid is always None, and there is a window in supervise_pty between spawning the child (session.rs:336) and recording child_pid into metadata (session.rs:365-368). If shutdown runs in that window, stop_session_silent finds child_pid==None and supervisor_pid==None, so pid_is_ours is false for both (session.rs:618,625): it signals nothing and just marks the session Stopped. The child is reaped only if the supervisor thread reaches the m22 'was stopped during startup' bail (session.rs:360) and its reap (session.rs:408) BEFORE process::exit(0) tears the daemon (and that thread) down. process::exit races the supervisor thread and can win, leaving the just-spawned child as an orphan. PTY-master close on daemon exit delivers SIGHUP, which kills most programs, but a child that ignores SIGHUP (or re-execs) survives unreaped and unmanaged — exactly the orphan m24 set out to prevent. Observable: after `shutdown`, a stray process keeps running with no owning daemon and a session dir marked Stopped.

**Resolution.** Record child_pid (and child_start_time) into metadata immediately after spawn_command (src/session.rs:336), BEFORE the FIFO open and the status/Stopped check, instead of only at lines 365-366. Keep status as Starting at that point; flip to Running later as today. With child_pid persisted early, a concurrent shutdown's stop_session_silent will find pid_is_ours(child_pid, child_start_time) true and call terminate_pid(child_pid), which signals the child's own process group (-child_pid; valid because portable-pty setsid's the child) with SIGTERM/SIGKILL on the shutdown thread BEFORE it reaches std::process::exit(0). That guarantees the just-spawned child is terminated regardless of whether the supervisor thread wins or loses the exit race. (Heavier alternatives: have the daemon track and join outstanding supervisor threads, or iterate sessions and signal each PTY child's process group, before process::exit — but the early child_pid record is the minimal correct fix.)

### r4 (medium, fix-interaction) — Old supervisor thread's terminal mark_stopped clobbers a freshly-restarted daemon session, bricking its control plane

- **Anchor:** mark_stopped, src/session.rs:689-695 (clobber) interacting with start_session_in_daemon's spawned thread, src/daemon.rs:243, and supervise_pty publish at src/session.rs:364-368
- **Fixed in:** `dddffc8`

**Problem.** In daemon mode the supervisor runs as a thread; on child exit it calls mark_stopped(name,...) (daemon.rs:243), an unconditional load-modify-write of metadata (mark_stopped, session.rs:689-695) with no identity/generation guard. Scenario: session 'foo' is Running with old child C1 owned by thread T1. C1 exits on its own, so T1 returns from child.wait() and is scheduled to call mark_stopped, but the scheduler hasn't run it yet. A manager agent restarts the session the moment it sees the child died: `start foo`. The start lock is free (T1 holds no lock), session_is_active() returns false because pid_is_ours(C1) sees C1 dead (m5), so the restart proceeds: initialize_session_files rotates logs and writes fresh Starting metadata, and a new thread T2 spawns child C2, sets status=Running, child_pid=C2 (supervise_pty, session.rs:364-368). NOW T1 finally runs mark_stopped: it loads the current (T2/Running/C2) metadata, overwrites status=Stopped with C1's exit_status/exit_code, and saves. The live restarted session is now recorded Stopped. Because write_session_bytes requires status==Running (session.rs:455), every subsequent send/keys fails 'session is not running' even though C2 and its forwarder are alive; C2 is unreachable from the control plane (and leaks) until another restart. wait_for_running_metadata for the T2 start can also observe the clobbered Stopped state and bail 'stopped while starting' or, if C1 exited 0, falsely print 'ran to completion (exit code 0)' for a live restart. Baseline had the same threading, but the fixes (m5 reliably permitting restart of a dead-child Running session + the strict Running gate on send) make this lost update concretely brick an otherwise-healthy session.

**Resolution.** Make the terminal Stopped transition identity-aware so a stale thread cannot clobber a newer run. Concretely, change mark_stopped (session.rs:689) to take the identity of the run the caller owned and skip the write if metadata no longer matches. Simplest correct version using existing fields: pass the child_pid/child_start_time that supervise_pty published (return them, or have the daemon thread read them) and in mark_stopped do `let m = load_metadata(name)?; if m.child_pid != owned_child_pid || m.child_start_time != owned_child_start_time { return Ok(()); }` before setting Stopped. Even more robust: add a monotonically increasing `generation: u64` to SessionMetadata stamped in initialize_session_files and re-read by supervise_pty before publishing Running; mark_stopped then writes Stopped only if the loaded generation equals the one that thread owned. Note both stop_session_silent (session.rs:636) and wait_for_running_metadata's timeout (session.rs:277) also call mark_stopped, so guard the load-modify-write inside mark_stopped (or a shared helper) rather than only at the daemon.rs:243 call site; those callers hold the start identity or, for user-stop, already verified pid_is_ours, so a generation/identity match keeps them working. As a hardening complement, when a restart proceeds over a previous Running run whose child is dead, the new initialize_session_files path could also best-effort terminate any still-live prior child_pid to avoid leaking a child created by a racing thread.

### r6 (medium, incomplete-fix) — PID-reuse defense (m5) is bypassed for the child when its start-time token fails to record at publish time

- **Anchor:** supervise_pty child identity write, src/session.rs:365-366; pid_is_ours fallback, src/session.rs:739-742
- **Fixed in:** `dddffc8`

**Problem.** supervise_pty records child identity as child_pid = child.process_id() and child_start_time = child.process_id().and_then(process_start_time) (session.rs:365-366). If process_start_time returns None at that instant (the child already exited and proc_pidinfo/proc-stat declines to report a token, or a transient proc_pidinfo failure on macOS), metadata ends up with child_pid=Some(pid) but child_start_time=None. Later, pid_is_ours(Some(pid), None) hits the legacy fallback branch (session.rs:739-742) and returns true on bare liveness alone. stop_session_silent and shutdown gate the kill on pid_is_ours (session.rs:618) and, because signal_pid_and_group also signals the negative pid (session.rs:674), send SIGTERM/SIGKILL to that pid AND its process group. If the OS recycled that exact PID to an unrelated process after the child died, stop/shutdown/daemon-exit kills an innocent process group. This is precisely the PID-reuse hole m5 set out to close, but the token-vs-no-token fallback was meant for legacy on-disk metadata, not for freshly written metadata where the token query merely failed. The window is narrow (token must fail to record for a live-then-dead child, the PID must be recycled, and the user must run stop), but the consequence is killing the wrong process group.

**Resolution.** Stop letting a freshly written child identity fall back to bare-liveness. The null-token fallback in pid_is_ours must apply only to genuinely legacy metadata, never to a session we just wrote. Two complementary changes:

1) At publish time (session.rs:365-366), do not record a PID whose start-time token could not be read. Record them as an atomic pair so child_pid is Some only when child_start_time is Some:

    let child_pid = child.process_id();
    let child_start = child_pid.and_then(process_start_time);
    match child_start {
        Some(_) => { metadata.child_pid = child_pid; metadata.child_start_time = child_start; }
        None => { metadata.child_pid = None; metadata.child_start_time = None; }
    }

If the token cannot be read, the child has already exited (or is about to), so dropping child_pid loses nothing useful: supervise_pty still owns the Child handle and reaps it via child.wait() at line 383, and stop/shutdown then simply have no child PID to (mis)signal. This keeps the supervisor_pid path intact (its token is read for a live process at run_supervisor:287, so it is reliable) and confines killing to PIDs we can still positively identify.

2) (Hardening, to also cover legacy metadata written by older builds) Tighten the fallback so an in-window race cannot signal a recycled PID: only honor the None-token bare-liveness path when the recorded created_at_unix is older than this build's metadata schema, or simply require a recorded token for any session whose metadata was written by the current version. Practically, gate the None => true arm in pid_is_ours behind an explicit legacy flag rather than treating every missing token as legacy.

Change (1) alone closes the confirmed scenario; (2) is defense-in-depth for pre-existing on-disk metadata.

### r10 (low, fix-interaction) — Daemon shutdown stops sessions serially inside the client connection and can exceed the client's 60s timeout

- **Anchor:** shutdown_sessions_for_daemon serial terminate_pid loop, src/daemon.rs:281-286 (per-child wait in terminate_pid, src/session.rs:648-665) vs client timeout, src/protocol.rs (CLIENT_IO_TIMEOUT)
- **Fixed in:** `2b052ac`

**Problem.** handle_daemon_request runs shutdown_sessions_for_daemon synchronously in the connection handler and only writes the response after every session is stopped (daemon.rs:188-191, 251-292). Each stop_session_silent calls terminate_pid, which waits up to 20x100ms for SIGTERM then up to 20x100ms for SIGKILL = ~4s per child that ignores SIGTERM (session.rs:648-665), processed one session at a time. The client sets a 60s read timeout (protocol.rs, CLIENT_IO_TIMEOUT). With on the order of ~15+ sessions whose children ignore SIGTERM (or fewer plus other latency), the daemon does not produce the response within 60s, so the client raises 'daemon accepted the connection but did not respond within 60s; it may be suspended or wedged — restart it or rerun with --direct' even though the daemon is healthy and the shutdown is in fact proceeding/completing. Misleading error with an easy workaround (the shutdown still happens), but it directly contradicts the diagnostic the m13/m15 timeouts were meant to provide.

**Resolution.** Acknowledge the shutdown before doing the slow termination, OR bound/parallelize the stops so the client's 60s window is never blocked by serial SIGTERM/SIGKILL grace periods. Minimal option: in handle_daemon_stream, for DaemonRequest::Shutdown, write the success response (e.g. "daemon shutting down\n") FIRST, then run shutdown_sessions_for_daemon, then exit — the client already only needs the ack to know shutdown was accepted. Alternatively, in shutdown_sessions_for_daemon spawn one thread per session so the wall-clock cost is ~max(per-child) instead of sum, and/or shorten terminate_pid's per-phase grace (e.g. SIGTERM grace 1s, SIGKILL grace 1s) for the shutdown path. The reply-first approach is smallest and removes the false "wedged" diagnostic entirely while the daemon still reaps/terminates children before std::process::exit(0).

### r11 (low, incomplete-fix) — child_path drops the client's forwarded PATH entirely if BaseDirs::new() fails, reverting a daemon session to the daemon's PATH

- **Anchor:** child_path, src/paths.rs:246-254 (BaseDirs::new()? at src/paths.rs:249)
- **Fixed in:** `2b052ac`

**Problem.** child_path(Some(client_path)) calls BaseDirs::new()? (paths.rs:249) purely to locate ~/.local/bin for prepending; if BaseDirs::new() returns None (HOME unset and no usable passwd entry), the `?` makes child_path return None even though a perfectly good client PATH was supplied. supervise_pty then skips command.env("PATH", ...) (session.rs:332-334) and the child does not get the client's PATH at all — it inherits the daemon's process environment PATH via portable-pty's get_base_env snapshot instead. The m18 fix's whole point was to run daemon sessions with the user's PATH; under this edge it silently reverts to the daemon's PATH, so a command that resolves in the user's shell can fail to resolve (or resolve to a different binary) in the session. Rare (requires a degenerate environment) but a silent functional regression of the m18 guarantee.

**Resolution.** In child_path (src/paths.rs:246-254), don't gate the whole client PATH on BaseDirs success. When BaseDirs::new() is unavailable, return the client PATH unmodified (only skip the ~/.local/bin prepend). E.g.:

    Some(path) => match BaseDirs::new() {
        Some(home) => Some(path_with_local_bin_from(path, home.home_dir())),
        None => Some(path.to_string()),
    },

This preserves the m18 guarantee (the child still gets the client's PATH) and only loses the ~/.local/bin convenience prepend in the degenerate no-home environment.

### r12 (low, portability) — doctor's login-shell comparison hardcodes zsh, ignoring the user's actual login shell so the mismatch warning is wrong or never fires off zsh

- **Anchor:** doctor login-shell probe, src/doctor.rs:69-72 (warning derived at src/doctor.rs:88-94)
- **Fixed in:** `2b052ac`

**Problem.** The behavior-based doctor (c4) probes the login shell with Command::new("zsh").args(["-lic", "command -v claude; claude --version"]) at doctor.rs:69-72 to compare where the login shell resolves claude against Ignibyte Bridge's resolution, then derives a load-bearing warning at doctor.rs:88-94 ('login shell resolves claude to X, but Ignibyte Bridge resolves Y'). It hardcodes zsh rather than the user's $SHELL. For a user whose login shell is bash/fish, this reads zsh's dotfiles (not their .bash_profile/.zprofile PATH customizations), so the warning compares against the wrong shell and can be misleading or spuriously fire/not-fire. On a host without zsh on PATH (common on Linux, the stated future target, and on macOS accounts using bash/fish), Command::new("zsh") fails with ENOENT, print_command_output prints 'failed: ...', login_path becomes None, and the mismatch warning is silently skipped — the diagnostic intended to catch PATH mismatches becomes inert while showing a confusing error line. This is diagnostic-only output (the doctor subcommand), never a runtime session path, and has an easy workaround, but it is a real correctness/portability gap in code a fix introduced.

**Resolution.** Two-part fix; the report's $SHELL suggestion alone is insufficient because it does not address scenario 3 (first-line stdout noise poisons ANY login shell).

(a) Use the user's actual login shell instead of hardcoded zsh: read std::env::var("SHELL") (fall back to a getpwuid lookup, then "/bin/sh"), and invoke that binary with appropriate login-interactive flags. If the resolved shell binary is unavailable, skip the login-shell section with an explicit informational note (e.g. "login shell '<sh>' unavailable; skipping comparison") rather than emitting a `failed:` line and silently dropping the warning.

(b) Make the parse robust to dotfile stdout noise: do not treat `.lines().next()` as the path. Print a unique sentinel and read the line immediately after it, e.g. run the probe as `printf '__AB_CLAUDE__\n'; command -v claude` and set login_path to the first non-empty line that follows the `__AB_CLAUDE__` marker (a token user dotfiles cannot reproduce). Additionally guard the comparison so it only warns when login_path looks like an absolute path (starts with '/') and is_executable, avoiding spurious warnings from aliases/functions or leftover banner text.

Also add a test that exercises the claude branch (e.g. a fake `claude` on PATH plus a noisy fake login shell) asserting no spurious mismatch warning is produced.

### r7 (low, regression) — AnsiCleaner does not treat ESC as cancel/restart, leaking control fragments into the clean log

- **Anchor:** AnsiCleaner::step + AnsiCleaner::escape, src/clean.rs:48-77 (escape arm at clean.rs:100-109, Csi at clean.rs:57-63, EscapeIntermediate at clean.rs:52-56)
- **Fixed in:** `6ba762d`

**Problem.** clean::AnsiCleaner::step never honors the ECMA-48/vte rule that an ESC byte (0x1b) aborts any in-progress sequence and re-enters the Escape state. In State::Escape, escape() routes a second ESC through the `_ => State::Ground` arm (clean.rs:107), consuming it as if it were a short escape's final byte. In State::Csi (clean.rs:57) and State::EscapeIntermediate (clean.rs:52) an ESC is simply not in the tested terminator range, so it is swallowed while the state persists. Concrete failure: a program emits a CSI interrupted by a fresh CSI, e.g. "\x1b[01;3\x1b[0m" (partial SGR, then a reset). Trace: ESC->Escape, '['->Csi, '0','1',';','3' stay Csi, the second ESC is swallowed (stays Csi), then '[' (0x5b, which IS in the 0x40..=0x7e final-byte range) falsely terminates the first CSI -> Ground, and the trailing '0','m' are emitted as literal text. The human-readable clean.log (and read/--tail over it) then shows a stray '0m' fragment; "\x1b\x1b[0m" similarly leaks '[0m'. The vt100-rendered screen snapshot handles this correctly (vte restarts on ESC), so only the clean log diverges; raw.log is unaffected and no real text is lost or UTF-8 corrupted. Baseline used strip_ansi_escapes::strip (vte-based), which DOES restart on ESC and would have stripped these fragments — so for ESC-interrupted sequences this is a fidelity regression introduced by the c1/m16 rewrite (which in exchange correctly fixed the baseline's worse per-chunk split-sequence leak).

**Resolution.** In AnsiCleaner::step (src/clean.rs:48), add an ESC cancel-and-restart guard at the very top, excluding only the ST-completion states (Osc/OscEsc/StringSeq/StringEsc), which legitimately treat ESC as the start of the ST terminator and must keep their existing handling:

    fn step(&mut self, byte: u8, out: &mut Vec<u8>) {
        if byte == 0x1b
            && !matches!(self.state, State::Osc | State::OscEsc | State::StringSeq | State::StringEsc)
        {
            self.state = State::Escape;
            return;
        }
        match self.state { /* unchanged */ }
    }

This makes an ESC anywhere in Ground/Escape/EscapeIntermediate/Csi cancel the in-progress sequence and re-enter Escape, matching vte. I verified with the probe that this yields "" for both "\x1b[01;3\x1b[0m" and "\x1b\x1b[0m" (matching baseline), keeps "hello" for "\x1b[38;5;240mhello\x1b[0m", and preserves OSC stripping ("\x1b]0;...\x07body"->"body", "\x1b]8;;...\x1b\\link"->"link") because the Osc/StringSeq exclusion prevents the ESC inside an ESC-"\\" ST from wrongly restarting. The exclusion of the ST states is essential — dropping it would break OSC/string-ST stripping.

Minor note for the implementer: the early return skips the pending_cr flush that ground() would otherwise do when an ESC arrives in Ground with pending_cr set. This is observationally harmless (the deferred '\n' is flushed by the next Ground byte; a trailing buffered CR is already intentionally not emitted at end-of-stream per the existing tests), and the probe confirmed "a\r\x1b[0mb" -> "a\nb". If byte-exact parity with the old pending_cr timing is desired, flush pending_cr (push '\n', clear flag) before the early return when state is Ground.

Also add regression tests asserting clean_str(b"\x1b[01;3\x1b[0m") == "" and clean_str(b"\x1b\x1b[0m") == "" alongside the existing OSC tests.

### r8 (low, incomplete-fix) — stop during startup can be silently undone: supervise_pty re-promotes a just-stopped session to Running

- **Anchor:** supervise_pty, src/session.rs:356-368 (load/check-Stopped/set-Running is not atomic w.r.t. concurrent mark_stopped in stop_session_silent)
- **Fixed in:** `dddffc8`

**Problem.** Scenario (daemon mode, also direct): a session is Starting. The user runs `stop s`. stop_session_silent (session.rs:604) loads metadata=Starting, signals whatever PIDs are recorded (often none yet), then mark_stopped writes status=Stopped and the client prints 'stopped session s'. Concurrently the supervise_pty thread is between its metadata load at session.rs:356 and its save at session.rs:368: it read status=Starting (before the user's mark_stopped committed), passed the Stopped-check at session.rs:360, and now writes status=Running with the live child_pid at session.rs:364-368, overwriting the user's Stopped. Observable consequence: the user received a success 'stopped' message, but the session is now Running with a live child that keeps consuming input/producing output; the stop was lost. Versus baseline: baseline supervise_pty had NO Stopped-check and ALWAYS set Running after spawn, so this resurrection happened unconditionally; the m11/m24 fix narrowed it to a check-then-write race but did not make the Starting->Running transition atomic against a concurrent mark_stopped, so the failure still occurs under interleaving.

**Resolution.** Serialize the Starting->Running promotion against stop with a dedicated per-session status lock that the start *handler* does NOT hold (reusing start.lock directly would deadlock: in daemon mode the start handler holds start.lock with LOCK_NB across wait_for_running_metadata, so supervise_pty re-acquiring it would fail EWOULDBLOCK). Add e.g. acquire_status_lock(session_dir) -> flock(LOCK_EX, blocking) on <session_dir>/status.lock, then: (a) in supervise_pty hold it across lines 356-368 (load, the Stopped-check, and the save_metadata that writes Running) so the promotion only commits if it still observes a non-Stopped status under the lock; and (b) in stop_session_silent hold the same lock across the load at :606 through mark_stopped at :636 (load status, signal PIDs, write Stopped). Because both writers mutate status under the same exclusive lock and the start handler never takes it, the promotion and the stop can no longer interleave: a stop that commits during startup is observed by supervise_pty's :360 check (so it bails and the outer guard reaps the child), and a stop that arrives after promotion observes Running and terminates the child. A lighter alternative is a compare-and-set: re-read status immediately before the Running write and skip the write if it is no longer Starting -- but that still has a (smaller) TOCTOU unless done under a lock, so the dedicated status lock is the robust fix.

### r9 (low, fix-interaction) — Output-capture thread (and its log/PTY fds) leaks for the daemon's lifetime when the 2s drain cap expires

- **Anchor:** supervise_pty drain loop, src/session.rs:397-400 (output_thread spawned at src/session.rs:376 is never joined when it does not finish within 2s)
- **Fixed in:** `c9d2e92`

**Problem.** supervise_pty spawns output_thread for capture_output (session.rs:376) and, after the child exits, waits for it only via a bounded is_finished poll with a 2s deadline (session.rs:397-400); it never joins the handle. Scenario: a session whose command backgrounds a process that inherits the PTY slave, e.g. --cmd "bash -lc 'sleep 600 & disown'"; bash exits immediately but the slave fd stays open via the surviving sleep, so capture_output never sees EOF and output_thread.is_finished() stays false. The deadline expires, supervise_pty RETURNS Ok, and the JoinHandle is dropped — detaching a thread that still holds the cloned PTY master reader fd plus the raw.log and clean.log fds and keeps appending to the logs and rewriting screen.txt (for up to vt100's lifetime) for a session now marked Stopped. In daemon mode this leaks one thread and several fds per such session for the daemon's whole lifetime, trending toward fd/thread exhaustion, and produces the surprising effect of a Stopped session whose logs and screen snapshot keep changing. Baseline blocked on the reader (no leak, though it could hang forever — the bug m19 fixed); the m4/m19 drain-cap fix traded the hang for this detached-thread/fd leak and continued writes.

**Resolution.** Plumb a stop signal into capture_output AND force the reader to actually unblock (a flag alone won't wake a blocked blocking read). Concretely in supervise_pty: open the cloned reader with O_NONBLOCK (or, simpler and reliable, after child.wait() set O_NONBLOCK on the reader fd via fcntl), pass an Arc<AtomicBool> stop flag to capture_output, and have capture_output's loop treat WouldBlock as "no data" and re-check stop (sleeping briefly), returning Ok when stop is set. Then after the 2s drain window set stop = true and output_thread.join() unconditionally so the thread, its dup'd master read fd, and the raw.log/clean.log fds are released. Equivalently: keep the master reader fd reachable (do not move it wholly into the thread) and, on timeout, force EOF by closing/shutting the dup'd reader fd so the blocked read returns, then join. Either way the JoinHandle must be joined, never dropped, so no thread/fd is abandoned for the daemon's lifetime and a Stopped session's logs/screen stop changing.

## Refuted findings

Raised by a finder but rejected at verification — kept here for the audit trail.

### r2 — Direct-mode stop during startup kills the supervisor's group but the setsid'd PTY child survives orphaned and becomes unkillable

- **Claim.** Direct mode: `ignibyte-bridge --direct start s --cmd '...'` spawns the setsid supervisor; run_supervisor (session.rs:281) records supervisor_pid + start_time with status=Starting, then supervise_pty openpty's and spawn_command's the child (session.rs:336-339). The child does its OWN setsid() (portable-pty), so it lives in its own process group, not the supervisor's. There is a real window (FIFO open + load_metadata + Stopped-check at session.rs:360-362 + save Running at 368) during which the child PROCESS exists but child_pid is still None in metadata. A concurrent `ignibyte-bridge --direct stop s` in that window: stop_session_silent (session.rs:604) loads status=Starting (not Stopped, so no early-return), sees child_pid=None so pid_is_ours(None,..)=false and skips the child, but supervisor_pid IS recorded so it calls terminate_pid(supervisor_pid). signal_pid_and_group signals -supervisor_pid (session.rs:674), which only reaches the supervisor's own group; the child setsid'd away and is NOT signaled. The single-threaded supervisor (default SIGTERM disposition) dies before it can run the reap guard at session.rs:407-410. Result: the PTY child is orphaned (reparented to launchd), keeps running and holding the PTY slave, while stop_session_silent then mark_stopped writes status=Stopped with child_pid never recorded. Observable: status/list report Stopped, and a subsequent stop early-returns at session.rs:610 because status==Stopped, so the orphan is UNKILLABLE via ignibyte-bridge until it exits on its own. The m12 reap-if-marked-Stopped fix only covers the sub-case where stop marks Stopped before the supervisor reaches line 360; it does not cover stop terminating the supervisor mid-startup in direct mode.

**Why refuted.** The claim's signal-ROUTING mechanism is real, but its load-bearing CONSEQUENCE ("the PTY child keeps running, orphaned, unkillable") is prevented by a kernel mechanism the claim never accounts for, and the regression framing is false.

WHAT THE CLAIM GETS RIGHT (verified against current code):
- portable-pty 0.8.1 (~/.cargo/.../portable-pty-0.8.1/src/unix.rs:200-247) runs in the child's pre_exec: it resets SIGCHLD/SIGHUP/SIGINT/SIGQUIT/SIGTERM/SIGALRM to SIG_DFL (lines 208-217), then calls setsid() (line 220), then TIOCSCTTY (controlling_tty defaults true, cmdbuilder.rs:216; session.rs uses CommandBuilder::new). So the child becomes its OWN session/process-group leader, separate from the supervisor's group.
- The race window is real: in supervise_pty (session.rs:336-368) the child PROCESS exists after spawn_command (339) but metadata.child_pid stays None until save at 368. stop_session_silent (604) reads status=Starting (no early-return at 610), sees child_pid=None so pid_is_ours(None,..)=false and skips the child (618-623), and signals only the supervisor (625-630). signal_pid_and_group (674) sends to [-supervisor_pid, supervisor_pid] — neither reaches the setsid'd child. terminate_pid blocks until the supervisor dies (648-667) BEFORE mark_stopped (636), and the single-threaded supervisor (no SIGTERM handler; output/input threads not yet spawned) dies on default SIGTERM before reaching the reap guard at 407-410. So child_pid is never recorded; final metadata is status=Stopped, child_pid=None.

WHY THE CONSEQUENCE IS REFUTED (the mechanism the claim omits):
When the supervisor dies it closes its sole PTY MASTER fd (pair.master / `master` at session.rs:342; the reader/writer dups at 370-373 are not yet created in this window, so exactly one master fd is open). On both BSD/macOS and Linux, closing the last master fd of a pty delivers SIGHUP to the slave's controlling/foreground process group. The child set itself as that controlling process (setsid+TIOCSCTTY) and its SIGHUP disposition was reset to SIG_DFL by portable-pty. I empirically verified on this macOS host (/tmp probes): closing the master while a setsid+TIOCSCTTY child reads delivers SIGHUP, and a default-disposition child is KILLED by signal 1 (a SIG_IGN child survived with exit 1007). So for the overwhelmingly common case — cat, python3 -i, most REPLs/CLIs, and a Node app like claude that registers no SIGHUP listener — the supervisor's death reaps the child via SIGHUP. The orphan does NOT "keep running"; the claim's blanket assertion is false for normal commands.

NOT A REGRESSION (regression/incomplete-fix tag is unsupported): baseline d230009 stop_session_silent (main.rs:1078-1099) used the identical signal_pid_and_group([-pid, pid]) (main.rs:1133-1138) against the same setsid'd-child topology, recorded child_pid only after spawn (main.rs:741), and so in this exact window equally read child_pid=None and signaled only -supervisor_pid — the child was never reached at baseline either. The new early-return on Stopped (608-612) doesn't worsen it: child_pid is None in this window in BOTH versions, so a subsequent stop had nothing to signal at baseline too. The current code ADDED pid_is_ours gating (618/625) and the m12 reap guard (407-410) — strictly more protection, none removed. So the fixes neither introduced nor worsened this behavior.

RESIDUAL (narrow, pre-existing, not the claimed failure): a target program that explicitly ignores/handles SIGHUP would survive the master-close SIGHUP and then be unreachable via ignibyte-bridge (no recorded child_pid). That is a genuine but much narrower edge that exists identically at baseline and is a property of the PTY design, not a fix regression. It does not support the claim as written (unconditional orphan survival / regression), so the verdict is refuted; the single fact that would flip the narrow subcase is whether the specific launched program installs a SIGHUP handler.

### r5 — Full-disk capture write failures silently blind the manager agent: all session output is discarded with no client-visible signal

- **Claim.** The m20 resilience change makes capture_output treat every sink write error as warn-once-and-continue (logs.rs:54-71): on a full disk (ENOSPC) the raw log, clean log, AND screen snapshot all stop being written after a single eprintln to the daemon's stderr, while the loop keeps draining the PTY. The session stays Running and write_session_bytes keeps succeeding, so a manager agent can still send/keys, but every read/screen returns stale or empty content with ok=true and no error. The only failure signal (warn_once) goes to the daemon process's stderr, which a programmatic driver never sees. Net effect under a plausible condition (disk fills while a session runs): the agent drives the session completely blind — it cannot tell its view is frozen — and all transcript output for that period is lost. This is the intended 'don't wedge the child' tradeoff, but the lack of any client-observable indication turns a recoverable disk-full condition into silent, undiagnosable data loss for the consumer.

**Why refuted.** The claim's factual mechanics are accurate, but the conclusion (that this is a fix-introduced finding) is wrong: it is the explicitly-excluded intentional tradeoff and is not a regression.

VERIFIED FACTS (all true):
- capture_output (src/logs.rs:54-71) warn-once-and-continues per sink on write error; on ENOSPC the raw log, clean log, and screen snapshot (via write_atomic, src/logs.rs:67) all stop updating while the loop keeps draining the PTY.
- The session stays Running: supervise_pty only reaches mark_stopped when child.wait() returns (src/session.rs:383; daemon.rs:243). A draining-but-failing capture never stops the child.
- write_session_bytes (src/session.rs:452-482) checks only status==Running and writes the FIFO; it never touches the capture sinks, so send/keys keep returning ok=true.
- read_output_text/read_screen_text/status_text read the now-stale files; the daemon wraps them as ok=true,error=None (daemon.rs:181-199). No error reaches the client.
- The only signal, warn_once's eprintln (src/logs.rs:75-82), goes to supervisor/daemon stderr. In direct mode the supervisor is spawned .stderr(Stdio::null()) (session.rs:118); in daemon mode capture runs inside the daemon whose stderr a programmatic client never reads. So a socket consumer sees nothing.

WHY THIS IS NOT A FINDING:
1) NOT A REGRESSION. At baseline d230009, capture_output `?`-propagated a write error, but that error returned into output_thread's JoinHandle and was DISCARDED by `let _ = output_thread.join();` (baseline main.rs:763). The supervisor was blocked on child.wait() (baseline main.rs:760) and only marked the session Stopped when the child independently exited, recording the CHILD's exit_status — never the capture error. So reads/screen returned stale content with ok=true and no error AT BASELINE TOO — identical client-observability. Worse, because the reader thread had exited, the PTY buffer would fill and the child would wedge (frozen zombie still reporting Running). The m20 fix did not remove any pre-existing client signal (there was none) and STRICTLY IMPROVED the failure mode by continuing to drain. Even the intermediate pre-m20 modular code discards output_thread's Result (it only polls is_finished(), session.rs:398-400), so no version ever surfaced a capture error to the client. The rule "REGRESSION = the fix broke something that worked at baseline" does not apply.

2) INTENTIONAL TRADEOFF + ROADMAP ENHANCEMENT. The prompt names the m20 continue-on-write-error as the intended "don't wedge the child" tradeoff and excludes such intentional/correct design choices and design-doc future gaps from being findings. The claim itself concedes "This is the intended 'don't wedge the child' tradeoff." Its proposed remedy is to ADD a new capability (a "capture degraded" metadata/exit_status field) — an enhancement, not a bug fix. The fix does exactly what it was designed to do, correctly.

3) THE PROPOSED REMEDY IS DEFEATED BY THE CONDITION. On a full disk, persisting a "capture degraded" marker via write_atomic to metadata (session.rs:712-718) would itself fail with ENOSPC, so the suggested signal could not be reliably recorded. This confirms the stale-reads behavior is an inherent property of the durable-file design under disk exhaustion, not a localized defect in capture_output.

The fix is complete and correct; no concrete bug is introduced or left by it.

### r13 — On a Unix that is neither macOS nor Linux, the new PID-reuse defense silently degrades to bare liveness for every session

- **Claim.** procinfo::process_start_time has three cfg blocks: macOS (proc_pidinfo), Linux (/proc/<pid>/stat), and not(macos/linux) which unconditionally returns None (procinfo.rs:53-58). The m5/c2 fix made pid_is_ours (session.rs:739-742) treat a recorded start-time token of None as 'fall back to a bare liveness probe.' On a current build for any third Unix (e.g. FreeBSD/illumos), run_supervisor (session.rs:287) and supervise_pty (session.rs:366) therefore ALWAYS record None for supervisor_start_time/child_start_time, so EVERY session — not just legacy metadata — loses the PID-reuse protection the fix added: after an unclean shutdown a recycled PID is accepted as 'ours', and stop_session_silent (session.rs:618-630) would then SIGTERM/SIGKILL that PID and its whole process group. The team's stated scope is 'macOS now, Linux later', so a BSD build is not a declared target and CI would not exercise it; on the two real targets the tokens are populated and the defense holds. This is distinct from the prompt's explicitly-excluded legacy-metadata case (an old file): here it is a freshly written file on an unsupported OS.

**Why refuted.** The claim's CODE mechanism is real and verified, but under this review's explicit rules it is NOT an in-scope finding. Refuting mechanism by mechanism:

VERIFIED FACTS (all true):
- procinfo.rs:53-58: the not(any(macos,linux)) arm returns None unconditionally with no compile_error!. Every other platform call in src/ is POSIX-portable libc (setsid session.rs:124; flock session.rs:178/daemon.rs:84; O_NONBLOCK/fcntl session.rs:352/465/490/495; ENXIO session.rs:468; geteuid paths.rs:93; O_NOFOLLOW paths.rs:186) — all present on FreeBSD/illumos via the libc crate. So a third-Unix build genuinely compiles and process_start_time always returns None there.
- run_supervisor records supervisor_start_time = process_start_time(pid) (session.rs:287); supervise_pty records child_start_time = child.process_id().and_then(process_start_time) (session.rs:366) — both None on a third Unix.
- pid_is_ours treats None as the bare-liveness arm: None => true (session.rs:739-742).
- stop_session_silent (session.rs:618-630) -> terminate_pid (646) -> signal_pid_and_group (670-687) signals Pid::from_raw(-pid) AND from_raw(pid), so a recycled PID and its group would be hit.

WHY THIS IS REFUTED (three independent, each sufficient, grounds — the claim concedes the first two):

1. EXPLICITLY EXCLUDED BY THE PROMPT. The 'WHAT IS NOT A FINDING' list names 'pid_is_ours falling back to bare liveness when no start-time token was recorded.' The third-Unix case IS exactly that fallback, reached through the deliberately-written None-returning arm. The '(legacy metadata only)' parenthetical describes the expected trigger on the SUPPORTED platforms; it does not turn the same intentional None-fallback into a bug on a platform the design never targeted. The behavior the design intentionally accepts (no token => bare liveness) is identical whether the None comes from old metadata or from the third-Unix cfg arm.

2. NO FAILURE ON A SUPPORTED PATH. Project scope is fixed as 'macOS now, Linux later,' and the rubric requires 'a concrete failure scenario with observable consequence.' On both declared targets the tokens ARE populated (macOS arm procinfo.rs:19-39 and Linux arm procinfo.rs:44-50 both return Some), so the defense holds. The scenario manifests only on FreeBSD/illumos — never built, shipped, or CI'd.

3. NOT A REGRESSION. Verified against baseline d230009:src/main.rs: zero start-time machinery (grep: only the group-kill at line 1137 matched), bare liveness everywhere (is_some_and(|pid| process_alive) at 538/596/964/1018), and stop_session_silent (line 1078) signaled any recorded Some(child_pid)/Some(supervisor_pid) UNCONDITIONALLY (lines 1083/1089) with the same from_raw(-pid) group-kill. So baseline accepted (and signaled) recycled PIDs on EVERY platform, with no identity OR liveness gate in stop. The current code on a third Unix is strictly NO WORSE — it additionally requires the PID to be alive (pid_is_ours' process_alive gate, session.rs:736) before signaling. The fix improved macOS+Linux and left a third Unix no-worse-than-baseline; nothing that worked was broken, so the 'regression is at least the severity of what it broke' clause does not apply.

The claim itself frames its value purely as hardening on an undeclared target — below the prompt's stated bar ('concrete failure scenario with observable consequence' on a supported path). The first verifier's 'uncertain' rested solely on declining to apply the rubric's scope/exclusion clauses; those clauses are unambiguous here. Mechanism real, but not an in-scope finding: intentionally-accepted None-fallback, undeclared platform, no regression. Verdict: refuted.
