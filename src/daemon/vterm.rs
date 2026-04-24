//! Virtual terminal emulator wrapping `vt100::Parser`.
//!
//! The raw PTY byte stream contains cursor-addressed control sequences
//! (CSI H, CSI 2J, CSI K, etc.) that TUI apps use to redraw the screen
//! in-place. Naive ANSI-stripping of such a stream yields garbled fragments
//! because the final display depends on cursor position, not line order.
//!
//! `VirtualTerminal` feeds bytes into a vt100 parser which maintains a
//! rendered screen grid. `rendered_screen()` returns that grid as plain
//! text; `rendered_screen_formatted()` returns it with SGR color/attribute
//! codes preserved (but no cursor positioning).

use vt100::Color;

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

    /// Return the last `n` non-empty lines of the rendered screen, with SGR
    /// (color/attribute) escape codes preserved. No cursor-positioning codes
    /// are emitted — callers are responsible for placing the cursor.
    ///
    /// Each returned line begins with SGR reset so state does not bleed in
    /// from previous output; lines are separated by `\n`. The sequence ends
    /// with SGR reset so callers need not emit one themselves.
    pub fn rendered_last_lines_formatted(&self, n: usize) -> Vec<u8> {
        if n == 0 {
            return Vec::new();
        }
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        // Text per row, used to trim trailing blank rows the same way
        // rendered_last_lines does.
        let mut row_texts: Vec<String> = Vec::with_capacity(rows as usize);
        for r in 0..rows {
            let mut text = String::new();
            for c in 0..cols {
                if let Some(cell) = screen.cell(r, c) {
                    if !cell.is_wide_continuation() {
                        text.push_str(cell.contents());
                    }
                }
            }
            row_texts.push(text);
        }
        // Skip blank rows (matches rendered_last_lines's filter(|l| !l.trim().is_empty())).
        let non_blank: Vec<u16> = row_texts
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.trim().is_empty())
            .map(|(i, _)| i as u16)
            .collect();
        if non_blank.is_empty() {
            return Vec::new();
        }
        let start = non_blank.len().saturating_sub(n);
        let selected = &non_blank[start..];

        let mut out = Vec::new();
        for (i, &row_idx) in selected.iter().enumerate() {
            if i > 0 {
                out.push(b'\n');
            }
            write_row_sgr(&mut out, screen, row_idx, cols);
        }
        // Final reset so downstream writes aren't colored.
        out.extend_from_slice(b"\x1b[0m");
        out
    }
}

