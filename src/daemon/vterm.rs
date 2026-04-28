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

/// Number of rows the live parser keeps in its internal scrollback. When
/// streaming output scrolls past the live screen, those rows move into this
/// buffer and remain queryable for tall preview captures (bd-pmk). 200 rows
/// is enough for any reasonable preview height; per-cell vt100 storage caps
/// memory at well under a megabyte per session even at 200x200.
const PARSER_SCROLLBACK_ROWS: usize = 200;

pub struct VirtualTerminal {
    parser: vt100::Parser,
}

impl VirtualTerminal {
    /// Create a new virtual terminal with the given dimensions.
    ///
    /// The internal vt100 parser retains `PARSER_SCROLLBACK_ROWS` rows of
    /// scrolled-off content so streaming output exceeding the screen height
    /// can still be recovered for preview / capture (bd-pmk).
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, PARSER_SCROLLBACK_ROWS),
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

    /// Return the emulator's current grid size as `(rows, cols)`.
    #[allow(dead_code)]
    pub fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
    }

    /// Return the rendered screen as plain UTF-8 text.
    ///
    /// Trailing blank lines and per-row trailing whitespace are trimmed.
    /// The result reflects cursor-addressed screen state, not the raw byte
    /// stream — TUI app redraws (CSI H, CSI 2J, etc.) produce correct output.
    #[allow(dead_code)]
    pub fn rendered_screen(&self) -> String {
        self.parser.screen().contents()
    }

    /// Return the last `n` non-empty lines of the rendered screen.
    ///
    /// Bounded by the live screen size (PTY rows). For history beyond the
    /// screen — needed by `amux top`'s preview pane in tall terminals — use
    /// `render_raw_scrollback_formatted` against the session's raw scrollback
    /// ring buffer instead (see bd-pmk).
    #[allow(dead_code)]
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

    /// Return the last `n` non-empty lines of the rendered screen plus the
    /// parser's internal scrollback, formatted with SGR codes.
    ///
    /// Reads from the LIVE parser maintained by the daemon's io_loop, which
    /// has tracked every byte the agent has written and therefore reflects
    /// the agent's actual current display state. This is the correct source
    /// for preview / capture: cursor-addressed redraws (e.g. claude's main-
    /// screen partial repaints) are interpreted in their full byte context,
    /// not from a sliding ring buffer that may have evicted earlier setup
    /// (bd-8w7).
    ///
    /// Lines that scrolled off the screen come from the parser's internal
    /// scrollback (configured at construction time) so streaming output
    /// past the screen height is still available (bd-pmk).
    ///
    /// Mutates the parser's `set_scrollback` offset while reading and
    /// restores it before returning. The caller must hold a `&mut` lock.
    pub fn rendered_recent_formatted(&mut self, n: usize) -> Vec<u8> {
        if n == 0 {
            return Vec::new();
        }
        let saved_offset = self.parser.screen().scrollback();
        let (rows, cols) = self.parser.screen().size();
        let screen_rows = rows as usize;

        // Probe actual scrollback length by clamping past max — the vt100
        // crate clamps to the real scrollback size internally.
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let scrollback_len = self.parser.screen().scrollback();

        // Total rows available to walk, oldest first.
        let total = scrollback_len + screen_rows;
        let mut row_texts: Vec<String> = Vec::with_capacity(total);

        // Walk scrollback in batches of `screen_rows`. When set_scrollback(N)
        // is in effect, the parser's visible_rows iterator shows scrollback
        // rows starting at index (scrollback_len - N), then chains with the
        // top of the live screen. For purely-scrollback batches (N >=
        // screen_rows) we read all `screen_rows` cells from positions
        // (scrollback_len - N) .. (scrollback_len - N + screen_rows).
        let mut scrollback_consumed = 0usize;
        while scrollback_consumed < scrollback_len {
            let offset = scrollback_len - scrollback_consumed;
            self.parser.screen_mut().set_scrollback(offset);
            let take = screen_rows.min(scrollback_len - scrollback_consumed);
            let screen = self.parser.screen();
            for r in 0..take {
                let mut text = String::new();
                for c in 0..cols {
                    if let Some(cell) = screen.cell(r as u16, c) {
                        if !cell.is_wide_continuation() {
                            text.push_str(cell.contents());
                        }
                    }
                }
                row_texts.push(text);
            }
            scrollback_consumed += take;
        }

        // Read current screen rows.
        self.parser.screen_mut().set_scrollback(0);
        {
            let screen = self.parser.screen();
            for r in 0..screen_rows {
                let mut text = String::new();
                for c in 0..cols {
                    if let Some(cell) = screen.cell(r as u16, c) {
                        if !cell.is_wide_continuation() {
                            text.push_str(cell.contents());
                        }
                    }
                }
                row_texts.push(text);
            }
        }

        // Restore original scrollback offset before any early return.
        self.parser.screen_mut().set_scrollback(saved_offset);

        // Pick the last `n` non-blank rows by their position in the walk.
        let non_blank: Vec<usize> = row_texts
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.trim().is_empty())
            .map(|(i, _)| i)
            .collect();
        if non_blank.is_empty() {
            return Vec::new();
        }
        let start = non_blank.len().saturating_sub(n);
        let selected = &non_blank[start..];

        // Format the selected rows. To minimise scrollback offset churn we
        // group reads by the offset they require, then format in walk order.
        // A row at total index `t`:
        //   - t < scrollback_len  → set_scrollback(scrollback_len - t), row 0
        //   - t >= scrollback_len → set_scrollback(0), row (t - scrollback_len)
        let mut out = Vec::new();
        for (i, &t) in selected.iter().enumerate() {
            if i > 0 {
                out.push(b'\n');
            }
            if t < scrollback_len {
                self.parser
                    .screen_mut()
                    .set_scrollback(scrollback_len - t);
                write_row_sgr(&mut out, self.parser.screen(), 0, cols);
            } else {
                self.parser.screen_mut().set_scrollback(0);
                write_row_sgr(
                    &mut out,
                    self.parser.screen(),
                    (t - scrollback_len) as u16,
                    cols,
                );
            }
        }
        out.extend_from_slice(b"\x1b[0m");

        // Final restore (the format pass moved the offset around).
        self.parser.screen_mut().set_scrollback(saved_offset);

        out
    }

    /// Return the last `n` non-empty lines of the rendered screen, with SGR
    /// (color/attribute) escape codes preserved. No cursor-positioning codes
    /// are emitted — callers are responsible for placing the cursor.
    ///
    /// Each returned line begins with SGR reset so state does not bleed in
    /// from previous output; lines are separated by `\n`. The sequence ends
    /// with SGR reset so callers need not emit one themselves.
    ///
    /// Bounded by the live screen size; see note on `rendered_last_lines`.
    /// Prefer `rendered_recent_formatted` for preview / capture: it includes
    /// the parser's scrollback rows for streaming output that has scrolled
    /// off the live screen.
    #[allow(dead_code)]
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

