# Adversarial Code Review — agent-bridge

- **Date:** 2026-06-10
- **Baseline:** commit `d230009` (`src/main.rs`, 1,321 lines, as written by Codex). All line references are against this commit.
- **Verdict counts:** 0 critical, 3 high, 19 medium, 13 low — 35 confirmed, 1 refuted, 0 uncertain.

## Methodology

44 agents in a four-stage adversarial pipeline:

1. **Find** — 6 independent reviewers, each reading the full source through one lens: concurrency/cross-process races, Unix/PTY semantics, security, error handling & resources, protocol/CLI boundary, spec-vs-docs conformance.
2. **Merge** — finder output plus a 16-item manually-curated seed list deduplicated into 32 distinct findings.
3. **Critic** — a completeness pass over functions/categories with no findings; added 4 more (c1–c4).
4. **Verify** — one adversarial verifier per finding, instructed to *refute* by default, checking claims against POSIX/macOS semantics, Rust std behavior, and crate sources at the locked versions. Only findings surviving refutation are listed as confirmed. Verifiers could adjust severity; where they did, both values are shown.

Roadmap gaps the design doc already acknowledges (resize, idle detection, MCP adapter, log rotation, daemon auto-start) were excluded by instruction.

## Summary

| ID | Severity | Finding | Status |
|----|----------|---------|--------|
| m1 | high | Session logs, screen snapshots, and metadata created world/group-readable (0644 files in 0755 dirs), exposing every session secret to other local users | open |
| m3 | high | Daemon-routed start resolves omitted/relative --cwd against the daemon's working directory, not the client's | open |
| m4 | high | Daemon mode permanently leaks the forward_input thread, the FIFO fd, and the PTY master fd per session lifecycle, eventually wedging the daemon on EMFILE/PTY exhaustion | open |
| c1 | medium | clean.log silently deletes tabs (and every C0 control except \n), corrupting `read` output | open |
| c2 | medium | Stale 'Running' metadata plus PID reuse falsely blocks `start` and falsifies status/list after any unclean shutdown | open |
| c3 | medium | Client has no socket timeouts: every CLI command hangs forever when the daemon accepts but never replies | open |
| m10 | medium | --direct start silently clobbers a live daemon-owned session of the same name, orphaning its child (asymmetric liveness check) | open |
| m11 | medium | start-vs-start TOCTOU: the check-init-spawn sequence has no lock and a session in Starting status never blocks a second start | open |
| m12 | medium | stop during session startup is silently lost: no status gate, pids not yet recorded, and the owner later unconditionally writes Running | open |
| m13 | medium | Daemon shutdown calls std::process::exit(0) while request threads and session owners are mid-flight, and skips Starting sessions entirely | open |
| m14 | medium | AGENT_BRIDGE_HOME is trusted verbatim and the docs point it at world-writable /tmp, enabling symlink/pre-creation attacks | open |
| m16 | medium | Stateless per-chunk ANSI stripping corrupts clean.log whenever an escape sequence splits across an 8KB read boundary | open |
| m17 | medium | read loads the entire unbounded log into memory to tail a few lines | open |
| m18 | medium | Daemon-started sessions use the daemon's PATH/env, while `doctor` always diagnoses the client's environment | open |
| m19 | medium | Session exit is never detected when grandchildren hold the PTY slave: output-thread join hangs and metadata stays Running forever | open |
| m2 | medium | metadata.json written with truncate-then-write (fs::write), racing every concurrent reader and writer with no atomicity | open |
| m20 | medium | Any capture-thread write error (disk full) silently kills output capture and freezes the session as a zombie 'Running' | open |
| m5 | medium | stop blind-signals persisted PIDs with no identity or status check: after crash/reboot it SIGKILLs a reused PID and its whole process group, and EPERM wedges the session forever | open |
| m6 | medium | read and read --raw fail permanently once a session log contains non-UTF-8 bytes | open |
| m7 | medium | Session names '.', '..', and leading '-' pass validation: '..' scribbles session files into the bridge root, and '-name' breaks the direct-mode supervisor re-spawn | open |
| m8 | medium | screen.txt is rewritten with non-atomic fs::write on every PTY chunk, so concurrent screen reads see empty or truncated screens | open |
| m9 | medium | FIFO input writes are not atomic: O_NONBLOCK write_all delivers a partial prefix then errors on large sends, and concurrent senders can interleave mid-message | open |
| c4 | low | doctor ships hardcoded author-machine heuristics that emit false warnings on every other install layout | open |
| m15 | low | Daemon has no read timeout or request-size limit and spawns an unbounded thread per connection (local DoS) | open |
| m21 | low | Daemon startup socket takeover is a check-connect-remove-bind TOCTOU that can unlink a live daemon's socket | open |
| m22 | low | Daemon-mode error between spawn and wait leaks an unreaped child that becomes a permanent zombie and wedges stop/shutdown | open |
| m24 | low | 5-second start timeout reports failure but leaves the detached supervisor running, contradicting itself | open |
| m25 | low | Daemon responses flatten error chains, hiding root causes and conflating missing session with corrupt state | open |
| m26 | low | Malformed or unknown daemon requests get no response, so clients see only 'daemon closed connection without a response' | open |
| m27 | low | `send` text beginning with '-' is rejected by clap instead of being sent to the session | open |
| m28 | low | Doc example `agent-bridge send claude "..." --enter` errors: the flag is --no-enter | open |
| m29 | low | README quickstart never exercises the daemon it starts: AGENT_BRIDGE_HOME on the daemon line only | open |
| m30 | low | start reports failure for fast-exiting commands even when they succeed (exit code 0) | open |
| m31 | low | Restarting a session name silently erases its previous raw/clean logs and screen | open |
| m32 | low | send immediately after start can hit the FIFO before any reader exists (ENXIO window): Running is saved before forward_input opens the FIFO | open |

## Confirmed findings

### m1: Session logs, screen snapshots, and metadata created world/group-readable (0644 files in 0755 dirs), exposing every session secret to other local users

- **Severity:** high (finder claimed critical; adjusted at verification)
- **Anchor:** initialize_session_files src/main.rs:647 (also run_daemon create_dir_all src/main.rs:330, start_session_detached src/main.rs:534, save_metadata src/main.rs:1188)
- **Found by:** security, unix-pty, errors, manual-read
- **Status:** open

**Problem.** The code locks down only the socket (chmod 0600, run_daemon src/main.rs:343) and the FIFO (mkfifo 0600, src/main.rs:644); everything else is created under the default umask: ~/.agent-bridge and every session dir via create_dir_all (0755), raw.log/clean.log/screen.txt via File::create (0666 & umask = 0644), metadata.json via fs::write (0644). Verified empirically on this machine (umask 022 -> dirs 0755, files 0644). raw.log/clean.log capture every PTY byte — typed prompts, pasted keys, terminal-echoed input, and all program output (API keys, auth tokens, source, Claude transcripts) — and metadata.json records the full command line, which may carry secrets as arguments. On stock macOS every human account is in group staff and /Users/<user> is drwxr-x--- (group staff r-x), so another local staff user can traverse home -> ~/.agent-bridge (0755) -> sessions/<name> (0755) and read all transcripts; on any host with a 0755 home, 'other' gets in too. No chmod is ever applied to these artifacts, so the exposure lasts their whole lifetime.

**Recommended fix.** Set umask(0o077) once at the top of main() (unsafe { libc::umask(0o077); }) so every creation site inherits owner-only modes; additionally (defense in depth) create directories with DirBuilder::new().recursive(true).mode(0o700) at src/main.rs:330, 534, 592, 992, and open raw.log/clean.log/screen.txt/metadata.json with OpenOptions::new().write(true).create(true).mode(0o600) instead of File::create/fs::write at lines 647-649, 801, 1188 (the truncating rewrites preserve the inode mode, so fixing creation modes covers steady state). On session start, chmod any pre-existing session dir/files to 0700/0600 to repair artifacts created by older versions.

### m3: Daemon-routed start resolves omitted/relative --cwd against the daemon's working directory, not the client's

- **Severity:** high
- **Anchor:** start_session_in_daemon src/main.rs:584 (root cause daemon_request_for_command src/main.rs:243)
- **Found by:** concurrency, unix-pty, protocol, spec, manual-read
- **Status:** open

