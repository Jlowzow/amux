use std::collections::HashMap;

/// Strip ANSI escape sequences from raw bytes.
///
/// Handles CSI sequences (colors, cursor movement), OSC sequences (terminal title),
/// DCS/SOS/PM/APC string sequences, other Fe escape sequences, and single-byte
/// C1 control codes (0x80-0x9F) commonly found in terminal output.
pub(crate) fn strip_ansi(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == 0x1b {
            i += 1;
            if i >= input.len() {
                break;
            }
            match input[i] {
                b'[' => {
                    // CSI sequence: ESC [ (parameter bytes 0x30-0x3F)* (intermediate bytes 0x20-0x2F)* (final byte 0x40-0x7E)
                    i += 1;
                    skip_csi_params(input, &mut i);
                }
                b']' => {
                    // OSC sequence: ESC ] ... (terminated by BEL or ST)
                    i += 1;
                    skip_string_payload(input, &mut i);
                }
                b'P' | b'X' | b'^' | b'_' => {
                    // DCS (P), SOS (X), PM (^), APC (_): string sequences terminated by ST
                    i += 1;
                    skip_string_payload(input, &mut i);
                }
                0x40..=0x5F => {
                    // Other Fe escape sequences (ESC followed by 0x40-0x5F)
                    i += 1;
                }
                _ => {
                    // Unknown sequence after ESC, skip just the ESC
                }
            }
        } else if input[i] == 0x9B {
            // Single-byte CSI (C1 control code)
            i += 1;
            skip_csi_params(input, &mut i);
        } else if input[i] == 0x9D {
            // Single-byte OSC (C1 control code)
            i += 1;
            skip_string_payload(input, &mut i);
        } else if input[i] == 0x90 || input[i] == 0x98 || input[i] == 0x9E || input[i] == 0x9F {
            // Single-byte DCS (0x90), SOS (0x98), PM (0x9E), APC (0x9F)
            i += 1;
            skip_string_payload(input, &mut i);
        } else if (0x80..=0x9F).contains(&input[i]) {
            // Other C1 control codes — skip
            i += 1;
        } else {
            output.push(input[i]);
            i += 1;
        }
    }
    output
}

/// Skip CSI parameter bytes, intermediate bytes, and final byte.
fn skip_csi_params(input: &[u8], i: &mut usize) {
    while *i < input.len() && (0x30..=0x3F).contains(&input[*i]) {
        *i += 1;
    }
    while *i < input.len() && (0x20..=0x2F).contains(&input[*i]) {
        *i += 1;
    }
    if *i < input.len() && (0x40..=0x7E).contains(&input[*i]) {
        *i += 1;
    }
}

/// Skip a string payload terminated by BEL (0x07) or ST (ESC \ or 0x9C).
fn skip_string_payload(input: &[u8], i: &mut usize) {
    while *i < input.len() {
        if input[*i] == 0x07 || input[*i] == 0x9C {
            *i += 1;
            break;
        }
        if input[*i] == 0x1b && *i + 1 < input.len() && input[*i + 1] == b'\\' {
            *i += 2;
            break;
        }
        *i += 1;
    }
}

/// Process raw terminal output for clean plain-text rendering.
/// Handles: \r\n → \n normalization, bare \r (line overwrite), \x08 (backspace),
/// strips other control chars (except \n, \t), ensures trailing newline.
pub(crate) fn clean_control_chars(input: &[u8]) -> Vec<u8> {
    if input.is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<Vec<u8>> = vec![Vec::new()];

    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        match b {
            b'\r' => {
                if i + 1 < input.len() && input[i + 1] == b'\n' {
                    lines.push(Vec::new());
                    i += 2;
                } else {
                    let saved = std::mem::take(lines.last_mut().unwrap());
                    let mut cursor = 0usize;
                    let mut merged = saved;
                    i += 1;
                    while i < input.len() {
                        let c = input[i];
                        if c == b'\r' || c == b'\n' {
                            break;
                        }
                        if c == 0x08 {
                            if cursor > 0 {
                                cursor -= 1;
                            }
                            i += 1;
                            continue;
                        }
                        if c < 0x20 && c != b'\t' {
                            i += 1;
                            continue;
                        }
                        if cursor < merged.len() {
                            merged[cursor] = c;
                        } else {
                            merged.push(c);
                        }
                        cursor += 1;
                        i += 1;
                    }
                    *lines.last_mut().unwrap() = merged;
                }
            }
            b'\n' => {
                lines.push(Vec::new());
                i += 1;
            }
            0x08 => {
                lines.last_mut().unwrap().pop();
                i += 1;
            }
            0x07 | 0x7F => {
                // BEL and DEL — skip
                i += 1;
            }
            c if c < 0x20 && c != b'\t' => {
                i += 1;
            }
            _ => {
                lines.last_mut().unwrap().push(b);
                i += 1;
            }
        }
    }

    let mut output = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        output.extend_from_slice(line);
        if idx < lines.len() - 1 {
            output.push(b'\n');
        }
    }

    if !output.is_empty() && !output.ends_with(b"\n") {
        output.push(b'\n');
    }

    output
}

