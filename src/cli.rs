use clap::{Parser, Subcommand};

/// Minimum allowed `--rows` value. Anything smaller is rejected; many TUIs
/// behave badly below ~10 rows.
pub const MIN_ROWS: u16 = 10;
/// Maximum allowed `--rows` value. Way above any real terminal; mostly a
/// guard against accidental huge values blowing memory in vterm grids.
pub const MAX_ROWS: u16 = 500;

fn parse_rows(s: &str) -> Result<u16, String> {
    let n: u16 = s.parse().map_err(|_| format!("'{}' is not a valid row count", s))?;
    if n < MIN_ROWS || n > MAX_ROWS {
        return Err(format!(
            "rows must be in [{}, {}], got {}",
            MIN_ROWS, MAX_ROWS, n
        ));
    }
    Ok(n)
}

#[derive(Parser)]
#[command(name = "amux", about = "AI Agent Multiplexer", version)]
pub struct Cli {
    /// Select an amux instance: every runtime file (socket, pid, log)
    /// lives under /tmp/amux-{uid}-{instance}, giving this invocation
    /// its own daemon and session registry. Equivalent to setting the
    /// `AMUX_INSTANCE` env var; the flag wins when both are present.
    /// Use this to run multiple orchestrators side-by-side without
    /// their sessions showing up in each other's `amux ls`.
    #[arg(short = 'I', long = "instance", global = true)]
    pub instance: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a new session
    New {
        /// Session name (human-readable identifier)
        #[arg(short = 'n', long = "name", visible_alias = "target", short_alias = 't')]
        name: Option<String>,
        /// Start detached (don't attach after creation)
        #[arg(short, long)]
        detached: bool,
        /// Set environment variable (KEY=VALUE), can be specified multiple times
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Working directory for the session
        #[arg(short = 'c', long = "cwd")]
        cwd: Option<String>,
        /// Create a git worktree and run the session in it
        #[arg(short = 'w', long = "worktree")]
        worktree: Option<String>,
        /// Send an initial message after the session is ready (implies --detached)
        #[arg(short = 'm', long = "init-message")]
        init_message: Option<String>,
        /// Initial PTY rows for the session (clamped to [10, 500]).
        /// When omitted, the session spawns at the invoking terminal's
        /// size (or 80x24 if amux was invoked without a tty). `amux top`
        /// later resizes the PTY to match its own viewer's terminal when
        /// no client is attached — so the spawn-time size is just a
        /// starting point, not a ceiling.
        #[arg(short = 'r', long = "rows", value_parser = parse_rows)]
        rows: Option<u16>,
        /// Command to run
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Attach to a session
    #[command(alias = "a")]
    Attach {
        /// Target session name
        #[arg(short = 't', long = "target")]
        name: String,
    },
    /// Follow session output (read-only streaming, no stdin)
    Follow {
        /// Target session name
        #[arg(short = 't', long = "target")]
        name: String,
        /// Output raw terminal bytes (ANSI/control chars included)
        #[arg(long)]
        raw: bool,
        /// Strip ANSI escape sequences (default behavior, kept for backwards compat)
        #[arg(long, hide = true)]
        plain: bool,
    },
    /// List sessions
    Ls {
        /// Output in JSON format
        #[arg(long)]
        json: bool,
    },
    /// Get detailed info for a single session
    Info {
        /// Target session name
        #[arg(short = 't', long = "target")]
        name: String,
        /// Output in JSON format
        #[arg(long)]
        json: bool,
    },
    /// Wait for a session to exit
    Wait {
        /// Target session name (single session mode)
        #[arg(short = 't', long = "target", required_unless_present = "any")]
        name: Option<String>,
        /// Wait for any of the given sessions to exit
        #[arg(long, num_args = 1.., value_name = "SESSION")]
        any: Vec<String>,
        /// Timeout in seconds (0 = wait forever)
        #[arg(long, default_value = "0")]
        timeout: u64,
        /// Print the exit code after the session exits
        #[arg(long)]
        exit_code: bool,
    },
    /// Watch multiple sessions and print exit events as they occur
    Watch {
        /// Session names to watch
        #[arg(required = true)]
        sessions: Vec<String>,
        /// Output in JSON format
        #[arg(long)]
        json: bool,
        /// Shell command to run when a session exits.
        /// Template variables: {name}, {code}, {pid}, {duration}
        #[arg(long)]
        on_exit: Option<String>,
    },
    /// Kill a session (or all sessions with --all)
    Kill {
        /// Target session name
        #[arg(short = 't', long = "target", required_unless_present = "all")]
        name: Option<String>,
        /// Kill all sessions
        #[arg(long)]
        all: bool,
    },
    /// Kill all sessions
    KillAll,
    /// Send keys to a session
    Send {
        /// Target session name
        #[arg(short = 't', long = "target")]
        name: String,
        /// Send literal text without trailing newline
        #[arg(short = 'l', long = "literal")]
        literal: bool,
        /// Text to send
        text: Vec<String>,
    },
    /// Check if a session exists (exit 0 if yes, 1 if no)
    Has {
        /// Target session name
        #[arg(short = 't', long = "target")]
        name: String,
    },
    /// Capture scrollback from a session
    Capture {
        /// Target session name
        #[arg(short = 't', long = "target")]
        name: String,
        /// Number of lines to dump
        #[arg(short, long, default_value = "50")]
        lines: usize,
        /// Output raw terminal bytes (ANSI/control chars included)
        #[arg(long)]
        raw: bool,
        /// Strip ANSI escape sequences (default behavior, kept for backwards compat)
        #[arg(long, hide = true)]
        plain: bool,
    },
    /// Get or set session-level environment variables
    Env {
        #[command(subcommand)]
        action: EnvAction,
    },
    /// Start the daemon server
    StartServer,
    /// Stop daemon (use --force to kill sessions first)
    KillServer {
        /// Kill all sessions before stopping the server
        #[arg(short, long)]
        force: bool,
    },
    /// Live TUI dashboard showing all sessions
    Top {
        /// Print a single snapshot and exit (no TUI)
        #[arg(long)]
        once: bool,
    },
    /// Ping the server (health check)
    Ping,
}

#[derive(Subcommand, Debug)]
pub enum EnvAction {
    /// Set an environment variable on a session
    Set {
        /// Target session name
        #[arg(short = 't', long = "target")]
        name: String,
        /// Variable name
        key: String,
        /// Variable value
        value: String,
    },
    /// Get an environment variable from a session
    Get {
        /// Target session name
        #[arg(short = 't', long = "target")]
        name: String,
        /// Variable name
        key: String,
    },
    /// List all environment variables on a session
    #[command(alias = "ls")]
    List {
        /// Target session name
        #[arg(short = 't', long = "target")]
        name: String,
    },
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    #[test]
    fn test_version_flag_recognized() {
        let result = super::Cli::try_parse_from(["amux", "--version"]);
        match result {
            Err(e) => assert_eq!(e.kind(), clap::error::ErrorKind::DisplayVersion),
            Ok(_) => panic!("expected --version to produce DisplayVersion error"),
        }
    }