**Problem.** daemon_request_for_command forwards cwd: Option<PathBuf> verbatim (src/main.rs:243-247) without client-side resolution; start_session_in_daemon resolves None via std::env::current_dir() OF THE DAEMON PROCESS and canonicalizes relative paths against the daemon's cwd (src/main.rs:583-586). In the recommended setup (daemon up, commands auto-routed), `agent-bridge start s --cmd claude` run from inside a project silently starts Claude in whatever directory the daemon was launched from ($HOME, etc.), so the coding agent reads/edits the wrong repository. Verified empirically: with a daemon launched from /private/tmp/ab-daemon-cwd, `start py --cmd "python3 -i"` from ~/Projects/agent-bridge produced a session whose status reports cwd /private/tmp/ab-daemon-cwd. `--cwd ./subdir` likewise resolves against the daemon's cwd (wrong directory or a confusing 'failed to canonicalize cwd' error). Direct mode resolves the client's cwd correctly, so behavior changes silently depending on whether a daemon happens to be reachable.

**Recommended fix.** Resolve cwd in the client before the request is sent: in main (or by changing daemon_request_for_command to return Result), for Commands::Start compute `let cwd = cwd.map_or_else(std::env::current_dir, Ok)?.canonicalize()?` and send DaemonRequest::Start with that absolute path (ideally make the wire field a non-optional PathBuf). As defense in depth, have start_session_in_daemon bail on a missing or non-absolute cwd instead of falling back to the daemon's current_dir().

### m4: Daemon mode permanently leaks the forward_input thread, the FIFO fd, and the PTY master fd per session lifecycle, eventually wedging the daemon on EMFILE/PTY exhaustion

- **Severity:** high
- **Anchor:** forward_input src/main.rs:809 (spawned and abandoned at supervise_pty src/main.rs:758)
- **Found by:** unix-pty, errors, concurrency, manual-read
- **Status:** open

**Problem.** forward_input loops forever (src/main.rs:809-830): it holds the FIFO open O_RDWR — the thread is its own writer, so read() never returns EOF and the read==0 branch is dead code — and nothing signals it when the child exits; supervise_pty joins only the output thread (line 763) and abandons the input thread (`_input_thread`, line 758). The thread also owns the PTY master writer (portable-pty take_writer dups the master fd — verified in portable-pty unix.rs — so drop(pair.master) does not release the PTY). In direct mode the supervisor process exit reaps everything, but in the long-lived daemon every start/stop cycle permanently leaks 1 blocked OS thread + 2 fds (the old FIFO inode and the dup'd PTY master) + 1 system-wide PTY slot (the device frees only when the last master fd closes). At macOS's default 256-fd soft limit, roughly 120 session lifecycles wedge the daemon: openpty fails ('failed to open PTY') for all new sessions, listener.accept() degenerates into a hot error-print spin taking down all commands, and ptmx exhaustion can affect other terminal apps. Only a daemon restart recovers.

**Recommended fix.** In supervise_pty, keep the input thread's JoinHandle and make it exitable: share an Arc<AtomicBool> stop flag with forward_input; have forward_input check the flag after every read and return when set. After child.wait() returns, set the flag, then wake the blocked reader by opening the session FIFO O_WRONLY|O_NONBLOCK (same pattern as write_session_bytes) and writing a single sentinel byte (discarded by the forwarder when the flag is set), then join the input thread before supervise_pty returns. The thread exit drops both the FIFO fd and the UnixMasterWriter (the dup'd PTY master fd), freeing the pty slot. Alternatively, open the FIFO in forward_input with O_NONBLOCK and convert the loop to treat EAGAIN as sleep-then-check-flag, which removes the need for the sentinel write.

### c1: clean.log silently deletes tabs (and every C0 control except \n), corrupting `read` output

- **Severity:** medium
- **Anchor:** capture_output src/main.rs:794
- **Found by:** critic
- **Status:** open

**Problem.** strip_ansi_escapes::strip's vte Performer only forwards b'\n' from execute(); every other C0 control byte -- \t, lone \r, \x08 -- is silently dropped (verified in strip-ansi-escapes 0.2 source: `fn execute(&mut self, byte: u8) { if byte == b'\n' { ... } }`). Concrete failures: (1) any tab-containing output (cat-ing a Makefile inside a session, go test/gofmt output, TSV data, a REPL printing 'a\tb') reaches clean.log with the tabs deleted, so `agent-bridge read` shows 'ab' -- the manager AI sees silently corrupted file content and will reproduce the corruption if it copies it back; (2) lone-CR progress redraws (pip/npm/cargo spinners) lose their only line separator, fusing hundreds of overwritten frames into one mega-line, which breaks `--tail` line counting. This is distinct from the already-found chunk-boundary escape-splitting finding: it happens on every chunk, by the crate's design, with no chunking involved. The only workaround, `read --raw`, fails the stated purpose (it is full of ANSI noise) and is separately broken on non-UTF-8.

**Recommended fix.** Replace strip_ansi_escapes in capture_output (src/main.rs:794) with a small custom vte::Perform implementation kept as one persistent parser across read chunks (this also fixes the chunk-boundary escape-splitting issue). In its execute(): write the byte for b'\t' and b'\n'; for b'\r' emit a newline but suppress a newline for an immediately following b'\n' (i.e., collapse \r\n to one \n, turn lone \r into \n); ignore the rest. vte 0.14.1 is already in the dependency tree via strip-ansi-escapes, so this adds no new dependency.

### c2: Stale 'Running' metadata plus PID reuse falsely blocks `start` and falsifies status/list after any unclean shutdown

- **Severity:** medium
- **Anchor:** start_session_detached src/main.rs:537-545
- **Found by:** critic
- **Status:** open

**Problem.** If the session owner dies without running mark_stopped (supervisor SIGKILLed, daemon crash, power loss), metadata.json keeps status=Running with the old pids forever. The start gate then trusts a bare kill(pid,0) liveness probe: start_session_detached src/main.rs:537-545 (and start_session_in_daemon src/main.rs:595-603) bails "session '<name>' is already running" as soon as the recorded supervisor/child pid has been recycled by ANY unrelated process -- routine on macOS where pids wrap at 99999 within days of normal use. Because process_alive (src/main.rs:1308-1314) also maps EPERM to alive, a pid recycled by a root-owned daemon blocks the name indefinitely. Result: a core command (`start`) refuses to work for that session name even though nothing is running, and status_text/list_sessions_text (src/main.rs:962-967, 1016-1018) report supervisor_alive/child_alive=true for an impostor process, misleading the operator. The only built-in remedy is `stop`, which the already-found stop finding shows SIGKILLs the innocent recycled pid and its process group -- so the safe path out is hand-editing metadata.json. This is the read-side/staleness half of the persisted-pid problem; the existing finding covers only stop's blind signaling.

**Recommended fix.** When recording supervisor_pid/child_pid, also persist a process-identity token — on macOS the process start time from proc_pidinfo(pid, PROC_PIDTBSDINFO) (pbi_start_tvsec/usec), on Linux the starttime field of /proc/<pid>/stat. In process_alive-based checks (start gates at src/main.rs:537-545 and 595-603, status_text, list_sessions_text, stop_session_silent), treat the pid as the session only if both the pid is alive AND the stored start time matches; treat EPERM as not-ours. On identity mismatch, reconcile metadata to Stopped (e.g., exit_status "lost: owner died") instead of bailing, so start proceeds and stop never signals a recycled pid.

### c3: Client has no socket timeouts: every CLI command hangs forever when the daemon accepts but never replies

- **Severity:** medium
- **Anchor:** try_send_daemon_request src/main.rs:296-300
- **Found by:** critic
- **Status:** open

**Problem.** try_send_daemon_request never calls set_read_timeout/set_write_timeout (grep confirms zero timeouts in the whole binary). After a successful connect it blocks in read_line at src/main.rs:296-300 indefinitely. Concrete scenario requiring no malice: the docs/README run the daemon in a foreground terminal; the user suspends that terminal job with ctrl-Z (SIGSTOP) or the terminal app stops scheduling it. The kernel still completes new connections via the listen backlog and buffers the request write, so every subsequent agent-bridge command (start/send/read/screen/status/list/stop) hangs silently forever -- no error, no fallback to direct mode. For the primary use case (a manager AI shelling out to this CLI), one wedged daemon stalls the whole agent workflow with no diagnostic. This is the client-side counterpart, in a different process and code path, of the already-found daemon-side missing read timeout (that finding is about DoS-ing the daemon, not about the CLI hanging).

