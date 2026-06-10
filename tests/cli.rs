//! Direct-mode (no daemon) end-to-end CLI tests.
//!
//! These spawn real PTYs via the binary. Each test uses an isolated bridge
//! home and a SessionGuard so a failure cannot leak a detached supervisor.

mod common;

use std::time::Duration;

use common::{wait_until, SessionGuard, TestHome};
use predicates::prelude::*;

/// Poll a session's clean log until it contains `needle`.
fn read_contains(home: &TestHome, name: &str, needle: &str) -> bool {
    wait_until(Duration::from_secs(5), || {
        let output = home.direct().args(["read", name, "--tail", "200"]).output();
        matches!(output, Ok(out) if String::from_utf8_lossy(&out.stdout).contains(needle))
    })
}

#[test]
fn start_send_read_stop_lifecycle() {
    let home = TestHome::new();
    let _guard = SessionGuard::new(&home, "life");

    home.direct()
        .args(["start", "life", "--cmd", "cat"])
        .assert()
        .success()
        .stdout(predicate::str::contains("started session 'life'"));

    assert_eq!(home.status_field("life"), "Running");

    home.direct()
        .args(["send", "life", "marker-alpha"])
        .assert()
        .success();
    assert!(read_contains(&home, "life", "marker-alpha"));

    home.direct().args(["stop", "life"]).assert().success();
    assert!(wait_until(Duration::from_secs(5), || home.status_field("life")
        == "Stopped"));
}

#[test]
fn no_enter_then_keys_enter_submits() {
    let home = TestHome::new();
    let _guard = SessionGuard::new(&home, "ne");

    home.direct()
        .args(["start", "ne", "--cmd", "cat"])
        .assert()
        .success();

    // Without a trailing newline cat echoes nothing yet (canonical mode).
    home.direct()
        .args(["send", "ne", "held-line", "--no-enter"])
        .assert()
        .success();
    home.direct().args(["keys", "ne", "enter"]).assert().success();
    assert!(read_contains(&home, "ne", "held-line"));
}

#[test]
fn ctrl_d_ends_session() {
    let home = TestHome::new();
    let _guard = SessionGuard::new(&home, "eof");

    home.direct()
        .args(["start", "eof", "--cmd", "cat"])
        .assert()
        .success();
    home.direct().args(["keys", "eof", "ctrl-d"]).assert().success();
    assert!(
        wait_until(Duration::from_secs(5), || home.status_field("eof") == "Stopped"),
        "ctrl-d should end cat"
    );
}

#[test]
fn cwd_is_honored() {
    let home = TestHome::new();
    let workdir = tempfile::tempdir().unwrap();
    // Canonicalize so the /private symlink on macOS matches what the session
    // records.
    let canonical = workdir.path().canonicalize().unwrap();
    let _guard = SessionGuard::new(&home, "cwd");

    home.direct()
        .args(["start", "cwd", "--cmd", "cat", "--cwd"])
        .arg(workdir.path())
        .assert()
        .success();

    let status = home.status("cwd");
    assert!(
        status.contains(&format!("cwd: {}", canonical.display())),
        "status was: {status}"
    );
}

