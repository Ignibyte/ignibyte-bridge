//! PTY output capture, input forwarding, and log/snapshot text helpers.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result};

use crate::{
    clean::AnsiCleaner, paths::write_atomic, CLEAN_LOG, RAW_LOG, SCREEN_COLS, SCREEN_ROWS,
    SCREEN_SNAPSHOT,
};

pub fn capture_output(reader: &mut Box<dyn Read + Send>, session_dir: &Path) -> Result<()> {
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
    let mut terminal = vt100::Parser::new(SCREEN_ROWS, SCREEN_COLS, 2_000);
    let mut cleaner = AnsiCleaner::default();
    let mut clean = Vec::new();
    let mut buffer = [0_u8; 8192];

    loop {
        let read = reader.read(&mut buffer).context("failed to read PTY")?;
        if read == 0 {
            return Ok(());
        }

        let chunk = &buffer[..read];
        raw_log
            .write_all(chunk)
            .context("failed to write raw log")?;
        raw_log.flush().context("failed to flush raw log")?;

        clean.clear();
        cleaner.clean(chunk, &mut clean);
        clean_log
            .write_all(&clean)
            .context("failed to write clean log")?;
        clean_log.flush().context("failed to flush clean log")?;

        terminal.process(chunk);
        write_atomic(
            &session_dir.join(SCREEN_SNAPSHOT),
            trim_screen_snapshot(&terminal.screen().contents()).as_bytes(),
        )
        .context("failed to write screen snapshot")?;
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

    let mut file = std::fs::File::open(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
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
