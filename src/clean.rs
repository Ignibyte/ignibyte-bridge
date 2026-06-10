//! Streaming ANSI/terminal-control stripper for the human-readable clean log.
//!
//! Unlike a per-chunk strip, [`AnsiCleaner`] carries parser state across calls,
//! so an escape sequence split over two reads is still removed cleanly. It also
//! keeps content that a naive C0 filter drops: tabs are preserved, and carriage
//! returns are normalized to newlines (`\r\n` collapses to one `\n`, a lone
//! `\r` — e.g. a progress redraw — becomes a line break) so `read`/`--tail`
//! see one line per frame instead of one fused mega-line.

/// Stateful stripper. Construct once per capture stream and feed each chunk to
/// [`AnsiCleaner::clean`].
#[derive(Default)]
pub struct AnsiCleaner {
    state: State,
    /// A `\r` was seen and we are waiting to see whether a `\n` follows.
    pending_cr: bool,
}

#[derive(Default, Clone, Copy)]
enum State {
    /// Normal text.
    #[default]
    Ground,
    /// Saw `ESC`; the next byte selects the sequence type.
    Escape,
    /// `ESC` followed by intermediate bytes, waiting for a final byte.
    EscapeIntermediate,
    /// Inside a CSI sequence (`ESC [`), waiting for a final byte.
    Csi,
    /// Inside an OSC sequence (`ESC ]`), terminated by `BEL` or `ST`.
    Osc,
    /// Inside OSC and saw `ESC`; the next byte completes the `ST` terminator.
    OscEsc,
    /// Inside a DCS/SOS/PM/APC string, terminated by `ST`.
    StringSeq,
    /// Inside a string sequence and saw `ESC`; next byte completes `ST`.
    StringEsc,
}

impl AnsiCleaner {
    /// Append the cleaned form of `input` to `out`.
    pub fn clean(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &byte in input {
            self.step(byte, out);
        }
    }

    fn step(&mut self, byte: u8, out: &mut Vec<u8>) {
        match self.state {
            State::Ground => self.ground(byte, out),
            State::Escape => self.escape(byte),
            State::EscapeIntermediate => {
                if (0x30..=0x7e).contains(&byte) {
                    self.state = State::Ground;
                }
            }
            State::Csi => {
                // Parameter (0x30-0x3f) and intermediate (0x20-0x2f) bytes stay
                // in CSI; a final byte (0x40-0x7e) ends it.
                if (0x40..=0x7e).contains(&byte) {
                    self.state = State::Ground;
                }
            }
            State::Osc => match byte {
                0x07 => self.state = State::Ground, // BEL terminates
                0x1b => self.state = State::OscEsc, // possible ST
                _ => {}
            },
            State::OscEsc => self.state = State::Ground,
            State::StringSeq => {
                if byte == 0x1b {
                    self.state = State::StringEsc;
                }
            }
            State::StringEsc => self.state = State::Ground,
        }
    }

    fn ground(&mut self, byte: u8, out: &mut Vec<u8>) {
        if self.pending_cr {
            self.pending_cr = false;
            out.push(b'\n');
            if byte == b'\n' {
                // `\r\n` already emitted as a single newline.
                return;
            }
        }

        match byte {
            0x1b => self.state = State::Escape,
            b'\r' => self.pending_cr = true,
            b'\n' | b'\t' => out.push(byte),
            // Drop the remaining C0 controls and DEL; keep everything else
            // (printable ASCII and UTF-8 bytes) verbatim.
            0x00..=0x08 | 0x0b..=0x0c | 0x0e..=0x1f | 0x7f => {}
            _ => out.push(byte),
        }
    }

    fn escape(&mut self, byte: u8) {
        self.state = match byte {
            b'[' => State::Csi,
            b']' => State::Osc,
            b'P' | b'X' | b'^' | b'_' => State::StringSeq,
            0x20..=0x2f => State::EscapeIntermediate,
            // Any other byte is the final byte of a short escape sequence.
            _ => State::Ground,
        };
    }
}

/// Convenience wrapper: clean a single standalone buffer.
#[cfg(test)]
pub fn clean_all(input: &[u8]) -> Vec<u8> {
    let mut cleaner = AnsiCleaner::default();
    let mut out = Vec::new();
    cleaner.clean(input, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean_str(input: &[u8]) -> String {
        String::from_utf8(clean_all(input)).unwrap()
    }

    #[test]
    fn strips_csi_color_sequences() {
        assert_eq!(clean_str(b"\x1b[38;5;240mhello\x1b[0m"), "hello");
    }

    #[test]
    fn preserves_tabs() {
        assert_eq!(clean_str(b"a\tb\tc"), "a\tb\tc");
    }

    #[test]
    fn collapses_crlf_to_single_newline() {
        assert_eq!(clean_str(b"line1\r\nline2\r\n"), "line1\nline2\n");
    }

    #[test]
    fn lone_cr_becomes_newline() {
        // A `\r` between text is a line break; a trailing `\r` stays buffered
        // (it may begin a `\r\n`) and so is not emitted at end of stream.
        assert_eq!(clean_str(b"frame1\rframe2\rframe3"), "frame1\nframe2\nframe3");
        assert_eq!(clean_str(b"frame1\rframe2\r"), "frame1\nframe2");
    }

    #[test]
    fn drops_other_c0_controls_and_del() {
        assert_eq!(clean_str(b"a\x07b\x08c\x7f"), "abc");
    }

    #[test]
    fn escape_split_across_chunks_is_stripped() {
        // The escape sequence straddles a read boundary; a stateless per-chunk
        // strip would leak "40mtext". The persistent parser must not.
        let mut cleaner = AnsiCleaner::default();
        let mut out = Vec::new();
        cleaner.clean(b"text\x1b[38;5;2", &mut out);
        cleaner.clean(b"40mmore", &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "textmore");
    }

    #[test]
    fn crlf_split_across_chunks_collapses() {
        let mut cleaner = AnsiCleaner::default();
        let mut out = Vec::new();
        cleaner.clean(b"line\r", &mut out);
        cleaner.clean(b"\nnext", &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "line\nnext");
    }

    #[test]
    fn strips_osc_title_terminated_by_bel() {
        assert_eq!(clean_str(b"\x1b]0;window title\x07body"), "body");
    }

    #[test]
    fn strips_osc_terminated_by_st() {
        assert_eq!(clean_str(b"\x1b]8;;http://x\x1b\\link"), "link");
    }

    #[test]
    fn preserves_utf8_multibyte() {
        assert_eq!(clean_str("café→\u{1f600}".as_bytes()), "café→\u{1f600}");
    }
}
