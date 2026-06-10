//! Client/daemon wire types and the Unix-socket client.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::socket_path;

/// How long the client waits for the daemon to accept input or produce a
/// response before giving up. A suspended or wedged daemon would otherwise hang
/// every command forever with no diagnostic.
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum DaemonRequest {
    Start {
        name: String,
        cwd: Option<PathBuf>,
        cmd: String,
        /// The client's PATH, so the daemon resolves and runs the command with
        /// the user's environment rather than the daemon's. Older clients omit
        /// it, in which case the daemon falls back to its own PATH.
        #[serde(default)]
        path: Option<String>,
    },
    Send {
        name: String,
        text: String,
        no_enter: bool,
    },
    Keys {
        name: String,
        keys: Vec<String>,
    },
    Read {
        name: String,
        tail: usize,
        raw: bool,
    },
    Screen {
        name: String,
        tail: usize,
    },
    Status {
        name: String,
    },
    List,
    Stop {
        name: String,
    },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub ok: bool,
    pub output: String,
    pub error: Option<String>,
}

pub fn try_send_daemon_request(request: &DaemonRequest) -> Result<Option<DaemonResponse>> {
    let path = socket_path()?;
    let mut stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to connect to daemon at {}", path.display()));
        }
    };

    stream
        .set_read_timeout(Some(CLIENT_IO_TIMEOUT))
        .context("failed to set daemon read timeout")?;
    stream
        .set_write_timeout(Some(CLIENT_IO_TIMEOUT))
        .context("failed to set daemon write timeout")?;

    serde_json::to_writer(&mut stream, request).context("failed to write daemon request")?;
    stream
        .write_all(b"\n")
        .context("failed to finish daemon request")?;
    stream.flush().context("failed to flush daemon request")?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|error| {
        if matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            anyhow::anyhow!(
                "daemon at {} accepted the connection but did not respond within {}s; \
                 it may be suspended or wedged — restart it or rerun with --direct",
                path.display(),
                CLIENT_IO_TIMEOUT.as_secs()
            )
        } else {
            anyhow::Error::new(error).context("failed to read daemon response")
        }
    })?;
    if line.is_empty() {
        bail!("daemon closed connection without a response");
    }

    let response = serde_json::from_str(&line).context("failed to parse daemon response")?;
    Ok(Some(response))
}