/// Replay raw PTY bytes through a freshly-sized vt100 parser and return the
/// last `n` non-blank rendered rows with SGR (color/attribute) escape codes
/// preserved.
///
/// The live `VirtualTerminal` is bound to the agent's PTY size (typically
/// 80x24), so its rendered screen has at most 24 rows of content. Callers
/// like `amux top`'s preview pane in a tall (e.g. 50-row) terminal need
/// more than that. This helper builds a temporary `vt100::Parser` sized to
/// the caller's preview height, feeds it the raw scrollback bytes, and
/// extracts the most recent `n` non-blank rows from that taller render.
///
/// Behavior notes:
/// - Alt-screen toggles in the byte stream are honored — a session that
///   ended in alt-screen mode (e.g. claude REPL, vim) renders its alt
///   screen, which is sized to the caller's `rows`. The agent itself only
///   draws within its own PTY rows, so taller preview rows above/below
///   stay blank and are filtered.
/// - The first few rows of the replay may show partial state if the raw
///   scrollback ring buffer's oldest bytes start mid-escape. Acceptable
///   for a preview pane.
#[allow(dead_code)]
pub fn render_raw_scrollback_formatted(
    raw: &[u8],
    rows: u16,
    cols: u16,
    n: usize,
) -> Vec<u8> {
    if n == 0 || raw.is_empty() {
        return Vec::new();
    }
    let rows = rows.max(1);
    let cols = cols.max(1);
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(raw);
    let screen = parser.screen();

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
    out.extend_from_slice(b"\x1b[0m");
    out
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

    // ---- render_raw_scrollback_formatted: replay raw scrollback into a tall parser ----

    #[test]
    fn raw_replay_more_than_screen_rows() {
        // Bug bd-pmk: live vterm caps at the agent's PTY size (24 rows), so
        // even when the caller asks for 40 lines they only get up to 24.
        // The replay helper builds a fresh tall parser and feeds raw bytes,
        // letting us recover scrolled-off history beyond the live screen.
        let mut raw = Vec::new();
        for i in 1..=80 {
            raw.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        // Replay through a 50-row parser; ask for 40 non-blank lines.
        let out = render_raw_scrollback_formatted(&raw, 50, 80, 40);
        let plain = strip_escapes(&out);
        let lines: Vec<&str> = plain.lines().collect();
        assert!(
            lines.len() >= 35,
            "expected >=35 replayed lines, got {} (lines={:?})",
            lines.len(),
            lines
        );
        // Tail should be the most recent lines.
        assert_eq!(*lines.last().unwrap(), "line 80");
    }

    #[test]
    fn raw_replay_empty_input() {
        let out = render_raw_scrollback_formatted(&[], 50, 80, 40);
        assert!(out.is_empty());
    }

    #[test]
    fn raw_replay_zero_lines() {
        let raw = b"hello\r\nworld\r\n";
        let out = render_raw_scrollback_formatted(raw, 24, 80, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn raw_replay_short_input_within_screen() {
        // Three lines into a 24-row parser; asking for 10 returns those 3.
        let raw = b"a\r\nb\r\nc\r\n";
        let out = render_raw_scrollback_formatted(raw, 24, 80, 10);
        let plain = strip_escapes(&out);
        assert_eq!(plain, "a\nb\nc");
    }

    #[test]
    fn raw_replay_alt_screen_does_not_show_smashed_content() {
        // Agent is in alt-screen mode (CSI ?1049h) drawing a 5-line TUI.
        // Replay through a tall (50-row) parser must still respect alt-screen
        // — output is the 5 visible lines, not crammed into more rows.
        let mut raw = Vec::new();
        // Pre-alt-screen scrollback with normal text.
        raw.extend_from_slice(b"history line 1\r\nhistory line 2\r\n");
        // Enter alt-screen.
        raw.extend_from_slice(b"\x1b[?1049h\x1b[2J\x1b[H");
        // Five lines of TUI.
        for i in 1..=5 {
            raw.extend_from_slice(format!("TUI row {i}\r\n").as_bytes());
        }
        let out = render_raw_scrollback_formatted(&raw, 50, 80, 40);
        let plain = strip_escapes(&out);
        // Should be the alt-screen content; pre-alt-screen history must NOT
        // appear (it lives in the main grid, which alt-screen hides).
        assert!(!plain.contains("history line"), "got: {:?}", plain);
        assert!(plain.contains("TUI row 1"), "got: {:?}", plain);
        assert!(plain.contains("TUI row 5"), "got: {:?}", plain);
    }

    #[test]
    fn raw_replay_strips_cursor_codes() {
        let raw = b"\x1b[31mred line\x1b[0m\r\nplain\r\n";
        let out = render_raw_scrollback_formatted(raw, 24, 80, 5);
        let text = String::from_utf8_lossy(&out).to_string();
        for bad in ["\x1b[H", "\x1b[1;1H", "\x1b[A", "\x1b[B", "\x1b[J", "\x1b[K"] {
            assert!(!text.contains(bad), "found cursor code {:?} in: {:?}", bad, text);
        }
    }

    #[test]
    fn raw_replay_preserves_sgr() {
        let raw = b"\x1b[31mred text\x1b[0m\r\n";
        let out = render_raw_scrollback_formatted(raw, 24, 80, 5);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("red text"), "missing text: {:?}", text);
        assert!(
            text.contains("\x1b[") && (text.contains(";31") || text.contains("[31")),
            "expected SGR red code in {:?}",
            text
        );
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

    // ---- rendered_recent_formatted: snapshot of live screen + parser scrollback ----

    /// bd-8w7 root cause regression: when an agent redraws using cursor
    /// positioning + clear-line (claude's pattern), the OLD scrollback-replay
    /// path produced a different rendering on every capture because the 64KB
    /// raw ring slid past the most recent full UI redraw. With the live-VT
    /// snapshot path, two consecutive captures of an idle (post-redraw) agent
    /// must be byte-identical because the live parser holds the agent's true
    /// current screen state.
    #[test]
    fn recent_formatted_consecutive_captures_idle_session_are_identical() {
        let mut vt = VirtualTerminal::new(24, 80);
        // Simulate a TUI that draws a multi-line UI, then sits idle. Cursor
        // jumps around, lines get rewritten with CSI K, etc.
        vt.process(b"\x1b[H\x1b[2J");
        vt.process(b"\x1b[1;1HHEADER\x1b[K");
        vt.process(b"\x1b[3;1Hcontent line A\x1b[K");
        vt.process(b"\x1b[4;1Hcontent line B\x1b[K");
        vt.process(b"\x1b[24;1HTOOLBAR\x1b[K");
        // Many partial repaints of just the toolbar (claude's spinner pattern).
        for _ in 0..20 {
            vt.process(b"\x1b[24;1HTOOLBAR\x1b[K");
        }

        let cap1 = vt.rendered_recent_formatted(40);
        let cap2 = vt.rendered_recent_formatted(40);
        assert_eq!(cap1, cap2, "consecutive idle captures must be identical");
    }

    /// bd-8w7 symptom regression: capturing a session that uses cursor-
    /// addressed redraws must return the agent's current screen state in
    /// full — not just the rows that received the most recent partial
    /// repaint. The OLD replay path would lose the static rows because the
    /// fresh parser only saw the partial-repaint bytes.
    #[test]
    fn recent_formatted_returns_full_screen_for_cursor_addressed_tui() {
        let mut vt = VirtualTerminal::new(24, 80);
        // Initial full UI paint (CSI commands address every region).
        vt.process(b"\x1b[H\x1b[2J");
        vt.process(b"\x1b[1;1HHEADER\x1b[K");
        vt.process(b"\x1b[3;1Hbody row 1\x1b[K");
        vt.process(b"\x1b[4;1Hbody row 2\x1b[K");
        vt.process(b"\x1b[5;1Hbody row 3\x1b[K");
        vt.process(b"\x1b[24;1HTOOLBAR\x1b[K");
        // Followed by many partial repaints — exactly the claude spinner case.
        for i in 0..50 {
            vt.process(format!("\x1b[24;1HTOOLBAR {i}\x1b[K").as_bytes());
        }

        let out = vt.rendered_recent_formatted(40);
        let plain = strip_escapes(&out);
        assert!(plain.contains("HEADER"), "missing HEADER: {plain:?}");
        assert!(plain.contains("body row 1"), "missing body row 1: {plain:?}");
        assert!(plain.contains("body row 2"), "missing body row 2: {plain:?}");
        assert!(plain.contains("body row 3"), "missing body row 3: {plain:?}");
        assert!(plain.contains("TOOLBAR"), "missing TOOLBAR: {plain:?}");
    }

    /// bd-pmk regression guard: streaming output that scrolls past the
    /// agent's PTY size should still be recoverable in tall captures via
    /// the live parser's internal scrollback (configured at construction).
    #[test]
    fn recent_formatted_recovers_lines_scrolled_past_screen() {
        let mut vt = VirtualTerminal::new(24, 80);
        // 80 lines into a 24-row screen — last 24 visible, first 56 in
        // parser scrollback (we configured 200 rows of scrollback).
        for i in 1..=80 {
            vt.process(format!("line {i}\r\n").as_bytes());
        }
        let out = vt.rendered_recent_formatted(60);
        let plain = strip_escapes(&out);
        let lines: Vec<&str> = plain.lines().collect();
        assert!(
            lines.len() >= 55,
            "expected at least 55 recovered lines, got {} (lines={:?})",
            lines.len(),
            lines,
        );
        assert_eq!(
            *lines.last().unwrap(),
            "line 80",
            "tail must be the most recent line"
        );
    }

    #[test]
    fn recent_formatted_n_zero_returns_empty() {
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"hello\r\nworld\r\n");
        assert!(vt.rendered_recent_formatted(0).is_empty());
    }

    #[test]
    fn recent_formatted_short_session_returns_only_written_lines() {
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"a\r\nb\r\nc\r\n");
        let out = vt.rendered_recent_formatted(50);
        let plain = strip_escapes(&out);
        assert_eq!(plain, "a\nb\nc");
    }

    #[test]
    fn recent_formatted_preserves_sgr_colors() {
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"\x1b[31mred line\x1b[0m");
        let out = vt.rendered_recent_formatted(5);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("red line"), "missing text: {text:?}");
        assert!(
            text.contains("\x1b[") && (text.contains(";31") || text.contains("[31")),
            "expected SGR red code in {text:?}"
        );
    }

    #[test]
    fn recent_formatted_ends_with_reset() {
        let mut vt = VirtualTerminal::new(24, 80);
        vt.process(b"\x1b[31mred\x1b[0m");
        let out = vt.rendered_recent_formatted(5);
        assert!(
            out.ends_with(b"\x1b[0m"),
            "expected trailing reset, got tail: {:?}",
            &out[out.len().saturating_sub(8)..]
        );
    }
}
