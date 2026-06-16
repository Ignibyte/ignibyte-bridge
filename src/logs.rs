//! PTY output capture, input forwarding, and log/snapshot text helpers.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    os::unix::io::AsRawFd,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result};

use crate::{clean::AnsiCleaner, paths::write_atomic, CLEAN_LOG, RAW_LOG, SCREEN_SNAPSHOT};

/// Capture PTY output into the raw/clean logs and the screen snapshot until the
/// reader reaches EOF or `stop` is set. `reader` owns a dup of the PTY master
/// fd; a `poll` with a short timeout lets the loop observe `stop` even when a
/// backgrounded grandchild holds the slave open and no EOF ever arrives, so the
/// owning thread can always be joined instead of leaking.
pub fn capture_output(
    reader: &mut File,
    session_dir: &Path,
    stop: &Arc<AtomicBool>,
    rows: u16,
    cols: u16,
) -> Result<()> {
    let mut raw_log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(session_dir.join(RAW_LOG))
        .context("failed to open raw log")?;
    let mut clean_log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(session_dir.join(CLEAN_LOG))
        .context("failed to open clean log")?;
    let mut terminal = vt100::Parser::new(rows, cols, 2_000);
    let mut cleaner = AnsiCleaner::default();
    let mut clean = Vec::new();
    let mut buffer = [0_u8; 8192];
    // Whether each sink has already reported a failure, so a persistent error
    // (e.g. a full disk) is logged once rather than every chunk.
    let mut warned = [false; 3];

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Wait for data with a timeout so the stop flag is observed even if the
        // reader never reaches EOF.
        let mut pfd = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pfd points to one valid pollfd; poll only reads/writes it.
        let rc = unsafe { libc::poll(&mut pfd, 1, 200) };
        if rc < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("failed to poll PTY");
        }
        if rc == 0 {
            continue; // timeout: re-check stop
        }

        // A read error (not EOF) is fatal: without a draining reader the child
        // would block on the PTY forever, so propagate and let the session stop.
        let read = reader.read(&mut buffer).context("failed to read PTY")?;
        if read == 0 {
            return Ok(());
        }

        let chunk = &buffer[..read];

        // Each sink is best-effort: a write failure must NOT abort the loop, or
        // the undrained PTY would wedge the child and freeze the session as a
        // zombie that still reports Running. Keep draining; report each sink's
        // first failure once.
        let raw_result = raw_log.write_all(chunk).and_then(|()| raw_log.flush());
        warn_once(&mut warned[0], "raw log", raw_result);

        clean.clear();
        cleaner.clean(chunk, &mut clean);
        let clean_result = clean_log.write_all(&clean).and_then(|()| clean_log.flush());
        warn_once(&mut warned[1], "clean log", clean_result);

        terminal.process(chunk);
        let screen_result = write_atomic(
            &session_dir.join(SCREEN_SNAPSHOT),
            trim_screen_snapshot(&terminal.screen().contents()).as_bytes(),
        );
        warn_once(&mut warned[2], "screen snapshot", screen_result);
    }
}

fn warn_once<E: std::fmt::Display>(warned: &mut bool, sink: &str, result: Result<(), E>) {
    if let Err(error) = result {
        if !*warned {
            *warned = true;
            eprintln!("ignibyte-bridge: {sink} write failed (continuing): {error}");
        }
    }
}

/// Forward bytes from the session FIFO to the PTY until `stop` is set.
///
/// `fifo` must already be open read+write with `O_NONBLOCK` (the caller opens
/// it before publishing the session as running so a racing `send` always finds
/// a reader). The read+write handle never sees EOF when senders disconnect, and
/// `O_NONBLOCK` lets the loop wake periodically to observe `stop` — so the owner
/// can join this thread when the child exits instead of leaking it (and the
/// dup'd PTY master fd it holds).
pub fn forward_input(
    mut fifo: File,
    writer: &mut Box<dyn Write + Send>,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let mut buffer = [0_u8; 8192];

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        match fifo.read(&mut buffer) {
            Ok(0) => thread::sleep(Duration::from_millis(20)),
            Ok(read) => {
                writer
                    .write_all(&buffer[..read])
                    .context("failed to write input to PTY")?;
                writer.flush().context("failed to flush PTY input")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error).context("failed to read input FIFO"),
        }
    }
}