**Recommended fix.** In try_send_daemon_request, after UnixStream::connect succeeds, call stream.set_write_timeout(Some(...)) and stream.set_read_timeout(Some(...)) -- e.g., 2s for Send/Keys/Read/Screen/Status/List and ~15s for Start/Stop/Shutdown (stop legitimately takes up to ~4s of bounded polling daemon-side). On ErrorKind::WouldBlock/TimedOut, return a clear error such as "daemon at <socket path> accepted the connection but did not respond; it may be suspended or wedged -- resume/restart it or rerun with --direct" instead of propagating a bare IO error or hanging.

### m10: --direct start silently clobbers a live daemon-owned session of the same name, orphaning its child (asymmetric liveness check)

- **Severity:** medium
- **Anchor:** start_session_detached src/main.rs:537-545
- **Found by:** concurrency, unix-pty, protocol, spec, errors
- **Status:** open

**Problem.** start_session_detached's already-running guard (src/main.rs:537-545) only bails if supervisor_pid is alive, but daemon-owned sessions always have supervisor_pid=None — the daemon path correctly checks child_pid instead (src/main.rs:595-602). So `agent-bridge --direct start name` against a live daemon session — a documented debugging workflow — passes deterministically, no race timing required. Verified: with daemon-owned 'py' (child 38402) Running, `--direct start py` succeeded; metadata repointed to new pids 38408/38409 and python 38402 stayed alive but untracked — `stop` can never reach it again. initialize_session_files deletes the live FIFO and truncates raw/clean/screen logs while the daemon's capture thread holds open append fds, so two PTYs interleave into one raw.log/clean.log and fight over screen.txt; when the daemon's owner thread later sees its child exit it calls mark_stopped, clobbering the new supervisor session's Running metadata so send/status report 'not running' for a session that is alive. The same hole fires without --direct after a daemon crash (socket gone so commands go direct, but the HUP-surviving child is still alive).

**Recommended fix.** Use one shared liveness predicate in both start paths, e.g. add `fn session_is_active(m: &SessionMetadata) -> bool { m.status == SessionStatus::Running && (m.supervisor_pid.is_some_and(|p| process_alive(p as i32)) || m.child_pid.is_some_and(|p| process_alive(p as i32))) }` and call it from both start_session_detached (src/main.rs:537-545) and start_session_in_daemon (src/main.rs:595-603), bailing "session already running" if it returns true.

### m11: start-vs-start TOCTOU: the check-init-spawn sequence has no lock and a session in Starting status never blocks a second start

- **Severity:** medium
- **Anchor:** start_session_detached src/main.rs:537 (mirror at start_session_in_daemon src/main.rs:595)
- **Found by:** concurrency, errors, protocol, manual-read, unix-pty
- **Status:** open

**Problem.** Both start paths do load_metadata -> check -> initialize_session_files -> spawn with no lock (direct: src/main.rs:537-575; daemon: 595-619 — thread-per-connection with no shared in-memory registry), and the check only bails when status==Running: a session in Starting (the entire spawn window, plus forever if a previous supervisor crashed pre-Running) is not protected at all. Two concurrent `start foo` (a manager-agent retry after the 5s wait timeout or a torn-metadata parse failure, parallel tool calls, or two agents) both pass the check: the second deletes the first's FIFO and truncates the logs mid-setup (lines 641-649, with a possible mkfifo EEXIST race between the two removes), and two supervisors/owner-threads each spawn a child. Result: two children of the same name; metadata records only the last writer's pids; both capture threads interleave bytes into the same raw.log/clean.log and alternately rewrite screen.txt with different vt100 states; sends reach only the instance holding the new FIFO; and `stop` kills only the recorded pids — the other child and its supervisor leak untracked and keep scribbling the logs, while both waiting clients report 'started' from whichever metadata won.

**Recommended fix.** In both start_session_detached and start_session_in_daemon, take an exclusive advisory lock before the metadata check and hold it through wait_for_running_metadata: open session_dir/start.lock and flock(LOCK_EX | LOCK_NB) (via nix::fcntl::Flock or libc::flock on the File fd); if the lock is held, bail "session '<name>' is already starting". Because the lock is held until the session reaches Running (or the start fails), a concurrent start either blocks/bails on the lock or arrives after Running and is caught by the existing status check. Keep allowing a stale Starting record (crashed supervisor) to be restarted once the lock is acquired. A daemon-only Mutex<HashSet<name>> is insufficient because direct mode (--direct or no daemon) bypasses the daemon process; the per-session lock file covers both paths and mixed mode.

### m12: stop during session startup is silently lost: no status gate, pids not yet recorded, and the owner later unconditionally writes Running

- **Severity:** medium
- **Anchor:** stop_session_silent src/main.rs:1078
- **Found by:** concurrency
- **Status:** open

**Problem.** stop_session_silent (line 1078) kills only the pids currently in metadata and then marks Stopped — it never gates on status or prevents a future transition. If stop lands while the session is Starting (daemon mode: child_pid and supervisor_pid both None until line 743; direct mode: before run_supervisor writes its pid at 692), it kills nothing, reports 'stopped session' success, and writes Stopped; the owner thread/supervisor then proceeds to spawn the child and unconditionally sets status=Running (supervise_pty lines 740-743), resurrecting the session the manager believes is dead. Worse, the concurrently-waiting start client can load the transient Stopped and report 'session stopped while starting: stopped by user' — leaving a session the manager believes both failed and stopped, but which is actually running. In direct mode there is also a leak variant: stop kills the supervisor (line 1090) in the window after spawn_command returns but before child_pid is saved (lines 732-743); the child was setsid'd into its own process group, so kill(-supervisor) never reaches it and its pid was never recorded — an orphaned interactive child the tool can no longer stop.

**Recommended fix.** Two small changes. (a) In supervise_pty, gate the promotion to Running: after spawn_command, when re-loading metadata (line 738), if status == Stopped (stop requested during startup), kill the just-spawned child (terminate_pid on child.process_id()) and return "stopped during startup" instead of writing Running. Apply the same status re-check in run_supervisor before/when saving supervisor_pid at 692. (b) In stop_session_silent, when the loaded status is Starting and both pids are None, do not silently succeed: briefly poll (e.g. up to ~2s) for pids to appear and kill them, or bail with "session is still starting; retry" — and still write Stopped so the owner's re-check in (a) aborts the spawn.

### m13: Daemon shutdown calls std::process::exit(0) while request threads and session owners are mid-flight, and skips Starting sessions entirely

- **Severity:** medium
- **Anchor:** handle_daemon_stream src/main.rs:388 (shutdown_sessions_for_daemon src/main.rs:1032-1069)
- **Found by:** concurrency, manual-read, security
- **Status:** open

**Problem.** shutdown_sessions_for_daemon stops only sessions whose metadata says Running at scan time (src/main.rs:1032-1069); handle_daemon_stream then calls std::process::exit(0) (line 388), killing all daemon threads at arbitrary points. A Start racing the shutdown (child spawned after the scan, or status still Starting) is skipped by the scan and its owner thread is destroyed by exit(0): a live PTY child is orphaned with no FIFO reader, metadata is permanently stuck at Starting/Running, subsequent send fails ENXIO and status lies. Any concurrent Stop/Send thread killed inside save_metadata's truncate-write window leaves corrupt metadata.json on disk, and other connected clients get their sockets dropped mid-response ('daemon closed connection without a response') with no way to know whether their command applied. Inverse ordering bug: if writing the shutdown response fails (client gone), the `?` at line 381 returns before exit(0), leaving a daemon that already killed every session but keeps running.

**Recommended fix.** Replace the bare std::process::exit(0) with an orderly shutdown sequence in the daemon: (a) on Shutdown, set a shared shutdown flag and stop accepting new connections (close/drop the listener or check the flag in the accept loop); (b) track in-flight request threads (e.g., Arc<(Mutex<usize>, Condvar)> incremented per connection) and wait for them to drain before stopping sessions; (c) in shutdown_sessions_for_daemon, include SessionStatus::Starting sessions — re-scan until no Running/Starting sessions remain, briefly waiting for Starting ones to publish child_pid and then terminating them; (d) write the response, then exit by returning from the accept loop normally (and if the response write fails, log and proceed with exit anyway rather than aborting via `?`, fixing the inversion at src/main.rs:381-388). Independently, make save_metadata atomic (write to metadata.json.tmp then fs::rename) so any abrupt termination cannot leave a truncated metadata.json.

