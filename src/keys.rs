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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_named_keys() {
        assert_eq!(encode_key("enter").unwrap(), b"\r");
        assert_eq!(encode_key("escape").unwrap(), b"\x1b");
        assert_eq!(encode_key("ctrl-c").unwrap(), b"\x03");
        assert_eq!(encode_key("tab").unwrap(), b"\t");
        assert_eq!(encode_key("up").unwrap(), b"\x1b[A");
        assert_eq!(encode_key("backspace").unwrap(), b"\x7f");
    }

    #[test]
    fn accepts_aliases_and_is_case_insensitive() {
        assert_eq!(encode_key("return").unwrap(), encode_key("enter").unwrap());
        assert_eq!(encode_key("esc").unwrap(), encode_key("escape").unwrap());
        assert_eq!(encode_key("c-c").unwrap(), encode_key("ctrl-c").unwrap());
        assert_eq!(encode_key("ESC").unwrap(), encode_key("escape").unwrap());
        assert_eq!(encode_key("Ctrl-C").unwrap(), encode_key("ctrl-c").unwrap());
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(encode_key("f13").is_err());
        assert!(encode_key("").is_err());
    }
}
