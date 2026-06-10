//! Terminal key-name to byte-sequence encoding.

use anyhow::{bail, Result};

pub fn encode_key(key: &str) -> Result<&'static [u8]> {
    match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => Ok(b"\r"),
        "escape" | "esc" => Ok(b"\x1b"),
        "ctrl-c" | "control-c" | "c-c" => Ok(b"\x03"),
        "ctrl-d" | "control-d" | "c-d" => Ok(b"\x04"),
        "ctrl-z" | "control-z" | "c-z" => Ok(b"\x1a"),
        "tab" => Ok(b"\t"),
        "backspace" | "bs" => Ok(b"\x7f"),
        "delete" | "del" => Ok(b"\x1b[3~"),
        "up" | "arrow-up" => Ok(b"\x1b[A"),
        "down" | "arrow-down" => Ok(b"\x1b[B"),
        "right" | "arrow-right" => Ok(b"\x1b[C"),
        "left" | "arrow-left" => Ok(b"\x1b[D"),
        "home" => Ok(b"\x1b[H"),
        "end" => Ok(b"\x1b[F"),
        _ => bail!("unsupported key '{key}'"),
    }
}