/// Parse `-e KEY=VALUE` strings into an env map. Returns None if no vars specified.
pub(crate) fn parse_env_vars(
    vars: &[String],
) -> anyhow::Result<Option<HashMap<String, String>>> {
    if vars.is_empty() {
        return Ok(None);
    }
    let mut map = HashMap::new();
    for var in vars {
        let (key, value) = var
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("invalid env var '{}': expected KEY=VALUE", var))?;
        if key.is_empty() {
            anyhow::bail!("invalid env var '{}': key cannot be empty", var);
        }
        map.insert(key.to_string(), value.to_string());
    }
    Ok(Some(map))
}

/// Create a git worktree for the given branch name.
/// Returns the absolute path to the new worktree directory.
pub(crate) fn create_git_worktree(branch: &str) -> anyhow::Result<String> {
    let toplevel = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !toplevel.status.success() {
        anyhow::bail!(
            "not a git repository (git rev-parse --show-toplevel failed)"
        );
    }
    let repo_root = String::from_utf8(toplevel.stdout)?.trim().to_string();

    let repo_name = std::path::Path::new(&repo_root)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let parent = std::path::Path::new(&repo_root)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/tmp"));
    let worktree_path = parent.join(format!("{}-{}", repo_name, branch));
    let worktree_str = worktree_path.to_string_lossy().to_string();

    let output = std::process::Command::new("git")
        .args(["worktree", "add", &worktree_str, "-b", branch])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree add failed: {}", stderr.trim());
    }

    eprintln!("amux: created worktree at {}", worktree_str);
    Ok(worktree_str)
}

/// Remove a git worktree directory.
#[allow(dead_code)]
pub(crate) fn remove_git_worktree(worktree_path: &str) -> anyhow::Result<()> {
    let output = std::process::Command::new("git")
        .args(["worktree", "remove", "--force", worktree_path])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree remove failed: {}", stderr.trim());
    }
    Ok(())
}

/// Ensure the daemon is running, starting it if needed.
pub(crate) fn ensure_daemon_running() -> anyhow::Result<()> {
    if !crate::common::server_running() {
        crate::daemon::fork_daemon()?;
        std::thread::sleep(std::time::Duration::from_millis(200));
        if !crate::common::server_running() {
            anyhow::bail!("failed to start daemon");
        }
    }
    Ok(())
}

/// Truncate a string to fit within `max_width`, adding "…" if truncated.
pub(crate) fn truncate(s: &str, max_width: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_width {
        s.to_string()
    } else if max_width <= 1 {
        s.chars().take(max_width).collect()
    } else {
        let truncated: String = s.chars().take(max_width - 1).collect();
        format!("{}…", truncated)
    }
}