    #[test]
    fn test_new_init_message_flag() {
        let cli = super::Cli::try_parse_from([
            "amux",
            "new",
            "--name",
            "worker",
            "--detached",
            "--init-message",
            "Hello world",
            "--",
            "bash",
        ])
        .unwrap();
        match cli.command.unwrap() {
            super::Command::New {
                name,
                detached,
                init_message,
                cmd,
                ..
            } => {
                assert_eq!(name.as_deref(), Some("worker"));
                assert!(detached);
                assert_eq!(init_message.as_deref(), Some("Hello world"));
                assert_eq!(cmd, vec!["bash"]);
            }
            other => panic!("expected New, got {:?}", other),
        }
    }

    #[test]
    fn test_capture_defaults_to_plain() {
        let cli = super::Cli::try_parse_from(["amux", "capture", "-t", "mysession"]).unwrap();
        match cli.command.unwrap() {
            super::Command::Capture { name, raw, .. } => {
                assert_eq!(name, "mysession");
                assert!(!raw, "capture should default to plain (raw=false)");
            }
            other => panic!("expected Capture, got {:?}", other),
        }
    }

    #[test]
    fn test_capture_raw_flag() {
        let cli =
            super::Cli::try_parse_from(["amux", "capture", "-t", "mysession", "--raw"]).unwrap();
        match cli.command.unwrap() {
            super::Command::Capture { name, raw, .. } => {
                assert_eq!(name, "mysession");
                assert!(raw, "capture --raw should set raw=true");
            }
            other => panic!("expected Capture, got {:?}", other),
        }
    }

