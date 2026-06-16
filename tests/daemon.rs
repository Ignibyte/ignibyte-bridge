//! Daemon-mode end-to-end tests.
//!
//! Each test owns a daemon child (DaemonGuard, killed on drop) scoped to an
//! isolated bridge home. Client commands auto-route through the socket.

mod common;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use common::{wait_until, DaemonGuard, SessionGuard, TestHome};
use predicates::prelude::*;

fn read_contains(home: &TestHome, name: &str, needle: &str) -> bool {
    wait_until(Duration::from_secs(5), || {
        let output = home.cmd().args(["read", name, "--tail", "200"]).output();
        matches!(output, Ok(out) if String::from_utf8_lossy(&out.stdout).contains(needle))
    })
}

#[test]
fn socket_is_private_and_second_daemon_refuses() {
    let home = TestHome::new();
    let _daemon = DaemonGuard::start(&home);

    let mode = std::fs::metadata(home.socket()).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(mode.mode() & 0o777, 0o600, "socket must be owner-only");

    // A second daemon against the same home must refuse (lock + live socket).
    home.cmd()
        .args(["daemon"])
        .timeout(Duration::from_secs(5))
        .assert()
        .failure();
}

#[test]
fn full_lifecycle_through_daemon() {
    let home = TestHome::new();
    let daemon = DaemonGuard::start(&home);
    let _guard = SessionGuard::new(&home, "d1");

    // A daemon-owned session reports its supervisor as "daemon".
    home.cmd()
        .args(["start", "d1", "--cmd", "cat"])
        .assert()
        .success()
        .stdout(predicate::str::contains("supervisor pid: daemon"));

    home.cmd()
        .args(["send", "d1", "via-daemon"])
        .assert()
        .success();
    assert!(read_contains(&home, "d1", "via-daemon"));

    home.cmd().args(["screen", "d1"]).assert().success();
    home.cmd().args(["status", "d1"]).assert().success();
    home.cmd()
        .args(["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("d1"));

    home.cmd().args(["stop", "d1"]).assert().success();

    // Shutdown stops the daemon and removes the socket.
    home.cmd().args(["shutdown"]).assert().success();
    assert!(wait_until(Duration::from_secs(5), || !home
        .socket()
        .exists()));
    drop(daemon);
}

#[test]
fn shutdown_stops_running_sessions() {
    let home = TestHome::new();
    let _daemon = DaemonGuard::start(&home);
    let _guard = SessionGuard::new(&home, "s1");

    home.cmd()
        .args(["start", "s1", "--cmd", "cat"])
        .assert()
        .success();
    let child_pid = pid_field(&home, "s1", "child_pid").expect("child pid");
    assert!(process_alive(child_pid));

    home.cmd().args(["shutdown"]).assert().success();
    assert!(
        wait_until(Duration::from_secs(6), || !process_alive(child_pid)),
        "shutdown should kill the running child"
    );
}

#[test]
fn client_falls_back_to_direct_without_daemon() {
    let home = TestHome::new();
    let _guard = SessionGuard::new(&home, "fb");

    // No daemon: a normal (non --direct) start runs a real supervisor process.
    home.cmd()
        .args(["start", "fb", "--cmd", "cat"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("supervisor pid:")
                .and(predicate::str::contains("supervisor pid: daemon").not()),
        );
}

#[test]
fn daemon_session_adopts_client_cwd() {
    let home = TestHome::new();
    let _daemon = DaemonGuard::start(&home);
    let _guard = SessionGuard::new(&home, "cw");

    let client_dir = tempfile::tempdir().unwrap();
    let canonical = client_dir.path().canonicalize().unwrap();

    // Run the client FROM client_dir with no --cwd; the session must adopt the
    // client's directory, not the daemon's.
    home.cmd()
        .current_dir(client_dir.path())
        .args(["start", "cw", "--cmd", "cat"])
        .assert()
        .success();

    let status = home.cmd().args(["status", "cw"]).output().unwrap();
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(
        text.contains(&format!("cwd: {}", canonical.display())),
        "status was: {text}"
    );
}

#[test]
fn malformed_request_gets_error_response_and_daemon_survives() {
    let home = TestHome::new();
    let _daemon = DaemonGuard::start(&home);

    // Send a garbage line directly to the socket.
    let mut stream = UnixStream::connect(home.socket()).unwrap();
    stream.write_all(b"{\"command\":\"bogus\"}\n").unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(
        response.contains("invalid daemon request"),
        "got: {response}"
    );

    // The daemon still serves the next request.
    home.cmd()
        .args(["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no sessions"));
}

#[test]
fn direct_flag_bypasses_running_daemon() {
    let home = TestHome::new();
    let _daemon = DaemonGuard::start(&home);
    let _guard = SessionGuard::new(&home, "byp");

    // --direct spawns a supervisor process even though a daemon is reachable.
    home.cmd()
        .args(["--direct", "start", "byp", "--cmd", "cat"])
        .assert()
        .success()
        .stdout(predicate::str::contains("supervisor pid: daemon").not());
}

// --- small process helpers (avoid depending on the lib's internals) ---

fn process_alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 only probes for existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn pid_field(home: &TestHome, name: &str, key: &str) -> Option<i32> {
    let text = std::fs::read_to_string(home.session_dir(name).join("metadata.json")).ok()?;
    let needle = format!("\"{key}\":");
    let start = text.find(&needle)? + needle.len();
    let rest = text[start..].trim_start();
    let end = rest.find([',', '\n', '}']).unwrap_or(rest.len());
    rest[..end].trim().parse().ok()
}