### m14: AGENT_BRIDGE_HOME is trusted verbatim and the docs point it at world-writable /tmp, enabling symlink/pre-creation attacks

- **Severity:** medium
- **Anchor:** bridge_root src/main.rs:1222 (consumed by run_daemon src/main.rs:330 and initialize_session_files src/main.rs:637)
- **Found by:** security
- **Status:** open

**Problem.** bridge_root() returns AGENT_BRIDGE_HOME unchecked, and run_daemon/initialize_session_files then create_dir_all it and File::create / fs::write inside it, all following symlinks. The README and design doc explicitly instruct `AGENT_BRIDGE_HOME=/private/tmp/agent-bridge-test`. In sticky world-writable /tmp, another local user can pre-create that path (or interior files like sessions/<name>/raw.log) as a symlink before the owner runs; create_dir_all/File::create/fs::write then follow the link, letting the attacker (a) own the directory the victim writes secret logs into and read them, or (b) redirect a log/screen/metadata write through a symlink to clobber a victim-writable file. Even with no symlink, an attacker-pre-created dir is attacker-owned yet used by the victim daemon.

**Recommended fix.** Three-part fix. (1) Harden bridge_root() in src/main.rs:1222: after resolving an explicit AGENT_BRIDGE_HOME, `lstat` the path and bail if it is a symlink or exists but is not owned by the current uid (use std::os::unix MetadataExt::uid via symlink_metadata), and refuse a root whose parent is world-writable unless it is sticky-and-owned; reject relative paths. (2) Create the tree privately and without following symlinks: create the root and sessions/<name> dirs with mode 0700 (DirBuilder::mode(0o700)), and create raw.log/clean.log/screen.txt/metadata.json with OpenOptions().create_new(true).custom_flags(libc::O_NOFOLLOW).mode(0o600) (or chmod the parent 0700 and reject any pre-existing non-owned entry) so a planted symlink or foreign file is rejected rather than followed/truncated. (3) Change README.md:20/41 and docs/AGENT-BRIDGE.md:71 to a per-user location — on macOS `$TMPDIR` (verified here as /var/folders/.../T mode 0700, owned by the user) or `$HOME/.agent-bridge-test`, never the shared /private/tmp.

### m16: Stateless per-chunk ANSI stripping corrupts clean.log whenever an escape sequence splits across an 8KB read boundary

- **Severity:** medium
- **Anchor:** capture_output src/main.rs:794
- **Found by:** unix-pty, errors, spec, manual-read
- **Status:** open

**Problem.** capture_output calls strip_ansi_escapes::strip(chunk) per read; strip() constructs a fresh vte parser per call (verified in strip-ansi-escapes 0.2.1 source), so parser state is discarded between chunks. An escape sequence straddling the 8192-byte buffer boundary is half-dropped, half-emitted as literal text: a chunk ending '\x1b[38;5;2' is swallowed mid-parse and the next chunk's '40m' is written to clean.log as visible text. Claude Code and other TUI targets emit escape-dense multi-KB redraw bursts constantly, so chunk boundaries land inside sequences routinely, sprinkling fragments like '40m' and ';5;240m' through clean.log — the default source for `read` — misleading the manager agent that parses it and contradicting the doc's 'readable append-only text' claim.

**Recommended fix.** In capture_output (src/main.rs), replace the per-chunk strip() call with one persistent stripping writer created before the read loop, so the vte parser state (including partial escape sequences and partial UTF-8) carries across chunk boundaries: `let mut clean_writer = strip_ansi_escapes::Writer::new(clean_log);` then per chunk `clean_writer.write_all(chunk)?; clean_writer.flush()?;` (flush is needed each iteration because Writer wraps the sink in a LineWriter). Alternatively, derive clean.log content from the already-persistent vt100 parser, but the persistent Writer is the minimal change.

### m17: read loads the entire unbounded log into memory to tail a few lines

- **Severity:** medium
- **Anchor:** read_output_text src/main.rs:918-921
- **Found by:** errors, manual-read
- **Status:** open

**Problem.** read_output_text reads the whole raw.log/clean.log into a String, then materializes a Vec<&str> of every line, just to print the last `tail` (default 300) lines. These logs grow without bound for the life of a session, and Claude-style TUIs rewrite the screen continuously, so raw.log reaches hundreds of MB within hours and GBs over days. Every `read` then allocates the entire file (twice: string plus line vector) — and in daemon mode this happens inside the durable daemon process on every poll of the manager's loop, with concurrent reads multiplying the spike: multi-second latencies, memory bloat, and eventual allocation failure/OOM kill of the daemon, which takes down every running session's ownership thread.

**Recommended fix.** In read_output_text, replace fs::read_to_string with a bounded tail: open the file, fstat for length, seek backward from EOF reading fixed-size blocks (e.g. 64 KiB) and counting newlines until `tail` lines are found or a hard byte cap (e.g. a few MiB) is hit, then decode just that suffix with String::from_utf8_lossy (which also removes the UTF-8-strictness failure on raw.log) and return the last `tail` lines. read_screen_text can keep the simple path since screen.txt is rewritten per chunk and bounded by the vt100 grid size.

### m18: Daemon-started sessions use the daemon's PATH/env, while `doctor` always diagnoses the client's environment

- **Severity:** medium
- **Anchor:** supervise_pty src/main.rs:702 (doctor never daemon-routed: daemon_request_for_command src/main.rs:274)
- **Found by:** protocol, concurrency
- **Status:** open

**Problem.** supervise_pty resolves the program via path_with_local_bin(), which reads the PATH of the process it runs in — the daemon when sessions are daemon-owned — and CommandBuilder inherits the full daemon environment (src/main.rs:700-730). Meanwhile `doctor` is never routed through the daemon (daemon_request_for_command returns None, src/main.rs:274), so it inspects the CLIENT's PATH/env. Concrete scenario: the daemon was started from a plain Terminal session without nvm/homebrew init (or later via launchd); the user's iTerm shell has node and env vars (ANTHROPIC_*, custom PATH). `agent-bridge start dev --cmd "npm run dev"` fails with 'session stopped while starting: No such file or directory', or claude launches without the auth/env the user's shell has — yet `agent-bridge doctor` reports everything resolves fine, because it tested a different environment than the one that actually spawns the child.

**Recommended fix.** Add the client environment to the start request and make doctor daemon-aware. Minimal version: (a) extend DaemonRequest::Start with `path: Option<String>` (client fills it with std::env::var("PATH")); thread it into start_session_in_daemon → supervise_pty so resolve_program_path and `command.env("PATH", …)` use the client-supplied PATH (falling back to the process PATH in direct mode) — optionally extend to a full env-var map for auth vars; (b) in doctor, attempt UnixStream::connect(socket_path()) and, if reachable, print a prominent warning that a running daemon will spawn sessions with the daemon's environment, not the one being diagnosed (or add a DaemonRequest::Doctor so the daemon reports its own PATH/resolution alongside the client's).

### m19: Session exit is never detected when grandchildren hold the PTY slave: output-thread join hangs and metadata stays Running forever

- **Severity:** medium
- **Anchor:** supervise_pty src/main.rs:763
- **Found by:** unix-pty
- **Status:** open

**Problem.** After child.wait() returns, supervise_pty blocks on output_thread.join(), but the PTY master only sees EOF once ALL slave fds close. Run `start sh --cmd sh`, type `sleep 1000 &` then `exit`: the shell dies, sleep inherits the slave as stdin/stdout/stderr, capture_output blocks in read() and the join hangs indefinitely. mark_stopped never runs, so status reports status=Running (child_alive=false) forever — a manager agent polling for completion waits forever. The doc's advertised targets (shells, dev servers, test watchers) routinely background children, and daemonizing dev servers trigger this; in daemon mode the session thread is also permanently wedged.

**Recommended fix.** In supervise_pty, persist the terminal state as soon as the child is reaped instead of gating it on PTY EOF: immediately after child.wait() returns, call mark_stopped(name, Some(format!("{exit_status:?}"))) (or restructure so run_supervisor/the daemon closure do it before joining), then join the output thread with a bounded deadline (e.g., poll JoinHandle::is_finished for up to ~2s to drain final output) or leave it detached in daemon mode. This keeps metadata accurate even when grandchildren hold the slave open.