    #[test]
    fn test_capture_plain_compat() {
        let cli =
            super::Cli::try_parse_from(["amux", "capture", "-t", "mysession", "--plain"]).unwrap();
        match cli.command.unwrap() {
            super::Command::Capture { raw, .. } => {
                assert!(!raw, "--plain should not set raw");
            }
            other => panic!("expected Capture, got {:?}", other),
        }
    }

    #[test]
    fn test_new_without_init_message() {
        let cli =
            super::Cli::try_parse_from(["amux", "new", "--detached", "--", "bash"]).unwrap();
        match cli.command.unwrap() {
            super::Command::New { init_message, .. } => {
                assert!(init_message.is_none());
            }
            other => panic!("expected New, got {:?}", other),
        }
    }

    #[test]
    fn test_new_rows_flag_long() {
        let cli = super::Cli::try_parse_from([
            "amux", "new", "--rows", "100", "--detached", "--", "bash",
        ])
        .unwrap();
        match cli.command.unwrap() {
            super::Command::New { rows, .. } => assert_eq!(rows, Some(100)),
            other => panic!("expected New, got {:?}", other),
        }
    }

    #[test]
    fn test_new_rows_flag_short() {
        let cli = super::Cli::try_parse_from([
            "amux", "new", "-r", "80", "--detached", "--", "bash",
        ])
        .unwrap();
        match cli.command.unwrap() {
            super::Command::New { rows, .. } => assert_eq!(rows, Some(80)),
            other => panic!("expected New, got {:?}", other),
        }
    }

    #[test]
    fn test_new_rows_default_is_none() {
        let cli =
            super::Cli::try_parse_from(["amux", "new", "--detached", "--", "bash"]).unwrap();
        match cli.command.unwrap() {
            // None at the CLI level — new_session() applies the 60 default
            // for detached sessions.
            super::Command::New { rows, .. } => assert_eq!(rows, None),
            other => panic!("expected New, got {:?}", other),
        }
    }

    #[test]
    fn test_new_rows_below_clamp_rejected() {
        let result = super::Cli::try_parse_from([
            "amux", "new", "--rows", "5", "--detached", "--", "bash",
        ]);
        let err = match result {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected --rows clamp to fail"),
        };
        assert!(
            err.contains("rows must be in"),
            "expected clamp error, got: {}",
            err
        );
    }

    #[test]
    fn test_new_rows_above_clamp_rejected() {
        let result = super::Cli::try_parse_from([
            "amux", "new", "--rows", "501", "--detached", "--", "bash",
        ]);
        let err = match result {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected --rows clamp to fail"),
        };
        assert!(
            err.contains("rows must be in"),
            "expected clamp error, got: {}",
            err
        );
    }

    #[test]
    fn test_new_rows_at_min_accepted() {
        let cli = super::Cli::try_parse_from([
            "amux",
            "new",
            "--rows",
            &super::MIN_ROWS.to_string(),
            "--detached",
            "--",
            "bash",
        ])
        .unwrap();
        match cli.command.unwrap() {
            super::Command::New { rows, .. } => assert_eq!(rows, Some(super::MIN_ROWS)),
            other => panic!("expected New, got {:?}", other),
        }
    }

    #[test]
    fn test_new_rows_at_max_accepted() {
        let cli = super::Cli::try_parse_from([
            "amux",
            "new",
            "--rows",
            &super::MAX_ROWS.to_string(),
            "--detached",
            "--",
            "bash",
        ])
        .unwrap();
        match cli.command.unwrap() {
            super::Command::New { rows, .. } => assert_eq!(rows, Some(super::MAX_ROWS)),
            other => panic!("expected New, got {:?}", other),
        }
    }

}
