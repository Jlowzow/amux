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

/// Final sanitization pass: only allow printable ASCII (0x20-0x7E), tab, and newline.
/// Strips any remaining control characters, DEL, and non-ASCII bytes that may have
/// leaked through strip_ansi + clean_control_chars. This is the safety net that
/// prevents raw PTY output from corrupting the terminal display.
pub(crate) fn sanitize_for_display(input: &[u8]) -> Vec<u8> {
    input
        .iter()
        .copied()
        .filter(|&b| b == b'\n' || b == b'\t' || (b >= 0x20 && b < 0x7F))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::{strip_ansi, clean_control_chars, sanitize_for_display, truncate};

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

    // --- sanitize_for_display: final safety net ---

    #[test]
    fn test_sanitize_for_display_allows_printable() {
        assert_eq!(sanitize_for_display(b"hello world\n"), b"hello world\n");
    }

    #[test]
    fn test_sanitize_for_display_allows_tabs() {
        assert_eq!(sanitize_for_display(b"a\tb\n"), b"a\tb\n");
    }

    #[test]
    fn test_sanitize_for_display_strips_control_chars() {
        assert_eq!(sanitize_for_display(b"he\x01ll\x02o"), b"hello");
    }

    #[test]
    fn test_sanitize_for_display_strips_high_bytes() {
        let input = b"hello\x80\x9b\xffworld";
        assert_eq!(sanitize_for_display(input), b"helloworld");
    }

    #[test]
    fn test_sanitize_for_display_strips_del() {
        assert_eq!(sanitize_for_display(b"hel\x7Flo"), b"hello");
    }

    #[test]
    fn test_sanitize_full_pipeline_heavy_tui() {
        // Simulate heavy TUI output: alternate screen, cursor movements, SGR, DCS, C1, etc.
        let mut input: Vec<u8> = Vec::new();
        input.extend_from_slice(b"\x1b[?1049h");         // alternate screen on
        input.extend_from_slice(b"\x1b[2J\x1b[H");       // clear + home
        input.extend_from_slice(b"\x1b[1;32mHello\x1b[0m"); // colored text
        input.extend_from_slice(b"\r\n");
        input.extend_from_slice(b"\x1b[?2004h");          // bracketed paste on
        input.extend_from_slice(b"\x9b38;5;196m");        // C1 CSI color
        input.extend_from_slice(b"World");
        input.extend_from_slice(b"\x1b[?1049l");          // alternate screen off
        input.push(0x7F);                                  // DEL
        input.push(0x90);                                  // C1 DCS...
        input.extend_from_slice(b"payload\x1b\\");         // ...terminated by ST
        input.extend_from_slice(b"\r\nDone\n");

        let stripped = strip_ansi(&input);
        let cleaned = clean_control_chars(&stripped);
        let safe = sanitize_for_display(&cleaned);
        let text = String::from_utf8(safe).expect("should be valid UTF-8");
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(text.contains("Done"));
        // No escape or control chars should remain
        for b in text.bytes() {
            assert!(
                b == b'\n' || b == b'\t' || (b >= 0x20 && b < 0x7F),
                "unexpected byte 0x{:02X} in sanitized output",
                b
            );
        }
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
}