### m2: metadata.json written with truncate-then-write (fs::write), racing every concurrent reader and writer with no atomicity

- **Severity:** medium (finder claimed high; adjusted at verification)
- **Anchor:** save_metadata src/main.rs:1188
- **Found by:** concurrency, unix-pty, security, errors, manual-read
- **Status:** open

**Problem.** save_metadata uses fs::write — open(O_TRUNC) then write, two syscalls, no atomicity — while concurrent readers and writers exist by design with zero locks. (a) Every direct-mode start polls load_metadata every 100ms (wait_for_running_metadata, line 668) while the supervisor writes the file 2-3 times (lines 692, 743); a read landing in the truncate window gets empty/partial JSON and the `?` at line 668 aborts start with 'failed to parse metadata.json' even though the session started fine (the manager may then retry, feeding the duplicate-start race); the same transient failure hits send/status (lines 872, 961), and list/shutdown silently drop the session from results (lines 1002, 1045). (b) Every stop of a supervisor session has TWO concurrent mark_stopped writers — the stopper (line 1099, 'stopped by user') and the exiting supervisor (line 695, real exit status) — racing unsynchronized load-modify-write truncate+write pairs from independent fds: last writer wins, and interleaving can leave mixed-version, unparseable JSON persistently on disk. (c) stop SIGKILLs the supervisor (line 1090) possibly inside mark_stopped, and daemon Shutdown calls process::exit(0) while other threads may be mid-save_metadata; either can cut a write between truncate and write, leaving metadata.json empty/truncated forever — after which status/send/stop all fail for that session (and it vanishes from list) until it is re-started.

**Recommended fix.** In save_metadata, write atomically: serialize to a temp file in the same session directory (e.g. metadata.json.tmp.{pid}), then fs::rename() it over metadata.json. rename(2) is atomic on POSIX, so readers always see a complete old or new document, and a kill or process::exit mid-save leaves at worst a stray temp file instead of a truncated metadata.json. Optionally harden the consumers: let wait_for_running_metadata tolerate a transient load_metadata failure (retry instead of `?`), and make mark_stopped skip the write (or preserve the existing exit_status) when the loaded status is already Stopped, so the stopper does not overwrite the supervisor's real exit status.

### m20: Any capture-thread write error (disk full) silently kills output capture and freezes the session as a zombie 'Running'

- **Severity:** medium
- **Anchor:** capture_output src/main.rs:783-806
- **Found by:** errors
- **Status:** open

**Problem.** capture_output returns Err on the first failed raw/clean/screen write — e.g. the disk fills overnight — and the thread exits, but nothing observes it: supervise_pty discards the result with `let _ = output_thread.join()` (src/main.rs:763) and joins only after child.wait(). With no reader draining the PTY master, the child blocks forever on its next write to a full PTY buffer, so child.wait() never returns either. Observable result: status/list report Running and child_alive=true indefinitely, `screen` and `read` serve stale content, no error is logged anywhere, and the manager agent waits on a session that can never make progress until someone runs stop. A single transient screen.txt write failure also takes down raw/clean logging because all three share one loop.

**Recommended fix.** Two minimal changes in src/main.rs: (a) make capture_output resilient — on a raw/clean/screen write failure, record the error once (e.g., to metadata or an errors file) and keep reading the PTY (drop the data or retry per-sink) so the child can never block on an undrained master; handle the screen.txt fs::write failure independently so it cannot abort raw/clean logging. (b) Stop discarding the thread result: replace `let _ = output_thread.join()` with logic that captures the capture error and folds it into the exit_status passed to mark_stopped; alternatively, on unrecoverable capture failure have the supervisor kill the child so child.wait() returns and the session is marked Stopped with the capture error instead of staying Running forever.

### m5: stop blind-signals persisted PIDs with no identity or status check: after crash/reboot it SIGKILLs a reused PID and its whole process group, and EPERM wedges the session forever

- **Severity:** medium (finder claimed high; adjusted at verification)
- **Anchor:** stop_session_silent src/main.rs:1078-1099 (terminate_pid src/main.rs:1104, signal_pid_and_group src/main.rs:1137)
- **Found by:** unix-pty, security, errors, manual-read, spec
- **Status:** open

**Problem.** stop_session_silent passes child_pid/supervisor_pid from metadata.json to terminate_pid -> signal_pid_and_group, which sends SIGTERM then SIGKILL to both kill(pid) and kill(-pid) — the entire process group — with no identity check and no status gate. Stale Running metadata is routine: daemon SIGKILL (daemon-owned sessions are never recovered on restart), supervisor crash, or reboot all skip mark_stopped. After PID reuse, a later `agent-bridge stop name` SIGKILLs an unrelated process and, via the -pid group signal, potentially an entire unrelated process group. If the recycled pid belongs to another user/root, kill returns EPERM: stop_session_silent bails BEFORE mark_stopped (src/main.rs:1095-1099) and process_alive treats EPERM as alive (src/main.rs:1311), so the session is stuck Running permanently — stop always fails, start of the same name is refused 'already running', recoverable only by hand-editing the session directory. Additionally, mark_stopped retains the pids and stop never checks status==Stopped, so stopping an already-exited session re-signals possibly-recycled pids, can report 'failed to stop session' for a session that is in fact stopped, and overwrites the real recorded exit_status with 'stopped by user'.

**Recommended fix.** In stop_session_silent: return early (after ensuring mark_stopped) when metadata.status == Stopped. Record each process's start time at spawn (macOS: proc_pidinfo/PROC_PIDTBSDINFO pbi_start_tvsec via libproc; Linux: /proc/<pid>/stat field 22) in metadata, and before signaling verify the live process's start time matches; on mismatch or EPERM treat the target as already gone and still call mark_stopped instead of bailing. Clear child_pid/supervisor_pid in mark_stopped, and only send the group signal (-pid) when identity has been verified.

### m6: read and read --raw fail permanently once a session log contains non-UTF-8 bytes

- **Severity:** medium (finder claimed high; adjusted at verification)
- **Anchor:** read_output_text src/main.rs:918
- **Found by:** unix-pty, errors, protocol, spec, manual-read
- **Status:** open

**Problem.** read_output_text uses fs::read_to_string on raw.log (exact PTY bytes, arbitrary) and clean.log (strip-ansi passes non-escape bytes through verbatim, so it is poisoned too). The first non-UTF-8 output — cat of a binary, git diff of a binary file, curl of a tarball, Latin-1 from a legacy tool, or a multibyte codepoint truncated mid-write/at kill — permanently poisons both append-only logs (no rotation): every subsequent `agent-bridge read NAME` and `read --raw`, direct or daemon-routed, fails with 'failed to read ... stream did not contain valid UTF-8' for the rest of the session's life. The manager agent loses all log access to a still-running session (only `screen` keeps working), and raw.log's documented exact-byte replay purpose is unmeetable through the CLI.

**Recommended fix.** In read_output_text (src/main.rs:918-919), replace fs::read_to_string with let bytes = fs::read(&path)...; let contents = String::from_utf8_lossy(&bytes); then split into lines as before. This makes `read --raw` resilient to arbitrary PTY bytes (invalid sequences render as U+FFFD). Optionally apply the same pattern to read_screen_text for defense in depth; for true byte-exact replay, a future option is writing the tailed raw bytes to stdout without UTF-8 conversion.

### m7: Session names '.', '..', and leading '-' pass validation: '..' scribbles session files into the bridge root, and '-name' breaks the direct-mode supervisor re-spawn

- **Severity:** medium (finder claimed high; adjusted at verification)
- **Anchor:** validate_session_name src/main.rs:1199-1212 (session_dir src/main.rs:1303)
- **Found by:** security, protocol, spec, manual-read
- **Status:** open

**Problem.** validate_session_name accepts any mix of [A-Za-z0-9._-] (src/main.rs:1199-1212), so '.', '..', and leading-dash names all pass. session_dir('..') = ~/.agent-bridge itself and session_dir('.') = the sessions root (src/main.rs:1303-1306). Verified: `agent-bridge start . --cmd "echo dot"` created raw.log, clean.log, screen.txt, metadata.json, and input.fifo directly in $AGENT_BRIDGE_HOME/sessions/; '..' targets the bridge root next to the live daemon socket, where initialize_session_files deletes/recreates input.fifo and truncates root-level files, and stop/status/read operate on bridge-root state — breaking the per-session sandbox and the documented sessions/{name} layout (such sessions are also invisible to `list`, which only enumerates subdirectories). It cannot reach arbitrary paths ('/' is blocked), but it clobbers root-level state. Separately, a name like '-foo' works in daemon mode but breaks direct mode: start_session_detached re-spawns `current_exe supervisor -foo ...` (src/main.rs:550-557) and clap in the supervisor rejects '-foo' as an unknown flag, so the supervisor exits instantly with stderr nulled, the client times out 'did not report running within 5 seconds', and metadata is stuck at Starting — the same name behaves differently depending on whether the daemon is up.

