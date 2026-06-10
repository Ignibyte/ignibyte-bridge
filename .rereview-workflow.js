export const meta = {
  name: 'rereview-agent-bridge-fixes',
  description: 'Adversarial re-review of the agent-bridge fix diff: regressions, incomplete fixes, new bugs, fix interactions',
  phases: [
    { title: 'Find', detail: '6 lenses over the baseline..HEAD diff and current source' },
    { title: 'Merge', detail: 'dedupe findings' },
    { title: 'Verify', detail: 'adversarial refuter per finding; tiebreak when uncertain' },
  ],
}

const ROOT = '/Users/chadpeppers/Projects/agent-bridge'
const BASE = 'd230009' // pristine Codex baseline

const CONTEXT = [
  'PROJECT: agent-bridge (' + ROOT + ') — a Rust CLI that owns persistent PTY sessions so a manager agent can drive interactive terminal programs (Claude Code, REPLs, dev servers). macOS now, Linux later.',
  '',
  'WHAT JUST HAPPENED: an AI-written prototype (single src/main.rs at baseline commit ' + BASE + ') was reviewed, then restructured into a lib (src/lib.rs + modules: session, daemon, logs, clean, paths, protocol, keys, procinfo, doctor) and 33 confirmed findings were fixed across 13 commits, then tests were added. YOUR JOB IS TO RE-REVIEW THAT WORK — specifically the NEW code the fixes introduced. Fixes are where regressions and new bugs hide.',
  '',
  'SEE THE CHANGES: run `git -C ' + ROOT + ' diff ' + BASE + ' -- src/` for the full source diff (baseline monolith -> current modular code), and `git -C ' + ROOT + ' log --oneline ' + BASE + '..HEAD` for the commit-by-commit story. Read the CURRENT source files directly too (they are the source of truth); the diff shows what moved vs what is genuinely new.',
  '',
  'THE 35 FINDINGS AND THEIR FIXES (verify each fix is COMPLETE and CORRECT, not partial/bypassable, and did not break a working path):',
  '- m1 perms: dirs 0700, files 0600 via paths::ensure_private_dir / create_private_file (O_NOFOLLOW).',
  '- m2/m8 atomic writes: paths::write_atomic (temp file + rename) for metadata and screen.txt.',
  '- m3/m18 client env: main resolves --cwd absolute (paths::resolve_cwd) and forwards PATH before routing; daemon uses them (paths::child_path); supervise_pty takes client_path.',
  '- m4/m19 pty lifecycle: logs::forward_input polls an AtomicBool stop flag (O_NONBLOCK FIFO); supervise_pty joins it on child exit and caps the output drain at 2s.',
  '- m5/c2/m10 liveness: procinfo::process_start_time tokens; session::pid_is_ours / session_is_active gate start/stop/status/list on PID + start-time match.',
  '- m6/m17 read: logs::tail_file backward-seeks in 64KiB blocks (8MiB cap) with from_utf8_lossy.',
  '- m7 names: validate_session_name rejects dot, dot-dot, leading-dash, and over-128-char names; supervisor argv passes the name behind a -- separator.',
  '- m9/m32 input: write_session_bytes clears O_NONBLOCK before writing and maps ENXIO; supervise_pty opens the FIFO reader before publishing Running.',
  '- m11/m12/m24 start: per-session flock (acquire_start_lock); supervise_pty bails+reaps if marked Stopped during startup; start-timeout terminates the in-flight supervisor/child and marks Stopped.',
  '- m13/m15/m21/m22/m25/m26/c3 daemon: acquire_daemon_lock (flock); handle_daemon_stream sets 30s timeouts, caps the request line at 1MiB via Read::take, replies to malformed requests, no-ops a 0-byte read, removes the socket on shutdown; shutdown covers Starting sessions; supervise_pty reaps the child on any post-spawn error; errors serialized with the alternate Display; client sets a 60s socket timeout (protocol.rs).',
  '- m16/c1 clean log: clean::AnsiCleaner is a stateful stripper carrying parser state across chunks, preserving tabs and normalizing CR to LF (replaces strip-ansi-escapes).',
  '- m20 resilience: capture_output keeps draining the PTY on a sink write error (warn-once) instead of aborting.',
  '- m27/m30/m31/c4: send text allow_hyphen_values; numeric exit_code in metadata so a clean fast exit reports success; restart rotates prior logs to a .prev file (command parsed before truncation); doctor derives warnings from observed behavior.',
  '',
  'WHAT IS NOT A FINDING: design choices that are intentional and correct — advisory flock (fine for cooperating processes); AnsiCleaner deliberately starting sequences only on ESC 0x1b and NOT on C1 bytes 0x9b/0x9d (those are UTF-8 continuation bytes — treating them as control would corrupt UTF-8); pid_is_ours falling back to bare liveness when no start-time token was recorded (legacy metadata only); roadmap gaps the design doc lists as future (MCP, resize, idle detection, log rotation, daemon-restart session recovery). Style/naming preferences are not findings. A finding needs a concrete failure scenario with observable consequence.',
].join('\n')

