use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "amux", about = "AI Agent Multiplexer", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
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
        /// Strip ANSI escape sequences from output
        #[arg(long)]
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
    /// Ping the server (health check)
    Ping,
}

#[derive(Subcommand)]
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
}
