//! Virtual terminal emulator wrapping `vt100::Parser`.
//!
//! The raw PTY byte stream contains cursor-addressed control sequences
//! (CSI H, CSI 2J, CSI K, etc.) that TUI apps use to redraw the screen
//! in-place. Naive ANSI-stripping of such a stream yields garbled fragments
//! because the final display depends on cursor position, not line order.
//!
//! `VirtualTerminal` feeds bytes into a vt100 parser which maintains a
//! rendered screen grid. `rendered_screen()` returns that grid as plain
//! text — the same content a user would see on a real terminal.

pub struct VirtualTerminal {
    parser: vt100::Parser,
}

impl VirtualTerminal {
    /// Create a new virtual terminal with the given dimensions.
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
        }
    }

    /// Feed raw PTY bytes to the emulator.
    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Resize the emulator's screen grid.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// Return the rendered screen as plain UTF-8 text.
    ///
    /// Trailing blank lines and per-row trailing whitespace are trimmed.
    /// The result reflects cursor-addressed screen state, not the raw byte
    /// stream — TUI app redraws (CSI H, CSI 2J, etc.) produce correct output.
    pub fn rendered_screen(&self) -> String {
        self.parser.screen().contents()
    }

    /// Return the last `n` non-empty lines of the rendered screen.
    pub fn rendered_last_lines(&self, n: usize) -> String {
        if n == 0 {
            return String::new();
        }
        let contents = self.rendered_screen();
        let lines: Vec<&str> = contents
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        let start = lines.len().saturating_sub(n);
        lines[start..].join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"hello world");
        let screen = vt.rendered_screen();
        assert!(
            screen.starts_with("hello world"),
            "expected 'hello world' at start, got: {:?}",
            screen
        );
    }

    #[test]
    fn newlines_produce_multiple_lines() {
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"line1\r\nline2\r\nline3\r\n");
        let screen = vt.rendered_screen();
        let lines: Vec<&str> = screen.lines().collect();
        assert_eq!(lines[0], "line1");
        assert_eq!(lines[1], "line2");
        assert_eq!(lines[2], "line3");
    }

    #[test]
    fn clear_screen_and_cursor_home_renders_final_state() {
        // Simulate a TUI redraw: write garbage, then clear, then write final.
        // After CSI 2J (clear) + CSI H (cursor home), only the post-clear
        // content should appear on screen.
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"garbage that should disappear\r\nmore garbage\r\n");
        vt.process(b"\x1b[2J\x1b[H"); // clear screen, cursor home
        vt.process(b"final content");
        let screen = vt.rendered_screen();
        assert!(
            screen.starts_with("final content"),
            "expected 'final content' after clear, got: {:?}",
            screen
        );
        assert!(
            !screen.contains("garbage"),
            "cleared garbage should not appear, got: {:?}",
            screen
        );
    }

    #[test]
    fn cursor_addressed_writes_overwrite_in_place() {
        // Write "abc" at column 0, then move cursor to column 0 and write "X".
        // Result should be "Xbc", not "abcX" and not "abcabc".
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"abc");
        vt.process(b"\x1b[H"); // cursor to (1,1)
        vt.process(b"X");
        let screen = vt.rendered_screen();
        let first_line = screen.lines().next().unwrap_or("");
        assert_eq!(first_line, "Xbc", "expected Xbc, got: {:?}", first_line);
    }

    #[test]
    fn clear_to_end_of_line() {
        // Write "abcdef", move cursor back to column 4, clear to EOL.
        // Result: "abc"
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"abcdef");
        vt.process(b"\x1b[1;4H"); // cursor to row 1, col 4
        vt.process(b"\x1b[K"); // clear from cursor to EOL
        let screen = vt.rendered_screen();
        let first_line = screen.lines().next().unwrap_or("");
        assert_eq!(first_line, "abc", "expected 'abc', got: {:?}", first_line);
    }

    #[test]
    fn ansi_color_codes_stripped_from_output() {
        // Color codes should not appear in the rendered plain-text output.
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"\x1b[32mgreen\x1b[0m \x1b[1mbold\x1b[0m");
        let screen = vt.rendered_screen();
        let first_line = screen.lines().next().unwrap_or("");
        assert_eq!(first_line, "green bold");
        assert!(!screen.contains("\x1b"));
    }

    #[test]
    fn rendered_last_lines_returns_n_lines() {
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"a\r\nb\r\nc\r\nd\r\ne\r\n");
        let last3 = vt.rendered_last_lines(3);
        assert_eq!(last3, "c\nd\ne");
    }

    #[test]
    fn rendered_last_lines_zero_returns_empty() {
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"a\r\nb\r\n");
        assert_eq!(vt.rendered_last_lines(0), "");
    }

    #[test]
    fn rendered_last_lines_more_than_available() {
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"a\r\nb\r\n");
        let all = vt.rendered_last_lines(10);
        assert_eq!(all, "a\nb");
    }

    #[test]
    fn tui_redraw_sequence_matches_real_output() {
        // Exact byte sequence produced by:
        //   printf "garbage line 1\n"
        //   printf "more garbage\n"
        //   printf "\033[2J\033[H"
        //   printf "FINAL: hello\n"
        // under a cooked-mode PTY (\n → \r\n due to OPOST/ONLCR).
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"garbage line 1\r\n");
        vt.process(b"more garbage\r\n");
        vt.process(b"\x1b[2J\x1b[H");
        vt.process(b"FINAL: hello\r\n");
        let screen = vt.rendered_screen();
        assert!(
            !screen.contains("garbage"),
            "garbage should be cleared, got: {:?}",
            screen
        );
        assert!(
            !screen.contains("[2J") && !screen.contains("[H"),
            "escape sequences should be consumed, got: {:?}",
            screen
        );
        assert!(
            screen.contains("FINAL: hello"),
            "expected FINAL, got: {:?}",
            screen
        );
    }

    #[test]
    fn resize_updates_screen_dimensions() {
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"hello");
        vt.resize(10, 40);
        // After resize the content should still be queryable without panic.
        let screen = vt.rendered_screen();
        assert!(screen.starts_with("hello"));
    }
}