const SEVERITY_GUIDE = [
  'Severity: critical = security boundary broken for other local users / data loss / core command unusable in normal use; high = real wrong behavior in everyday use or secrets exposed; medium = failure under plausible-but-less-common conditions (races, large inputs, binary output, crashes leaving stale state); low = resource hygiene, hardening, or misleading-error UX with an easy workaround.',
  'A REGRESSION (the fix broke something that worked at baseline, or a fix is incomplete/bypassable) is at least the severity of the behavior it broke.',
].join('\n')

const FINDINGS_SCHEMA = {
  type: 'object', required: ['findings'],
  properties: { findings: { type: 'array', items: {
    type: 'object', required: ['title', 'detail', 'anchor', 'severity', 'kind', 'fix'],
    properties: {
      title: { type: 'string' },
      detail: { type: 'string' },
      anchor: { type: 'string' },
      severity: { enum: ['critical', 'high', 'medium', 'low'] },
      kind: { enum: ['regression', 'incomplete-fix', 'new-bug', 'fix-interaction', 'portability'] },
      fix: { type: 'string' },
    } } } },
}

const MERGE_SCHEMA = {
  type: 'object', required: ['findings'],
  properties: { findings: { type: 'array', items: {
    type: 'object', required: ['id', 'title', 'detail', 'anchor', 'severity', 'kind', 'sources'],
    properties: {
      id: { type: 'string' }, title: { type: 'string' }, detail: { type: 'string' }, anchor: { type: 'string' },
      severity: { enum: ['critical', 'high', 'medium', 'low'] },
      kind: { type: 'string' },
      sources: { type: 'array', items: { type: 'string' } },
      fix: { type: 'string' },
    } } } },
}

const VERDICT_SCHEMA = {
  type: 'object', required: ['verdict', 'reasoning'],
  properties: {
    verdict: { enum: ['confirmed', 'refuted', 'uncertain'] },
    reasoning: { type: 'string' },
    severity: { enum: ['critical', 'high', 'medium', 'low'] },
    fix: { type: 'string' },
  },
}