**Recommended fix.** In validate_session_name (src/main.rs:1199), additionally reject name == "." , name == "..", and names starting with '-' (ideally any leading '.'). For defense in depth in start_session_detached (src/main.rs:550-557), move the name to the end of the supervisor argv behind a '--' separator — i.e. `supervisor --cwd <cwd> --cmd <cmd> -- <name>` — note that inserting '--' immediately before the name in its current first position would break parsing of the following --cwd/--cmd options, so it must go last. Optionally, after building session_dir, assert dir.parent() == Some(sessions_root) (or compare canonicalized paths) so any future validation gap cannot escape the per-session layout.

### m8: screen.txt is rewritten with non-atomic fs::write on every PTY chunk, so concurrent screen reads see empty or truncated screens

- **Severity:** medium
- **Anchor:** capture_output src/main.rs:801
- **Found by:** concurrency, unix-pty, spec, manual-read, errors, security
- **Status:** open

**Problem.** capture_output calls fs::write(screen.txt) after every <=8KB read chunk (line 801) — open(O_TRUNC) then write, potentially dozens of times per second during active output — while read_screen_text (line 941) in another process does an uncoordinated read_to_string. A read landing between truncate and write successfully returns an empty or partial screen. This is the highest-frequency non-atomic-write race in the codebase and hits the primary documented use case directly: a manager AI polling `screen` during Claude streaming to decide whether the session is idle or awaiting approval intermittently observes a blank/truncated snapshot and acts on it (concludes the session is hung or dead, re-sends a prompt). The full-screen serialize+write per chunk is also wasted work under heavy output.

**Recommended fix.** In capture_output, write the snapshot to a temp file in the same session directory (e.g. screen.txt.tmp) and fs::rename it over screen.txt; rename(2) atomically replaces the destination on the same filesystem, so readers always see a complete old or new snapshot. Optionally coalesce rewrites (e.g. at most once per 50-100 ms, or only when the PTY read would block) to cut write amplification. The same tmp+rename pattern should ideally be applied to save_metadata (line 1188), which has the identical non-atomic pattern at lower frequency.

### m9: FIFO input writes are not atomic: O_NONBLOCK write_all delivers a partial prefix then errors on large sends, and concurrent senders can interleave mid-message

- **Severity:** medium
- **Anchor:** write_session_bytes src/main.rs:884
- **Found by:** concurrency, unix-pty, errors, protocol, spec, manual-read
- **Status:** open