/// Return the last `tail` lines of a file without reading the whole thing into
/// memory. Reads fixed-size blocks backward from EOF until enough newlines are
/// seen or a hard byte cap is hit, then decodes lossily — so an unbounded log
/// stays cheap to tail and arbitrary (non-UTF-8) PTY bytes never break `read`.
pub fn tail_file(path: &Path, tail: usize) -> Result<String> {
    const BLOCK: usize = 64 * 1024;
    const MAX_BYTES: u64 = 8 * 1024 * 1024;

    let mut file =
        std::fs::File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let len = file
        .metadata()
        .with_context(|| format!("failed to inspect {}", path.display()))?
        .len();
    if len == 0 || tail == 0 {
        return Ok(String::new());
    }

    let floor = len.saturating_sub(MAX_BYTES);
    let mut pos = len;
    let mut collected: Vec<u8> = Vec::new();
    let mut newlines = 0usize;

    while pos > floor {
        let read_size = BLOCK.min((pos - floor) as usize);
        pos -= read_size as u64;
        file.seek(SeekFrom::Start(pos))
            .with_context(|| format!("failed to seek {}", path.display()))?;
        let mut block = vec![0_u8; read_size];
        file.read_exact(&mut block)
            .with_context(|| format!("failed to read {}", path.display()))?;
        newlines += block.iter().filter(|&&byte| byte == b'\n').count();
        block.extend_from_slice(&collected);
        collected = block;
        // One extra newline beyond `tail` guarantees the last `tail` lines are
        // whole; tail_lines then slices exactly the requested count.
        if newlines > tail {
            break;
        }
    }

    Ok(tail_lines(&String::from_utf8_lossy(&collected), tail))
}

pub fn tail_lines(contents: &str, tail: usize) -> String {
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(tail);
    let mut output = String::new();
    for line in &lines[start..] {
        output.push_str(line);
        output.push('\n');
    }

    output
}

pub fn trim_screen_snapshot(contents: &str) -> String {
    let mut lines = contents.lines().map(str::trim_end).collect::<Vec<_>>();

    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }

    if lines.is_empty() {
        String::new()
    } else {
        let mut snapshot = lines.join("\n");
        snapshot.push('\n');
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn tail_lines_returns_last_n() {
        let text = "a\nb\nc\nd\ne\n";
        assert_eq!(tail_lines(text, 2), "d\ne\n");
        assert_eq!(tail_lines(text, 0), "");
    }

    #[test]
    fn tail_lines_caps_at_available() {
        assert_eq!(tail_lines("a\nb\n", 10), "a\nb\n");
        assert_eq!(tail_lines("", 5), "");
    }

    #[test]
    fn tail_lines_handles_missing_trailing_newline() {
        assert_eq!(tail_lines("a\nb\nc", 2), "b\nc\n");
    }

    #[test]
    fn trim_snapshot_drops_trailing_blanks_and_spaces() {
        assert_eq!(trim_screen_snapshot("a  \nb\n\n\n"), "a\nb\n");
        assert_eq!(trim_screen_snapshot("\n\n"), "");
        assert_eq!(trim_screen_snapshot(""), "");
    }

    #[test]
    fn trim_snapshot_preserves_interior_blanks() {
        assert_eq!(trim_screen_snapshot("a\n\nb\n"), "a\n\nb\n");
    }

    #[test]
    fn tail_file_returns_last_lines_and_tolerates_non_utf8() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        // Lines plus an invalid UTF-8 byte that must not break tailing.
        file.write_all(b"one\ntwo\nthree\n\xff\xfe\nfour\n")
            .unwrap();
        file.flush().unwrap();
        let out = tail_file(file.path(), 2).unwrap();
        assert!(out.ends_with("four\n"), "got {out:?}");
        assert_eq!(out.lines().count(), 2);
    }

    #[test]
    fn tail_file_handles_more_lines_than_present() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"only\n").unwrap();
        file.flush().unwrap();
        assert_eq!(tail_file(file.path(), 100).unwrap(), "only\n");
    }
}
