//! PTY output capture, input forwarding, and log/snapshot text helpers.

use std::{
    fs::OpenOptions,
    io::{Read, Write},
    path::Path,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};

use crate::{CLEAN_LOG, RAW_LOG, SCREEN_COLS, SCREEN_ROWS, SCREEN_SNAPSHOT};

pub fn capture_output(reader: &mut Box<dyn Read + Send>, session_dir: &Path) -> Result<()> {
    let mut raw_log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(session_dir.join(RAW_LOG))
        .context("failed to open raw log")?;
    let mut clean_log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(session_dir.join(CLEAN_LOG))
        .context("failed to open clean log")?;
    let mut terminal = vt100::Parser::new(SCREEN_ROWS, SCREEN_COLS, 2_000);
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

        let clean = strip_ansi_escapes::strip(chunk);
        clean_log
            .write_all(&clean)
            .context("failed to write clean log")?;
        clean_log.flush().context("failed to flush clean log")?;

        terminal.process(chunk);
        std::fs::write(
            session_dir.join(SCREEN_SNAPSHOT),
            trim_screen_snapshot(&terminal.screen().contents()),
        )
        .context("failed to write screen snapshot")?;
    }
}

pub fn forward_input(input_fifo: &Path, writer: &mut Box<dyn Write + Send>) -> Result<()> {
    let mut fifo = OpenOptions::new()
        .read(true)
        .write(true)
        .open(input_fifo)
        .with_context(|| format!("failed to open {}", input_fifo.display()))?;
    let mut buffer = [0_u8; 8192];

    loop {
        let read = fifo
            .read(&mut buffer)
            .context("failed to read input FIFO")?;
        if read == 0 {
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        writer
            .write_all(&buffer[..read])
            .context("failed to write input to PTY")?;
        writer.flush().context("failed to flush PTY input")?;
    }
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