const LENSES = [
  { key: 'regression', title: 'Regressions and incomplete or bypassable fixes',
    charge: 'For each of the 35 findings, confirm the fix is COMPLETE and CORRECT and did not break a path that worked at baseline. Hunt for: fixes that only cover one of two modes (direct vs daemon); fixes guarded by a condition that an attacker or edge input can dodge; the m12 direct-mode variant (a stop landing after run_supervisor records supervisor_pid but before Running — does terminating the supervisor orphan the already-spawned PTY child, which was created inside the supervisor process?); whether validate_session_name is actually called on every path that builds session_dir; whether write_session_bytes still gates on Running after the m9/m32 changes. Compare current behavior against the baseline behavior the design doc and README claim.' },
  { key: 'concurrency', title: 'Concurrency in the new code',
    charge: 'Scrutinize the newly-added synchronization: acquire_start_lock and acquire_daemon_lock (flock LOCK_EX|LOCK_NB on a create_private_file that TRUNCATES the lock file each call — is truncation-under-lock safe? is the lock fd kept alive for the right scope? is there a window where the lock releases too early?); write_atomic temp-file naming (TMP_COUNTER process-global AtomicU64 plus pid — collision across threads or processes? leftover temp files on crash?); the m24 start-timeout path running terminate_pid plus mark_stopped from the client or handler thread WHILE the owner thread is still inside supervise_pty (double mark_stopped, double reap, killing a pid the owner still waits on); stop_session_silent vs the owner-thread mark_stopped now that writes are atomic; supervise_pty stop-flag plus input_thread.join ordering vs drop(master).' },
  { key: 'helpers', title: 'New pure helpers — edge cases',
    charge: 'Adversarially test the new self-contained functions by reasoning through inputs. logs::tail_file: off-by-one in the newlines-greater-than-tail break; behavior when the last line lacks a trailing newline; when tail exceeds the line count; when a block boundary splits a multibyte UTF-8 char (is lossy decode applied to the whole collected buffer or per block?); the 8MiB cap starting mid-line. clean::AnsiCleaner: DCS/SOS/PM/APC string termination (the StringEsc state going to Ground on ANY byte after ESC rather than only on backslash — does it drop a byte?); OSC terminated by BEL vs ST; a pending_cr left dangling at the end of a final chunk (lost newline?); CSI with intermediate bytes; an ESC at the very end of a chunk. procinfo: macOS token packing injectivity; the Linux /proc/<pid>/stat field-22 parse — it splits after the last close-paren, so reason about whether a process whose comm contains a close-paren shifts the field index. paths::write_atomic and create_private_file mode-repair logic.' },
  { key: 'portability', title: 'Cross-platform and Linux correctness (cannot be tested on this macOS host)',
    charge: 'The code targets Linux too but is only tested on macOS. Reason carefully about the Linux paths: the procinfo Linux cfg block parsing /proc/<pid>/stat (is starttime obtained from the correct field after the last close-paren, and is it a stable per-process identity?); flock availability and semantics on Linux vs macOS; O_NOFOLLOW behavior; libc::proc_pidinfo and PROC_PIDTBSDINFO being macOS-only (are they correctly cfg-gated so Linux compiles?); any std API or libc constant used unconditionally that differs across platforms; the not(macos/linux) fallback returning None for start_time and what that does to liveness on such a platform.' },
  { key: 'interactions', title: 'Interactions between fixes',
    charge: 'Look for fixes that undermine each other, and go beyond these examples: does opening the FIFO reader before publishing Running (m32) interact badly with the m22 post-spawn reap guard or the m12 Stopped-during-startup bail (is the FIFO fd or child leaked on those early returns)?; does the m20 keep-draining-on-write-error behavior defeat the m19 bounded-drain or mask a full disk indefinitely?; does write_atomic (m2/m8) plus the .prev rotation (m31) plus create_private_file race on the same session dir during a restart?; does the daemon request-size cap or 30s timeout (m15) break a legitimately large or slow Start/Send (a multi-hundred-KB send, or a start that legitimately takes more than 30s)?; does exit_code plumbing (m30) misreport a signal-killed process?; does child_path (m18) double-prepend the local bin dir or mishandle an empty client PATH?' },
  { key: 'security', title: 'Security of the new code',
    charge: 'Re-examine the hardening for residual gaps. ensure_bridge_dir walks components from the root calling ensure_private_dir on each — is there a TOCTOU between the lstat check and use? is O_NOFOLLOW applied to the leaf file but intermediate dirs still followed via create_dir? do write_atomic temp files and rotated .prev logs get owner-only perms, or could they momentarily be world-readable or inherit a bad mode? does the mode-repair in create_private_file/ensure_private_dir have a window where the file exists with loose perms before the chmod? can a crafted but validation-passing session name still escape the session dir? is AGENT_BRIDGE_HOME validation (absolute, non-symlink, owner) actually enforced on the daemon path and the supervisor re-exec path, not just the first CLI call? are secrets still written to .prev logs that outlive the session?' },
]

function finderPrompt(lens) {
  return [
    CONTEXT, '',
    'YOUR LENS: ' + lens.title, lens.charge, '',
    'Method: run `git -C ' + ROOT + ' diff ' + BASE + ' -- src/` and read the current files under ' + ROOT + '/src/ in full. Trace cross-process and cross-thread flows (client CLI -> daemon thread OR detached supervisor -> capture/forward threads -> filesystem). Build/run nothing; reason against real code lines and real POSIX/macOS/Linux + Rust-std + crate semantics (portable-pty 0.8, vte/serde versions are in Cargo.lock).',
    '',
    SEVERITY_GUIDE, '',
    'Report at most 10 findings — only real ones with a concrete failure scenario. For each: a one-line title; detail with the exact scenario (who does what, what goes wrong, observable consequence) and, for regressions, how baseline behaved vs now; anchor as function + file:LINE in the CURRENT tree; severity; kind (regression | incomplete-fix | new-bug | fix-interaction | portability); a one-sentence fix sketch. If the new code is genuinely sound through your lens, return few or zero findings — do not invent issues.',
  ].join('\n')
}