/// SGR-relevant attributes of a cell.
#[derive(Clone, Copy, PartialEq, Eq)]
struct SgrAttrs {
    fg: Color,
    bg: Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl SgrAttrs {
    fn default_state() -> Self {
        Self {
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
        }
    }

    fn from_cell(cell: &vt100::Cell) -> Self {
        Self {
            fg: cell.fgcolor(),
            bg: cell.bgcolor(),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }
}

/// Emit an SGR sequence that sets the terminal to `attrs`. Always emits a
/// reset first so the result is absolute rather than diff-based — simpler
/// than tracking the previous state for correctness.
fn emit_sgr(out: &mut Vec<u8>, attrs: &SgrAttrs) {
    let mut params: Vec<String> = vec!["0".to_string()];
    if attrs.bold {
        params.push("1".to_string());
    }
    if attrs.dim {
        params.push("2".to_string());
    }
    if attrs.italic {
        params.push("3".to_string());
    }
    if attrs.underline {
        params.push("4".to_string());
    }
    if attrs.inverse {
        params.push("7".to_string());
    }
    match attrs.fg {
        Color::Default => {}
        Color::Idx(i) if i < 8 => params.push((30 + i as u16).to_string()),
        Color::Idx(i) if i < 16 => params.push((90 + (i as u16 - 8)).to_string()),
        Color::Idx(i) => {
            params.push("38".to_string());
            params.push("5".to_string());
            params.push(i.to_string());
        }
        Color::Rgb(r, g, b) => {
            params.push("38".to_string());
            params.push("2".to_string());
            params.push(r.to_string());
            params.push(g.to_string());
            params.push(b.to_string());
        }
    }
    match attrs.bg {
        Color::Default => {}
        Color::Idx(i) if i < 8 => params.push((40 + i as u16).to_string()),
        Color::Idx(i) if i < 16 => params.push((100 + (i as u16 - 8)).to_string()),
        Color::Idx(i) => {
            params.push("48".to_string());
            params.push("5".to_string());
            params.push(i.to_string());
        }
        Color::Rgb(r, g, b) => {
            params.push("48".to_string());
            params.push("2".to_string());
            params.push(r.to_string());
            params.push(g.to_string());
            params.push(b.to_string());
        }
    }
    out.extend_from_slice(b"\x1b[");
    out.extend_from_slice(params.join(";").as_bytes());
    out.push(b'm');
}

/// Write one row to `out` as SGR-formatted bytes. Trailing blank cells at
/// the end of the row are dropped so a colored background does not extend
/// past the last visible character.
fn write_row_sgr(out: &mut Vec<u8>, screen: &vt100::Screen, row: u16, cols: u16) {
    // Find the last column that has visible content so we don't emit
    // trailing blank cells (which would paint the background past text).
    let mut last_content_col: Option<u16> = None;
    for c in 0..cols {
        if let Some(cell) = screen.cell(row, c) {
            if cell.has_contents() && !cell.is_wide_continuation() {
                let contents = cell.contents();
                if !contents.is_empty() && contents != " " {
                    last_content_col = Some(c);
                }
            }
        }
    }
    // Always start each line with a reset so no prior-line state leaks.
    let mut current = SgrAttrs::default_state();
    out.extend_from_slice(b"\x1b[0m");
    let limit = match last_content_col {
        Some(c) => c + 1,
        None => return,
    };
    for c in 0..limit {
        let cell = match screen.cell(row, c) {
            Some(cell) => cell,
            None => break,
        };
        if cell.is_wide_continuation() {
            continue;
        }
        let new_attrs = SgrAttrs::from_cell(cell);
        if new_attrs != current {
            emit_sgr(out, &new_attrs);
            current = new_attrs;
        }
        let contents = cell.contents();
        if contents.is_empty() {
            out.push(b' ');
        } else {
            out.extend_from_slice(contents.as_bytes());
        }
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

    // ---- rendered_last_lines_formatted: SGR preservation ----

    fn strip_escapes(bytes: &[u8]) -> String {
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() {
                    let b = bytes[i];
                    i += 1;
                    if (0x40..=0x7E).contains(&b) {
                        break;
                    }
                }
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn formatted_plain_text_has_no_cursor_movement_codes() {
        // Plain text in should produce plain text out, with only SGR reset
        // wrappers added — no cursor positioning escapes that would move
        // the cursor away from where a caller placed it.
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"hello world");
        let out = vt.rendered_last_lines_formatted(5);
        // Check no cursor positioning — CSI H, CSI A, CSI C, etc.
        let text = String::from_utf8_lossy(&out).to_string();
        for bad in ["\x1b[H", "\x1b[1;1H", "\x1b[A", "\x1b[B", "\x1b[C", "\x1b[D", "\x1b[J", "\x1b[K"] {
            assert!(!text.contains(bad), "found cursor code {:?} in: {:?}", bad, text);
        }
        assert_eq!(strip_escapes(&out), "hello world");
    }

    #[test]
    fn formatted_preserves_foreground_color() {
        // Process input with ANSI red color. Verify the output contains an
        // SGR sequence setting red (31) and the text.
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"\x1b[31mred text\x1b[0m");
        let out = vt.rendered_last_lines_formatted(5);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("red text"), "missing text: {:?}", text);
        // Look for any SGR sequence containing ";31" or "[31" — standard red fg.
        assert!(
            text.contains("\x1b[") && (text.contains(";31") || text.contains("[31")) ,
            "expected SGR red code in {:?}",
            text
        );
    }

    #[test]
    fn formatted_preserves_bold() {
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"\x1b[1mbold\x1b[0m plain");
        let out = vt.rendered_last_lines_formatted(5);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("bold"));
        // Bold attribute SGR is 1.
        assert!(
            text.contains("\x1b[0;1") || text.contains(";1m") || text.contains("[1m"),
            "expected bold SGR in {:?}",
            text
        );
    }

    #[test]
    fn formatted_ends_with_reset() {
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"\x1b[31mred\x1b[0m");
        let out = vt.rendered_last_lines_formatted(5);
        // Output should end with an SGR reset so it doesn't color subsequent output.
        assert!(
            out.ends_with(b"\x1b[0m"),
            "expected trailing reset, got tail: {:?}",
            &out[out.len().saturating_sub(8)..]
        );
    }

    #[test]
    fn formatted_n_zero_returns_empty() {
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"\x1b[31mred\x1b[0m");
        assert!(vt.rendered_last_lines_formatted(0).is_empty());
    }

    #[test]
    fn formatted_multiple_lines_separated_by_newlines() {
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"line1\r\nline2\r\nline3\r\n");
        let out = vt.rendered_last_lines_formatted(10);
        let plain = strip_escapes(&out);
        assert_eq!(plain, "line1\nline2\nline3");
    }

    #[test]
    fn formatted_returns_only_last_n_non_blank_lines() {
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"a\r\nb\r\nc\r\nd\r\ne\r\n");
        let out = vt.rendered_last_lines_formatted(2);
        let plain = strip_escapes(&out);
        assert_eq!(plain, "d\ne");
    }

    #[test]
    fn formatted_consumes_cursor_redraws_like_plain_does() {
        // The same TUI-redraw scenario as `tui_redraw_sequence_matches_real_output`
        // but via the formatted path — garbage must be gone, FINAL must remain,
        // and there must be no leftover cursor-positioning escapes.
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"garbage line 1\r\n");
        vt.process(b"more garbage\r\n");
        vt.process(b"\x1b[2J\x1b[H");
        vt.process(b"FINAL: hello\r\n");
        let out = vt.rendered_last_lines_formatted(10);
        let plain = strip_escapes(&out);
        assert!(!plain.contains("garbage"), "got: {:?}", plain);
        assert!(plain.contains("FINAL: hello"), "got: {:?}", plain);
    }
}