/// Truncate a string to `max_width` *visible* characters while keeping ANSI
/// CSI (escape) sequences intact. SGR codes, which have zero visible width,
/// pass through verbatim; printable chars count toward the width budget. If
/// any visible chars are dropped, an ellipsis replaces the last kept char.
pub(crate) fn truncate_preserving_ansi(s: &str, max_width: usize) -> String {
    let mut visible_count = 0usize;
    // Walk char by char, tracking byte offsets so we can slice the original
    // string at a char boundary (the whole string is UTF-8).
    let mut char_iter = s.char_indices().peekable();
    let mut truncate_byte_end: Option<usize> = None;
    let mut last_visible_byte_range: Option<(usize, usize)> = None;

    while let Some(&(idx, ch)) = char_iter.peek() {
        if ch == '\x1b' {
            // Copy the whole escape sequence verbatim — determine its length.
            // Supports CSI (ESC [) with parameter/intermediate/final bytes.
            let esc_start = idx;
            char_iter.next();
            if let Some(&(_, next)) = char_iter.peek() {
                if next == '[' {
                    char_iter.next();
                    // Scan params (0x30-0x3F) + intermediates (0x20-0x2F) + final (0x40-0x7E).
                    while let Some(&(_, c)) = char_iter.peek() {
                        let b = c as u32;
                        if (0x30..=0x3F).contains(&b) {
                            char_iter.next();
                        } else {
                            break;
                        }
                    }
                    while let Some(&(_, c)) = char_iter.peek() {
                        let b = c as u32;
                        if (0x20..=0x2F).contains(&b) {
                            char_iter.next();
                        } else {
                            break;
                        }
                    }
                    if let Some(&(_, c)) = char_iter.peek() {
                        let b = c as u32;
                        if (0x40..=0x7E).contains(&b) {
                            char_iter.next();
                        }
                    }
                } else {
                    // ESC followed by a single char — consume both.
                    char_iter.next();
                }
            }
            let _ = esc_start; // bounds tracked via char_iter
            continue;
        }
        // A visible character.
        if visible_count >= max_width {
            truncate_byte_end = Some(idx);
            break;
        }
        visible_count += 1;
        let char_end = idx + ch.len_utf8();
        last_visible_byte_range = Some((idx, char_end));
        char_iter.next();
    }

    match truncate_byte_end {
        None => s.to_string(),
        Some(end) => {
            // We dropped at least one visible char. Replace the last visible
            // char with an ellipsis (when there's budget to do so).
            if max_width == 0 {
                return String::new();
            }
            if let Some((last_start, _)) = last_visible_byte_range {
                let mut out = String::with_capacity(last_start + 3);
                out.push_str(&s[..last_start]);
                out.push('…');
                // Reset any lingering SGR introduced before the drop.
                // The caller already appends a final reset, so we don't
                // emit one here to avoid double-resets.
                out
            } else {
                // No visible chars fit — return empty, minus any escapes.
                let _ = end;
                String::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{strip_ansi, clean_control_chars, truncate, truncate_preserving_ansi};

    #[test]
    fn test_strip_ansi_plain_text() {
        let input = b"hello world";
        assert_eq!(strip_ansi(input), b"hello world");
    }

    #[test]
    fn test_strip_ansi_empty() {
        assert_eq!(strip_ansi(b""), b"");
    }

    #[test]
    fn test_strip_ansi_sgr_colors() {
        let input = b"\x1b[31mhello\x1b[0m world";
        assert_eq!(strip_ansi(input), b"hello world");
    }

    #[test]
    fn test_strip_ansi_cursor_movement() {
        let input = b"\x1b[2J\x1b[Hprompt$ ";
        assert_eq!(strip_ansi(input), b"prompt$ ");
    }

    #[test]
    fn test_strip_ansi_osc_bel_terminated() {
        let input = b"\x1b]0;my terminal\x07prompt$ ";
        assert_eq!(strip_ansi(input), b"prompt$ ");
    }

    #[test]
    fn test_strip_ansi_osc_st_terminated() {
        let input = b"\x1b]0;my terminal\x1b\\prompt$ ";
        assert_eq!(strip_ansi(input), b"prompt$ ");
    }

    #[test]
    fn test_strip_ansi_complex_csi() {
        let input = b"\x1b[?2004h\x1b[1;32muser@host\x1b[0m:~$ ";
        assert_eq!(strip_ansi(input), b"user@host:~$ ");
    }

    #[test]
    fn test_strip_ansi_preserves_newlines() {
        let input = b"\x1b[32mline1\x1b[0m\nline2\n";
        assert_eq!(strip_ansi(input), b"line1\nline2\n");
    }

    #[test]
    fn test_strip_ansi_mixed_content() {
        let input = b"\x1b[1m\x1b[7m%\x1b[27m\x1b[1m\x1b[0m \r \r\x1b[0m\x1b[27m\x1b[24m$ echo ALIVE\r\nALIVE\r\n";
        let result = strip_ansi(input);
        let result_str = String::from_utf8_lossy(&result);
        assert!(result_str.contains("ALIVE"));
        assert!(!result.contains(&0x1b));
    }

    #[test]
    fn test_clean_control_chars_normalizes_crlf() {
        assert_eq!(clean_control_chars(b"hello\r\nworld\r\n"), b"hello\nworld\n");
    }

    #[test]
    fn test_clean_control_chars_bare_cr_overwrites() {
        assert_eq!(clean_control_chars(b"abcde\rXY"), b"XYcde\n");
    }

    #[test]
    fn test_clean_control_chars_strips_non_printable() {
        let input = b"hello\x07world\x08!";
        assert_eq!(clean_control_chars(input), b"helloworl!\n");
    }

    #[test]
    fn test_clean_control_chars_backspace() {
        assert_eq!(clean_control_chars(b"abc\x08\x08XY"), b"aXY\n");
    }

    #[test]
    fn test_clean_control_chars_ensures_trailing_newline() {
        assert_eq!(clean_control_chars(b"hello"), b"hello\n");
        assert_eq!(clean_control_chars(b"hello\n"), b"hello\n");
    }

    #[test]
    fn test_clean_control_chars_empty() {
        assert_eq!(clean_control_chars(b""), b"");
    }

    #[test]
    fn test_clean_control_chars_preserves_tabs() {
        assert_eq!(clean_control_chars(b"a\tb\n"), b"a\tb\n");
    }

    // --- strip_ansi: C1 control code handling ---

    #[test]
    fn test_strip_ansi_c1_single_byte_csi() {
        // 0x9B is single-byte CSI (equivalent to ESC [)
        let input = b"\x9b31mhello\x9b0m";
        assert_eq!(strip_ansi(input), b"hello");
    }

    #[test]
    fn test_strip_ansi_c1_single_byte_osc() {
        // 0x9D is single-byte OSC (equivalent to ESC ])
        let input = b"\x9d0;title\x07hello";
        assert_eq!(strip_ansi(input), b"hello");
    }

    #[test]
    fn test_strip_ansi_c1_dcs_sequence() {
        // ESC P ... ESC \ (DCS with ST terminator)
        let input = b"\x1bPsome data\x1b\\hello";
        assert_eq!(strip_ansi(input), b"hello");
    }

    #[test]
    fn test_strip_ansi_c1_other_bytes() {
        // Other C1 bytes (0x80-0x9F) that aren't CSI/OSC/DCS/SOS/PM/APC should be stripped
        let mut input = vec![0x80, 0x81, 0x84, 0x85];
        input.extend_from_slice(b"hello");
        input.push(0x86);
        assert_eq!(strip_ansi(&input), b"hello");
    }

    // --- clean_control_chars: DEL and high-byte handling ---

    #[test]
    fn test_clean_control_chars_strips_del() {
        assert_eq!(clean_control_chars(b"hel\x7Flo"), b"hello\n");
    }

    #[test]
    fn test_truncate_short_string() {
        assert_eq!(truncate("hello", 60), "hello");
    }

    #[test]
    fn test_truncate_exact_length() {
        let s = "a".repeat(60);
        assert_eq!(truncate(&s, 60), s);
    }

    #[test]
    fn test_truncate_long_string() {
        let s = "a".repeat(100);
        let result = truncate(&s, 60);
        assert_eq!(result.len(), 62); // 59 chars + 3-byte '…'
        assert!(result.ends_with('…'));
        assert_eq!(result.chars().count(), 60); // 59 + ellipsis = 60 display chars
    }

    #[test]
    fn test_truncate_very_small_max() {
        assert_eq!(truncate("hello", 3), "he…");
        assert_eq!(truncate("hello", 1), "h");
    }

    // --- truncate_preserving_ansi ---

    #[test]
    fn test_truncate_preserving_ansi_no_escapes_short() {
        assert_eq!(truncate_preserving_ansi("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_preserving_ansi_no_escapes_long() {
        let result = truncate_preserving_ansi("hello world", 5);
        // 4 visible chars + "…"
        assert_eq!(result, "hell…");
    }

    #[test]
    fn test_truncate_preserving_ansi_keeps_sgr_intact() {
        // SGR codes have zero visible width and must survive truncation.
        let input = "\x1b[31mhello\x1b[0m world";
        let result = truncate_preserving_ansi(input, 5);
        // Visible is "hello world" (11 chars); truncated to 5 -> "hell…",
        // with the opening red SGR code preserved verbatim.
        assert!(result.starts_with("\x1b[31m"), "got: {:?}", result);
        assert!(result.ends_with('…'), "got: {:?}", result);
        // Should contain 4 visible chars from "hello".
        assert!(result.contains("hell"), "got: {:?}", result);
    }

    #[test]
    fn test_truncate_preserving_ansi_below_limit_preserves_all() {
        let input = "\x1b[1;32mhi\x1b[0m";
        // Visible is 2 chars "hi" — fits in 10, so full input returned.
        assert_eq!(truncate_preserving_ansi(input, 10), input);
    }

    #[test]
    fn test_truncate_preserving_ansi_multiple_sgr_segments() {
        // Two colored segments totalling 6 visible chars, truncated to 4.
        let input = "\x1b[31mABC\x1b[0m\x1b[32mXYZ\x1b[0m";
        let result = truncate_preserving_ansi(input, 4);
        // First three visible chars ABC (with red SGR in front), then an
        // ellipsis replaces 4th visible char (X). The second SGR code
        // appearing between ABC and XYZ should be copied through because
        // it sits between visible chars.
        assert!(result.starts_with("\x1b[31m"), "got: {:?}", result);
        assert!(result.contains("ABC"), "got: {:?}", result);
        assert!(result.ends_with('…'), "got: {:?}", result);
        // The "XYZ" must NOT be present — it's all past the budget.
        assert!(!result.contains('X'), "got: {:?}", result);
    }

    #[test]
    fn test_truncate_preserving_ansi_zero_width() {
        assert_eq!(truncate_preserving_ansi("hello", 0), "");
    }

    #[test]
    fn test_truncate_preserving_ansi_unicode_counts_as_one() {
        // Unicode box-drawing chars occupy one column each.
        let input = "│ foo │ bar";
        let result = truncate_preserving_ansi(input, 5);
        // 4 visible chars + ellipsis.
        assert_eq!(result.chars().count(), 5);
        assert!(result.ends_with('…'));
    }
}