function mergePrompt(all) {
  return [
    CONTEXT, '',
    'You are the dedup/merge step. Below are raw findings from 6 lenses re-reviewing the fix diff. Merge TRUE duplicates (same root cause at the same site) into one, union their sources, keep the clearest detail, the most specific anchor, the highest severity, and the most accurate kind. Never drop a non-duplicate. When unsure whether two share a root cause, keep both. Assign ids r1..rN ordered by severity (critical first).',
    '', 'RAW FINDINGS (JSON):', JSON.stringify(all, null, 1),
  ].join('\n')
}

function verifyPrompt(f) {
  return [
    CONTEXT, '',
    'You are a skeptical adversarial verifier. ONE claimed issue with the fixes is below. Default stance: the claim is WRONG until the code proves it. Try hard to REFUTE.',
    'Method: read the anchored code in the CURRENT ' + ROOT + '/src tree AND every function on the scenario path, including the other side of any cross-thread/cross-process interaction; consult `git -C ' + ROOT + ' diff ' + BASE + ' -- <file>` to see exactly what changed if useful. Check the precise semantics the claim depends on (flock, O_NONBLOCK/ENXIO, rename atomicity, proc_pidinfo, /proc parsing, portable-pty Child::wait/kill, serde, Rust std). For a REGRESSION claim, verify the baseline actually behaved better. You MAY consult man pages / docs.rs / std docs on the web for a load-bearing fact, and MAY write a tiny throwaway probe under /tmp (never touch the project tree, ~/.agent-bridge, or run the project binary). Build/run the project: no.',
    'Refutation must name the exact mechanism preventing the failure. Confirmation must walk the failure scenario step by step against real current code lines. verdict = confirmed | refuted | uncertain (uncertain only if you can name the single missing fact). If confirmed, give final severity and a minimal concrete fix.',
    '', SEVERITY_GUIDE, '',
    'CLAIMED FINDING (JSON):', JSON.stringify(f, null, 1),
  ].join('\n')
}

function tiebreakPrompt(f, v) {
  return [
    verifyPrompt(f), '',
    'A FIRST VERIFIER WAS UNCERTAIN. Their reasoning:', v.reasoning, '',
    'Settle it definitively. If pure reasoning cannot resolve a crate/syscall/platform fact, you MAY write a tiny scratch probe under /tmp (a small rustc program or a shell test of flock/FIFO/rename/proc semantics) — never touch the project tree or ~/.agent-bridge, never run the project binary. Return confirmed or refuted; uncertain only as a last resort.',
  ].join('\n')
}

phase('Find')
const finders = await parallel(LENSES.map(l => () =>
  agent(finderPrompt(l), { label: 'find:' + l.key, phase: 'Find', schema: FINDINGS_SCHEMA })))

const raw = []
finders.forEach((r, i) => {
  if (r && Array.isArray(r.findings)) r.findings.forEach(f => raw.push({ ...f, source: LENSES[i].key }))
})
log(raw.length + ' raw findings from ' + LENSES.length + ' lenses')
if (raw.length === 0) {
  return { confirmed: [], refuted: [], uncertain: [], note: 'no findings raised' }
}

phase('Merge')
const merged = await agent(mergePrompt(raw), { label: 'merge', phase: 'Merge', schema: MERGE_SCHEMA })
if (!merged || !Array.isArray(merged.findings) || merged.findings.length === 0) {
  throw new Error('merge step returned nothing')
}
log(merged.findings.length + ' findings after dedup')

phase('Verify')
const verified = await pipeline(
  merged.findings,
  f => agent(verifyPrompt(f), { label: 'verify:' + f.id, phase: 'Verify', schema: VERDICT_SCHEMA }),
  (v, f) => {
    if (!v) return { finding: f, verdict: null }
    if (v.verdict !== 'uncertain') return { finding: f, verdict: v }
    return agent(tiebreakPrompt(f, v), { label: 'tiebreak:' + f.id, phase: 'Verify', schema: VERDICT_SCHEMA })
      .then(v2 => ({ finding: f, verdict: v2 || v }))
  },
)

const results = verified.filter(Boolean)
const confirmed = results.filter(r => r.verdict && r.verdict.verdict === 'confirmed')
const refuted = results.filter(r => r.verdict && r.verdict.verdict === 'refuted')
const uncertain = results.filter(r => !r.verdict || r.verdict.verdict === 'uncertain')
log('verdicts: ' + confirmed.length + ' confirmed, ' + refuted.length + ' refuted, ' + uncertain.length + ' uncertain')

return { confirmed, refuted, uncertain }
