//! Shared integration-test harness.
//!
//! Every test runs against its own `AGENT_BRIDGE_HOME` temp dir, set per
//! `Command` (never `std::env::set_var`, which is process-global and racy under
//! the parallel test runner). Guards make a best effort to tear down detached
//! `setsid` supervisors so a failing test cannot leak `cat`/`sleep` processes
//! onto the developer's machine.

#![allow(dead_code)]

use std::{
    path::{Path, PathBuf},
    process::{Child, Command as StdCommand},
    time::{Duration, Instant},
};

use assert_cmd::cargo::cargo_bin;
use assert_cmd::Command;
use tempfile::TempDir;

/// Poll `condition` until it returns true or the deadline elapses. Returns
/// whether the condition was met. Use instead of a bare sleep so tests are not
/// timing-fragile.
pub fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// An isolated bridge home plus helpers to build pre-configured commands.
pub struct TestHome {
    dir: TempDir,
}

impl TestHome {
    pub fn new() -> Self {
        TestHome {
            dir: TempDir::new().expect("create temp AGENT_BRIDGE_HOME"),
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn socket(&self) -> PathBuf {
        self.dir.path().join("agent-bridge.sock")
    }

    pub fn session_dir(&self, name: &str) -> PathBuf {
        self.dir.path().join("sessions").join(name)
    }

    /// An `assert_cmd` Command for the binary, scoped to this home. `--direct`
    /// is not added, so commands route through a daemon if one is running.
    pub fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("agent-bridge").expect("locate agent-bridge binary");
        cmd.env("AGENT_BRIDGE_HOME", self.dir.path());
        cmd
    }

    /// Like [`TestHome::cmd`] but forces direct (no-daemon) execution.
    pub fn direct(&self) -> Command {
        let mut cmd = self.cmd();
        cmd.arg("--direct");
        cmd
    }

    /// Read a session's current status text (empty string if the command fails).
    pub fn status(&self, name: &str) -> String {
        let output = self.direct().args(["status", name]).output();
        match output {
            Ok(output) => String::from_utf8_lossy(&output.stdout).to_string(),
            Err(_) => String::new(),
        }
    }

    /// The `status:` field value for a session, or empty if unavailable.
    pub fn status_field(&self, name: &str) -> String {
        self.status(name)
            .lines()
            .find_map(|line| line.strip_prefix("status: "))
            .unwrap_or("")
            .trim()
            .to_string()
    }
}

impl Default for TestHome {
    fn default() -> Self {
        Self::new()
    }
}

/// Guard that best-effort stops a session (and SIGKILLs its recorded PIDs) when
/// dropped, so a panicking test cannot leak a detached supervisor.
pub struct SessionGuard<'a> {
    home: &'a TestHome,
    name: String,
}

impl<'a> SessionGuard<'a> {
    pub fn new(home: &'a TestHome, name: &str) -> Self {
        SessionGuard {
            home,
            name: name.to_string(),
        }
    }
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        let _ = self
            .home
            .direct()
            .args(["stop", &self.name])
            .timeout(Duration::from_secs(10))
            .output();
        // Belt and suspenders: kill any PIDs still recorded in metadata.
        let meta_path = self.home.session_dir(&self.name).join("metadata.json");
        if let Ok(text) = std::fs::read_to_string(meta_path) {
            for key in ["child_pid", "supervisor_pid"] {
                if let Some(pid) = extract_pid(&text, key) {
                    // SAFETY: kill with signal 9 has no memory effects.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }
    }
}

/// A daemon child process scoped to a [`TestHome`], killed on drop.
pub struct DaemonGuard {
    child: Child,
}

impl DaemonGuard {
    /// Spawn `agent-bridge daemon` against `home` and wait for its socket.
    pub fn start(home: &TestHome) -> Self {
        let log = std::fs::File::create(home.path().join("daemon.out")).expect("daemon log");
        let err = log.try_clone().expect("clone daemon log");
        let child = StdCommand::new(cargo_bin("agent-bridge"))
            .arg("daemon")
            .env("AGENT_BRIDGE_HOME", home.path())
            .stdin(std::process::Stdio::null())
            .stdout(log)
            .stderr(err)
            .spawn()
            .expect("spawn daemon");

        let socket = home.socket();
        assert!(
            wait_until(Duration::from_secs(10), || std::os::unix::net::UnixStream::connect(
                &socket
            )
            .is_ok()),
            "daemon socket did not appear"
        );

        DaemonGuard { child }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn extract_pid(json: &str, key: &str) -> Option<i32> {
    let needle = format!("\"{key}\":");
    let start = json.find(&needle)? + needle.len();
    let rest = json[start..].trim_start();
    let end = rest.find([',', '\n', '}']).unwrap_or(rest.len());
    rest[..end].trim().parse().ok()
}