**Problem.** write_session_bytes opens input.fifo O_WRONLY|O_NONBLOCK (line 880) and calls write_all (line 884); std's write_all does not retry WouldBlock. The FIFO holds ~16-64KB and forward_input drains it at the child's pace through the small PTY input queue, so a large send (multi-KB pasted diff/prompt — the tool's core Phase-2 use case) to a busy or non-reading child (REPL computing, dev server that never reads stdin, child suspended via `keys ctrl-z`) fills the buffer and write returns EAGAIN after partial progress: the CLI errors 'failed to write input' (with no byte count) AFTER an arbitrary prefix was already delivered, the child executes truncated input without the trailing \r, and the natural manager retry delivers the prefix twice. Pipe writes are atomic only up to PIPE_BUF (512 bytes on macOS), so two concurrent senders (two daemon threads, or daemon thread + --direct CLI) with larger payloads can also interleave bytes mid-message, splicing two prompts together.

**Recommended fix.** In write_session_bytes, keep O_NONBLOCK only for the open (no-reader probe), then clear it with fcntl(fd, F_SETFL, flags & ~O_NONBLOCK) before writing — or loop on ErrorKind::WouldBlock with a short sleep and an overall deadline instead of plain write_all. To preserve atomicity against concurrent senders, either chunk writes to <= PIPE_BUF (512 bytes) or serialize sends per session (per-session mutex in the daemon plus an flock on the session dir for --direct). On any failure, report how many bytes were already delivered.

### c4: doctor ships hardcoded author-machine heuristics that emit false warnings on every other install layout

- **Severity:** low
- **Anchor:** doctor src/main.rs:469-477
- **Found by:** critic
- **Status:** open

**Problem.** doctor's warning logic encodes one specific machine's history: it warns whenever the resolved claude path is not ~/.local/bin/claude ("known-good local Claude path was ~/.local/bin/claude in this environment; resolved path differs", src/main.rs:475-477) and flags /opt/homebrew/bin/claude based on a past local incident (src/main.rs:469-474). On any standard installation where claude legitimately lives elsewhere (npm -g at /usr/local/bin/claude, a Homebrew install, or the planned Linux target where /opt/homebrew never exists but ~/.local/bin may not be used), a perfectly working setup is reported with warnings every single run. Since doctor's whole job is to be the trustworthy diagnostic -- and its consumer is often the manager AI deciding whether the environment is healthy -- persistent spurious warnings cause wrong conclusions and wasted debugging, and train users to ignore the warnings section entirely. Note doctor also already runs `claude --version` through the resolved path (src/main.rs:484-491), which provides the actual health signal these heuristics try to guess at.

**Recommended fix.** Delete the two hardcoded path heuristics at src/main.rs:469-477 and derive warnings from data doctor already collects: warn if the `<resolved_program> --version` probe fails or emits nothing, and warn if the login-shell `command -v claude` output (already captured at 495-500, just needs parsing instead of only printing) resolves to a different path than resolved_program. If the historical /opt/homebrew incident is worth preserving, print it as an informational note only when that exact path resolved AND the version probe failed.

### m15: Daemon has no read timeout or request-size limit and spawns an unbounded thread per connection (local DoS)

- **Severity:** low (finder claimed medium; adjusted at verification)
- **Anchor:** handle_daemon_stream src/main.rs:371 (accept/spawn loop run_daemon src/main.rs:350)
- **Found by:** security, manual-read
- **Status:** open

**Problem.** run_daemon accepts each connection and thread::spawn's a handler with no cap; handle_daemon_stream does a single read_line with no timeout and no length bound. A local process that can reach the socket can (a) open many connections and never send a newline — each handler blocks forever in read_line, permanently pinning one thread per connection until the daemon exhausts threads/FDs/memory, or (b) send one enormous line with no \n, making read_line buffer it unbounded into memory (OOM). Either takes down the daemon and thus the control plane for all of the user's sessions. The socket's 0600 perms limit the attacker to the same user today, but the protocol is otherwise unauthenticated, so any same-user process (e.g. a compromised dependency) suffices.

**Recommended fix.** In handle_daemon_stream, set stream.set_read_timeout(Some(Duration::from_secs(10))) and set_write_timeout(...) on the accepted UnixStream before reading, and bound the request line by wrapping the cloned stream in Read::take(1 << 20) before BufReader::read_line, bailing if the cap is reached without a trailing newline. Optionally cap concurrent handler threads (simple counting semaphore), though with timeouts in place blocked handlers self-expire.

### m21: Daemon startup socket takeover is a check-connect-remove-bind TOCTOU that can unlink a live daemon's socket

- **Severity:** low
- **Anchor:** run_daemon src/main.rs:333
- **Found by:** concurrency, manual-read
- **Status:** open

**Problem.** run_daemon does exists -> connect-probe -> remove_file -> bind as four separate steps (lines 333-342). Two `agent-bridge daemon` invocations racing after a crash (stale socket present) both fail the connect probe, both decide the socket is stale; D1 removes and binds (now live), then D2 — which already passed its probe — removes D1's freshly bound socket file and binds its own. D1 keeps running and 'listening' on an unlinked inode forever: a silent zombie daemon process that no client can ever reach, while clients talk to D2. The probe can also hit D1 between bind and listen and read ECONNREFUSED, with the same takeover result. The daemon additionally never removes its socket on shutdown (exit(0) at line 388), guaranteeing every restart exercises this stale-socket path.

**Recommended fix.** In run_daemon, before the probe/remove/bind sequence, open ~/.agent-bridge/daemon.lock and take an exclusive non-blocking flock (flock(fd, LOCK_EX|LOCK_NB), available on macOS and Linux via nix or libc); on EWOULDBLOCK bail with "daemon already running or starting". Keep the lock fd alive (e.g. leak it or store it in a struct living for the accept loop) so the lock is held for the daemon's lifetime — flock releases automatically on process death, covering crashes. Additionally, unlink the socket path before std::process::exit(0) in handle_daemon_stream's shutdown branch so clean restarts no longer depend on the stale-socket cleanup path.

### m22: Daemon-mode error between spawn and wait leaks an unreaped child that becomes a permanent zombie and wedges stop/shutdown

- **Severity:** low
- **Anchor:** supervise_pty src/main.rs:738
- **Found by:** unix-pty
- **Status:** open

**Problem.** In supervise_pty, any error after spawn_command but before child.wait() — e.g. load_metadata at line 738 hitting a torn metadata write from the polling client, or try_clone_reader/take_writer failing — returns early without ever waiting on the child. In direct mode the supervisor exits and init reaps; in the daemon the child stays a child of the daemon process forever. When it eventually exits it becomes a permanent zombie: kill(pid, 0) succeeds on zombies so process_alive=true, SIGTERM/SIGKILL are accepted but do nothing, and terminate_pid bails 'process did not exit after SIGTERM and SIGKILL' — `stop` fails forever for that session and `shutdown` (which bails if any Running session fails to stop) refuses to exit the daemon.

**Recommended fix.** In supervise_pty, guarantee reaping on every post-spawn path: wrap the fallible section between spawn_command and child.wait() (load/save metadata, try_clone_reader, take_writer) so that on any Err it runs `let _ = child.kill(); let _ = child.wait();` before propagating (e.g. an inner closure/function whose Err arm reaps, or a small drop-guard holding the child). Additionally move the metadata load/update before spawn_command to shrink the window, and optionally have stop_session_silent skip terminate_pid for sessions already marked Stopped to avoid the 4s failing stop on stale pids.

### m24: 5-second start timeout reports failure but leaves the detached supervisor running, contradicting itself

- **Severity:** low
- **Anchor:** wait_for_running_metadata src/main.rs:683
- **Found by:** errors
- **Status:** open

**Problem.** wait_for_running_metadata polls 50x100ms and bails with 'did not report running within 5 seconds' — but by then the setsid'd supervisor (or daemon thread) has already been spawned and is not cleaned up on this path. Under load or slow disk/binary resolution the session then transitions to Running moments later: the user was told start failed, yet `list` shows it Running and a retry of the same start either errors 'already running' or, if it lands while status is still Starting, re-initializes the files under the live supervisor (feeding the duplicate-spawn race). The timeout expiring mid-operation leaves orphaned in-flight state instead of converging to either success or a cleaned-up failure.

**Recommended fix.** On the timeout path, converge instead of contradicting: in wait_for_running_metadata's final branch (src/main.rs:683), load the metadata and either (a) terminate the in-flight start — call terminate_pid on supervisor_pid/child_pid (best-effort) and mark_stopped(name, Some("start timed out")) so the failure leaves a clean Stopped state — or (b) return a non-failure message like "session 'NAME' is still starting (supervisor pid N); check 'agent-bridge status NAME'". Additionally, extend the already-running guard at lines 538-544 (and the daemon variant at 595-603) to also bail when status == Starting with a live supervisor/child, closing the retry-clobber window.

### m25: Daemon responses flatten error chains, hiding root causes and conflating missing session with corrupt state

- **Severity:** low
- **Anchor:** handle_daemon_request src/main.rs:431
- **Found by:** errors
- **Status:** open

**Problem.** handle_daemon_request serializes failures with error.to_string(), which for anyhow errors yields only the outermost context: a status/read of a nonexistent session returns just 'failed to read /Users/x/.agent-bridge/sessions/NAME/metadata.json' with the 'No such file or directory' cause stripped, indistinguishable from a permission problem or torn write; a send to a dead session returns 'failed to open input FIFO for NAME' without the ENXIO that explains the reader is gone. Since most commands auto-route through the daemon, this is the error UX the manager agent normally sees, and nothing anywhere says plainly 'no such session NAME'.

**Recommended fix.** In handle_daemon_request (src/main.rs line 431), serialize the full chain: error: Some(format!("{error:#}")). Additionally, in load_metadata (line 1176), map a missing metadata.json to an explicit error, e.g. match the io::ErrorKind::NotFound case (or check session_dir existence) and bail!("no such session '{name}'") so a typoed name is stated plainly in both modes.

### m26: Malformed or unknown daemon requests get no response, so clients see only 'daemon closed connection without a response'

- **Severity:** low
- **Anchor:** handle_daemon_stream src/main.rs:377
- **Found by:** protocol
- **Status:** open

**Problem.** handle_daemon_stream bails on read/parse failure BEFORE writing any DaemonResponse (src/main.rs:370-378); the error goes to the daemon's stderr and the stream drops, so the client's read_line gets EOF and reports the generic 'daemon closed connection without a response' (src/main.rs:301-302). Concrete scenario (version skew): the user rebuilds/upgrades agent-bridge while an old daemon is still running; the new CLI sends a request variant or tag the old daemon's serde enum rejects, and every command fails with that unexplained message until the user guesses to restart the daemon. The same applies to the planned MCP adapter or any hand-written client sending slightly wrong JSON — the protocol gives them zero diagnostic. (The daemon's own startup liveness probe also triggers a spurious 'empty daemon request' stderr line via this path.)

**Recommended fix.** In handle_daemon_stream, on empty-line or parse failure, write a DaemonResponse { ok: false, output: String::new(), error: Some(format!("invalid daemon request: {err}")) } plus "\n" to the stream (and flush) before returning, so clients receive the actual parse error instead of EOF. Optionally treat a zero-byte read (pure EOF, e.g. the startup liveness probe) as a silent no-op instead of an error to eliminate the spurious "empty daemon request" stderr line.

### m27: `send` text beginning with '-' is rejected by clap instead of being sent to the session

- **Severity:** low
- **Anchor:** Commands::Send src/main.rs:65
- **Found by:** protocol
- **Status:** open

**Problem.** The Send `text` positional has no allow_hyphen_values (src/main.rs:62-70), so clap treats leading-dash text as an option. Concrete scenario: the manager agent drives a Python REPL and runs `agent-bridge send py "-1 + 2"`, or sends `--help`/`-v` to a CLI under test; clap exits code 2 with 'unexpected argument' and nothing reaches the session. The `--` escape works but an AI client has no reason to know it's required, and for a tool whose whole job is forwarding arbitrary text this is a recurring trap.

**Recommended fix.** In /Users/chadpeppers/Projects/agent-bridge/src/main.rs add `#[arg(allow_hyphen_values = true)]` above the `text: String` field of `Commands::Send` (line 65-66). Optionally mention in the field's doc comment that literal `--help`/`--no-enter` text still needs the `--` escape (e.g. `agent-bridge send NAME -- --help`).

### m28: Doc example `agent-bridge send claude "..." --enter` errors: the flag is --no-enter

- **Severity:** low
- **Anchor:** Commands::Send src/main.rs:69 (doc: docs/AGENT-BRIDGE.md:375)
- **Found by:** spec
- **Status:** open

**Problem.** docs/AGENT-BRIDGE.md line 375 ('Command Surface — Initial service-era CLI') shows `agent-bridge send claude "Review the failing tests." --enter`. Verified: this exits 2 with "unexpected argument '--enter' found". The CLI only defines --no-enter (Enter is appended by default), so the one documented send example in the command-surface block fails if pasted, while every other command in that block parses as written.

**Recommended fix.** In docs/AGENT-BRIDGE.md line 375, drop `--enter` so the example reads `agent-bridge send claude "Review the failing tests."` (Enter is appended by default), and optionally add a second example showing `--no-enter` for suppressing it.

### m29: README quickstart never exercises the daemon it starts: AGENT_BRIDGE_HOME on the daemon line only

- **Severity:** low
- **Anchor:** bridge_root src/main.rs:1222 (doc: README.md:20-29)
- **Found by:** spec
- **Status:** open

