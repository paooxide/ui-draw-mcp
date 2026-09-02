//! Turn raw terminal output into text an agent can read.
//!
//! A PTY carries a rendering protocol, not a transcript: colours, cursor moves,
//! title changes, bracketed-paste toggles. Handing that to a model wastes tokens
//! and invites it to "read" escape bytes as content, so the control sequences
//! come out here.

/// Strip ANSI/VT control sequences and normalize line endings.
pub fn strip(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => {
                i += 1;
                if i >= bytes.len() {
                    break;
                }
                match bytes[i] {
                    // CSI: ESC [ params... final(@-~)
                    b'[' => {
                        i += 1;
                        while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                            i += 1;
                        }
                        i += 1;
                    }
                    // OSC: ESC ] ... terminated by BEL or ESC \ (window titles).
                    b']' => {
                        i += 1;
                        while i < bytes.len() {
                            if bytes[i] == 0x07 {
                                i += 1;
                                break;
                            }
                            if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                    }
                    // Two-byte and single-char escapes (charset selection, RI, ...).
                    b'(' | b')' | b'*' | b'+' | b'#' | b'%' => i += 2,
                    _ => i += 1,
                }
            }
            // Bare CR: a shell redraws the current line with it. Collapse CRLF
            // to LF and drop the rest so a progress bar does not arrive as a
            // hundred near-identical lines.
            b'\r' => {
                if bytes.get(i + 1) == Some(&b'\n') {
                    out.push('\n');
                    i += 2;
                } else {
                    i += 1;
                }
            }
            // Backspace overstrike, as used by `man`.
            0x08 => {
                out.pop();
                i += 1;
            }
            b'\n' | b'\t' => {
                out.push(bytes[i] as char);
                i += 1;
            }
            b if b < 0x20 || b == 0x7f => i += 1,
            _ => {
                // Copy one whole UTF-8 scalar so multi-byte characters survive.
                let start = i;
                i += 1;
                while i < bytes.len() && (bytes[i] & 0xC0) == 0x80 {
                    i += 1;
                }
                out.push_str(&String::from_utf8_lossy(&bytes[start..i]));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_and_cursor_moves_are_removed() {
        assert_eq!(strip("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip("a\x1b[2Kb"), "ab");
        assert_eq!(strip("\x1b[?2004hprompt$ "), "prompt$ ");
    }

    #[test]
    fn window_title_sequences_are_removed_with_either_terminator() {
        assert_eq!(strip("\x1b]0;my title\x07text"), "text");
        assert_eq!(strip("\x1b]0;my title\x1b\\text"), "text");
    }

    /// A progress bar redraws with bare CR. Keeping them would deliver the same
    /// line dozens of times.
    #[test]
    fn line_endings_normalize_and_bare_cr_is_dropped() {
        assert_eq!(strip("one\r\ntwo\r\n"), "one\ntwo\n");
        assert_eq!(strip("50%\r60%\r70%"), "50%60%70%");
    }

    #[test]
    fn backspace_overstrike_resolves_to_plain_text() {
        assert_eq!(strip("N\x08NA\x08AME"), "NAME");
    }

    #[test]
    fn utf8_survives() {
        assert_eq!(
            strip("caf\u{e9} \u{2192} \u{1f600}"),
            "caf\u{e9} \u{2192} \u{1f600}"
        );
        assert_eq!(strip("\x1b[32m\u{2713}\x1b[0m done"), "\u{2713} done");
    }

    #[test]
    fn incomplete_escape_at_end_does_not_panic() {
        assert_eq!(strip("text\x1b"), "text");
        assert_eq!(strip("text\x1b["), "text");
    }
}
