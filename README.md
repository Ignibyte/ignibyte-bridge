# Agent Bridge

Local persistent PTY session controller for AI coding agents.

This is an early prototype. The long-term target is a daemon-backed bridge that
lets Codex/Rusty manage persistent terminal programs such as Claude Code,
Codex CLI, dev servers, test watchers, shells, and REPLs.

See the working design doc:

```text
docs/AGENT-BRIDGE.md
```

## Current Commands

```bash
cargo build
./target/debug/agent-bridge doctor --cmd claude --cwd /Users/chadpeppers/Projects/rusty
AGENT_BRIDGE_HOME=/private/tmp/agent-bridge-test ./target/debug/agent-bridge daemon
./target/debug/agent-bridge start py --cwd /Users/chadpeppers/Projects/rusty --cmd "python3 -i"
./target/debug/agent-bridge send py "print(2 + 3)"
./target/debug/agent-bridge read py --tail 50
./target/debug/agent-bridge screen py --tail 50
./target/debug/agent-bridge keys py ctrl-c
./target/debug/agent-bridge status py
./target/debug/agent-bridge list
./target/debug/agent-bridge stop py
./target/debug/agent-bridge shutdown
```

Session files are stored under:

```text
~/.agent-bridge/sessions/{session-name}/
```

For tests, override storage without changing the child process home directory:

```bash
AGENT_BRIDGE_HOME=/private/tmp/agent-bridge-test ./target/debug/agent-bridge list
```

When a daemon is reachable, normal session commands route through its Unix
socket. Use `--direct` to bypass the daemon during debugging:

```bash
./target/debug/agent-bridge --direct list
```