#[test]
fn read_tail_returns_exact_line_count() {
    let home = TestHome::new();
    let _guard = SessionGuard::new(&home, "tail");

    home.direct()
        .args(["start", "tail", "--cmd", "sh -c 'seq 1 50; cat'"])
        .assert()
        .success();
    assert!(read_contains(&home, "tail", "50"));

    let output = home
        .direct()
        .args(["read", "tail", "--tail", "3"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(text.lines().count(), 3, "got: {text:?}");
    assert!(text.contains("50"));
}

#[test]
fn read_raw_survives_non_utf8_output() {
    let home = TestHome::new();
    let _guard = SessionGuard::new(&home, "bin");

    home.direct()
        .args(["start", "bin", "--cmd", r#"sh -c 'printf "a\377b\n"; cat'"#])
        .assert()
        .success();

    // Give the output time to land, then a raw read must still succeed.
    assert!(wait_until(Duration::from_secs(5), || {
        home.direct()
            .args(["read", "bin", "--raw"])
            .output()
            .map(|o| o.status.success() && !o.stdout.is_empty())
            .unwrap_or(false)
    }));
}

#[test]
fn large_send_is_delivered_fully() {
    let home = TestHome::new();
    let _guard = SessionGuard::new(&home, "big");

    home.direct()
        .args(["start", "big", "--cmd", "cat"])
        .assert()
        .success();

    // 4000 newline-terminated lines (~32KB) exceed a single pipe write.
    let payload = (0..4000)
        .map(|i| format!("line{i:04}"))
        .collect::<Vec<_>>()
        .join("\n");
    home.direct().args(["send", "big", &payload]).assert().success();

    assert!(read_contains(&home, "big", "line3999"));
}

#[test]
fn restart_bumps_generation_and_preserves_prior_logs() {
    let home = TestHome::new();
    let _guard = SessionGuard::new(&home, "re");

    let gen = |home: &TestHome| -> u64 {
        let text =
            std::fs::read_to_string(home.session_dir("re").join("metadata.json")).unwrap();
        let needle = "\"generation\":";
        let start = text.find(needle).unwrap() + needle.len();
        let rest = text[start..].trim_start();
        let end = rest.find([',', '\n', '}']).unwrap_or(rest.len());
        rest[..end].trim().parse().unwrap()
    };

    home.direct().args(["start", "re", "--cmd", "cat"]).assert().success();
    home.direct().args(["send", "re", "first-run"]).assert().success();
    assert!(read_contains(&home, "re", "first-run"));
    assert_eq!(gen(&home), 1);
    home.direct().args(["stop", "re"]).assert().success();
    assert!(wait_until(Duration::from_secs(5), || home.status_field("re") == "Stopped"));

    // Restart the same name: generation bumps, prior logs are kept as .prev, and
    // the new run works.
    home.direct().args(["start", "re", "--cmd", "cat"]).assert().success();
    assert_eq!(gen(&home), 2);
    assert!(home.session_dir("re").join("raw.log.prev").exists());
    home.direct().args(["send", "re", "second-run"]).assert().success();
    assert!(read_contains(&home, "re", "second-run"));
}

#[test]
fn duplicate_start_is_rejected() {
    let home = TestHome::new();
    let _guard = SessionGuard::new(&home, "dupe");

    home.direct()
        .args(["start", "dupe", "--cmd", "cat"])
        .assert()
        .success();
    home.direct()
        .args(["start", "dupe", "--cmd", "cat"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already"));
}

#[test]
fn traversal_session_names_are_rejected() {
    let home = TestHome::new();
    for name in ["..", "."] {
        home.direct()
            .args(["start", name, "--cmd", "cat"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("session name"));
    }
}

#[test]
fn nonexistent_command_fails_and_records_stopped() {
    let home = TestHome::new();
    let _guard = SessionGuard::new(&home, "nope");

    home.direct()
        .args(["start", "nope", "--cmd", "this-command-does-not-exist-xyz"])
        .assert()
        .failure();
    // Metadata should converge to Stopped, not stay Starting.
    assert!(wait_until(Duration::from_secs(6), || {
        let status = home.status_field("nope");
        status == "Stopped" || status.is_empty()
    }));
}

#[test]
fn fast_clean_exit_reports_success() {
    let home = TestHome::new();
    let _guard = SessionGuard::new(&home, "echo");

    home.direct()
        .args(["start", "echo", "--cmd", "echo hello-there"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ran to completion"));
}

#[test]
fn list_reports_empty_then_sorted_sessions() {
    let home = TestHome::new();
    home.direct()
        .args(["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no sessions"));

    let _a = SessionGuard::new(&home, "aaa");
    let _b = SessionGuard::new(&home, "bbb");
    home.direct().args(["start", "bbb", "--cmd", "cat"]).assert().success();
    home.direct().args(["start", "aaa", "--cmd", "cat"]).assert().success();

    let output = home.direct().args(["list"]).output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    let a = text.find("aaa").expect("aaa listed");
    let b = text.find("bbb").expect("bbb listed");
    assert!(a < b, "sessions should be sorted by name: {text}");
}

#[test]
fn status_reports_activity_and_idle_grows() {
    let home = TestHome::new();
    let _guard = SessionGuard::new(&home, "act");

    home.direct().args(["start", "act", "--cmd", "cat"]).assert().success();
    home.direct().args(["send", "act", "ping"]).assert().success();
    assert!(read_contains(&home, "act", "ping"));

    // Right after output, status reports activity fields and low idle.
    let status = home.status("act");
    assert!(status.contains("output_bytes:"), "status: {status}");
    assert!(status.contains("last_output_unix:"), "status: {status}");

    let idle_of = |home: &TestHome| -> u64 {
        home.status("act")
            .lines()
            .find_map(|l| l.strip_prefix("idle_seconds: "))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    let before = idle_of(&home);
    // Idle should grow once the session goes quiet.
    assert!(wait_until(Duration::from_secs(5), || idle_of(&home) > before));

    // list carries an idle column too.
    home.direct()
        .args(["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("idle="));
}

#[test]
fn unknown_session_gives_clear_error() {
    let home = TestHome::new();
    home.direct()
        .args(["status", "ghost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no such session 'ghost'"));
}

#[test]
fn doctor_reports_resolution_without_spurious_warnings() {
    let home = TestHome::new();
    // A plainly-resolvable non-claude command: doctor should resolve it, find
    // it executable, and emit no warnings (the author-machine heuristics are
    // gone).
    home.direct()
        .args(["doctor", "--cmd", "sh"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("executable_found: true")
                .and(predicate::str::contains("warnings: none")),
        );
}

#[test]
fn doctor_warns_when_command_missing() {
    let home = TestHome::new();
    home.direct()
        .args(["doctor", "--cmd", "definitely-not-a-real-binary-xyz"])
        .assert()
        .success()
        .stdout(predicate::str::contains("executable_found: false"));
}

#[test]
fn screen_shows_rendered_marker() {
    let home = TestHome::new();
    let _guard = SessionGuard::new(&home, "scr");

    home.direct()
        .args(["start", "scr", "--cmd", "sh -c 'printf SCREEN-MARKER; cat'"])
        .assert()
        .success();

    assert!(wait_until(Duration::from_secs(5), || {
        home.direct()
            .args(["screen", "scr"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("SCREEN-MARKER"))
            .unwrap_or(false)
    }));
}