**Problem.** README.md line 20 starts the daemon under AGENT_BRIDGE_HOME=/private/tmp/agent-bridge-test, but the following client commands (start/send/read/.../shutdown, lines 21-29) use the default home. Since the socket path derives from bridge_root, those clients look for ~/.agent-bridge/agent-bridge.sock, never find the test daemon, and silently run in direct mode under ~/.agent-bridge; the final `shutdown` prints 'daemon not running' while the test daemon keeps running. The block therefore contradicts the adjacent claim 'When a daemon is reachable, normal session commands route through its Unix socket' — a user following it verbatim tests the supervisor path while believing they tested the daemon.

**Recommended fix.** In README.md, use one consistent home across the whole command block: either drop AGENT_BRIDGE_HOME from line 20, or add a preceding `export AGENT_BRIDGE_HOME=/private/tmp/agent-bridge-test` so the daemon and all client commands (start/send/read/.../shutdown) share the same socket and session root. Also note that `agent-bridge daemon` runs in the foreground, so it needs a separate terminal or `&` for the subsequent commands to be runnable.

### m30: start reports failure for fast-exiting commands even when they succeed (exit code 0)

- **Severity:** low
- **Anchor:** wait_for_running_metadata src/main.rs:672
- **Found by:** spec
- **Status:** open

**Problem.** wait_for_running_metadata first sleeps 100ms; any command that finishes before the first poll is seen as Stopped and start bails. Verified: `agent-bridge start hi --cmd "echo hello"` exits 1 with "session 'hi' stopped while starting: ExitStatus { code: 0, signal: None }" even though clean.log captured 'hello'. The verified-so-far claim '`claude --version` works under the PTY' (Phase-0 acceptance) only holds because node startup exceeds 100ms — success of the documented acceptance test is timing-dependent, and any sub-100ms command yields a misleading error despite correct capture.

**Recommended fix.** Record the exit result in structured form and treat a clean fast exit as success. Concretely: in supervise_pty, capture portable_pty::ExitStatus before stringifying (it has exit_code()/success()) and pass it to mark_stopped so SessionMetadata gains e.g. exit_code: Option<u32> alongside the display string; then in wait_for_running_metadata (src/main.rs:672), when status == Stopped and exit_code == Some(0), return Ok(metadata) (callers can print "session ran to completion" instead of "started"), bailing only on nonzero/signal exits. Optionally also poll immediately and at a finer interval (e.g. 10-20ms) to narrow the window, but the exit-code check is the actual fix.

### m31: Restarting a session name silently erases its previous raw/clean logs and screen

- **Severity:** low
- **Anchor:** initialize_session_files src/main.rs:647
- **Found by:** spec
- **Status:** open

**Problem.** initialize_session_files uses File::create, which truncates raw.log, clean.log, and screen.txt whenever start reuses an existing name. A user who stops 'claude-main' on Friday and starts it again Monday loses the entire prior transcript, although the docs present raw.log as 'exact PTY bytes for replay/debugging' and Phase-3 acceptance expects restarts not to corrupt existing logs; neither README nor the design doc mentions truncation anywhere. Undocumented destruction of the artifact the docs tell users to rely on for replay.

**Recommended fix.** In initialize_session_files, before the File::create calls, rename any existing non-empty raw.log/clean.log to timestamped siblings (e.g. raw.log.<unix_ts>) or open them with OpenOptions::append, and document the chosen behavior in README/design doc. Additionally, move parse_command validation ahead of the truncation so a start that fails to parse or spawn does not destroy prior logs.

### m32: send immediately after start can hit the FIFO before any reader exists (ENXIO window): Running is saved before forward_input opens the FIFO

- **Severity:** low
- **Anchor:** supervise_pty src/main.rs:738-758 (write_session_bytes src/main.rs:877-882)
- **Found by:** manual-read, concurrency, unix-pty
- **Status:** open

**Problem.** supervise_pty saves status=Running (src/main.rs:743) BEFORE the input-forwarder thread is spawned and opens the FIFO (src/main.rs:758/810). A send issued immediately after start observes status==Running, opens input.fifo O_WRONLY|O_NONBLOCK, and fails with ENXIO ('failed to open input FIFO') because no reader has the FIFO open yet. The same confusing ENXIO also appears whenever the owner died leaving stale Running metadata, making it ambiguous whether the session is starting or dead.

**Recommended fix.** In supervise_pty, open the input FIFO O_RDWR before save_metadata(Running) (move the open out of forward_input: open the File at ~line 738, then pass it to the forwarder thread), so a reader provably exists whenever Running is visible. Optionally also retry ENXIO in write_session_bytes a few times with short sleeps and emit a clearer error ("no process is reading the session's input — session may still be starting or its owner died").

## Refuted findings

### m23: Daemon socket is briefly group/world-connectable: chmod 0600 happens only after bind

- **Anchor:** run_daemon src/main.rs:343
- **Claim.** UnixListener::bind creates the socket with 0777 & ~umask (typically 0755) inside the 0755 ~/.agent-bridge directory; fs::set_permissions(0600) runs afterwards, leaving a window in which another local user can connect (macOS does enforce write permission on the socket inode at connect time, so after chmod it's safe — but not before). A connection in that window can issue a Start request, i.e. execute an arbitrary command as the daemon user. The window is milliseconds and only at daemon startup, hence low, but it is free to eliminate.

**Why refuted.** The claim is internally inconsistent and fails against real macOS semantics. Code path: src/main.rs:341-342 `UnixListener::bind(&path)` then src/main.rs:343-344 `fs::set_permissions(0o600)`. Grep confirms the binary never calls umask, so the socket inherits the process umask (default 022 on macOS).

XNU `unp_bind` creates the socket node with `va_mode = ACCESSPERMS & ~fd_cmask` = `0777 & ~umask`, so under the default umask 022 the socket is born 0755 — exactly the "typical 0755" the claim cites. That mode is owner=rwx, group=r-x, other=r-x: the WRITE bit is set only for the owner.

The claim's own load-bearing premise is correct: macOS enforces write permission at connect. XNU `unp_connect` performs `vnode_authorize(vp, NULL, KAUTH_VNODE_WRITE_DATA, ctx)` (matching FreeBSD's historical `VOP_ACCESS(vp, VWRITE)` and Linux unix(7), which also require write to connect). Given a 0755 socket, that check grants connect only to the owner; any other local user's `UnixStream::connect` during the window between line 342 and line 343 is denied EACCES and never reaches `handle_daemon_stream`/the `Start` request.

The two premises the claim states (mode 0755 + write-enforced-at-connect) directly forbid its conclusion (a non-owner can connect). Moreover, for the purpose of connect permission the chmod is a no-op under the default umask: 0755 and 0600 both grant write to the owner only, so the connectable principal set is {owner} both before and after line 343 — there is no instant of broader access, i.e. no exploitable window. The chmod only strips group/other read+search bits, which are irrelevant to connect authorization.

A real window would require a non-default permissive umask leaving the other-write bit set (umask 000 or 020) or the group-write bit set (umask 002) plus an attacker sharing the owner's supplementary group. That contradicts the claim's stated "typical 0755" case and is not the macOS default, so the everyday failure scenario described does not occur.

The single fact this hinges on is whether macOS checks read vs write at connect: it checks write (KAUTH_VNODE_WRITE_DATA), which the claim itself concedes — and that fact is precisely what makes 0755 non-connectable by non-owners.

## Resolution log

Filled in as fixes land; each confirmed finding gets a resolution commit or an explicit wontfix rationale.

| ID | Resolution | Commit |
|----|------------|--------|
| m1 | — | — |
| m3 | — | — |
| m4 | — | — |
| c1 | — | — |
| c2 | — | — |
| c3 | — | — |
| m10 | — | — |
| m11 | — | — |
| m12 | — | — |
| m13 | — | — |
| m14 | — | — |
| m16 | — | — |
| m17 | — | — |
| m18 | — | — |
| m19 | — | — |
| m2 | — | — |
| m20 | — | — |
| m5 | — | — |
| m6 | — | — |
| m7 | — | — |
| m8 | — | — |
| m9 | — | — |
| c4 | — | — |
| m15 | — | — |
| m21 | — | — |
| m22 | — | — |
| m24 | — | — |
| m25 | — | — |
| m26 | — | — |
| m27 | — | — |
| m28 | — | — |
| m29 | — | — |
| m30 | — | — |
| m31 | — | — |
| m32 | — | — |
