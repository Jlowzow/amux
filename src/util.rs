use std::collections::HashMap;

/// Strip ANSI escape sequences from raw bytes.
///
/// Handles CSI sequences (colors, cursor movement), OSC sequences (terminal title),
/// and other Fe escape sequences commonly found in terminal output.
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
                    while i < input.len() && (0x30..=0x3F).contains(&input[i]) {
                        i += 1;
                    }
                    while i < input.len() && (0x20..=0x2F).contains(&input[i]) {
                        i += 1;
                    }
                    if i < input.len() && (0x40..=0x7E).contains(&input[i]) {
                        i += 1;
                    }
                }
                b']' => {
                    // OSC sequence: ESC ] ... (terminated by BEL or ST)
                    i += 1;
                    while i < input.len() {
                        if input[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if input[i] == 0x1b && i + 1 < input.len() && input[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                0x40..=0x5F => {
                    // Other Fe escape sequences (ESC followed by 0x40-0x5F)
                    i += 1;
                }
                _ => {
                    // Unknown sequence after ESC, skip just the ESC
                }
            }
        } else {
            output.push(input[i]);
            i += 1;
        }
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

#[cfg(test)]
mod tests {
    use super::strip_ansi;

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
}
