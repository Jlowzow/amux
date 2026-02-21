mod client;
mod common;
mod daemon;
mod protocol;

use clap::{Parser, Subcommand};

use crate::protocol::messages::{ClientMessage, DaemonMessage};

#[derive(Parser)]
#[command(name = "amux", about = "AI Agent Multiplexer")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new session
    New {
        /// Target session name
        #[arg(short = 't', long = "target")]
        name: Option<String>,
        /// Start detached (don't attach after creation)
        #[arg(short, long)]
        detached: bool,
        /// Set environment variable (KEY=VALUE), can be specified multiple times
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
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
    /// List sessions
    Ls {
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
        /// Strip ANSI escape codes from output
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
enum EnvAction {
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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let command = cli.command.unwrap_or_else(|| {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        Command::New {
            name: None,
            detached: false,
            env: Vec::new(),
            cmd: vec![shell],
        }
    });

    match command {
        Command::StartServer => {
            if common::server_running() {
                eprintln!("amux: server is already running");
                return Ok(());
            }
            daemon::fork_daemon()?;
        }
        Command::KillServer { force } => {
            if force {
                // Kill all sessions first, then stop the server.
                let resp = client::request(&ClientMessage::KillAllSessions)?;
                match resp {
                    DaemonMessage::KilledSessions { count } => {
                        if count > 0 {
                            eprintln!("amux: killed {} session(s)", count);
                        }
                    }
                    DaemonMessage::Error(e) => {
                        eprintln!("amux: error killing sessions: {}", e);
                    }
                    _ => {}
                }
            } else {
                // Check if sessions are running; refuse if so.
                let resp = client::request(&ClientMessage::ListSessions)?;
                if let DaemonMessage::SessionList(sessions) = resp {
                    let alive: Vec<_> = sessions.iter().filter(|s| s.alive).collect();
                    if !alive.is_empty() {
                        eprintln!(
                            "amux: {} session(s) still running (use --force to kill them)",
                            alive.len()
                        );
                        std::process::exit(1);
                    }
                }
            }
            let resp = client::request(&ClientMessage::KillServer)?;
            match resp {
                DaemonMessage::Ok => eprintln!("amux: server stopped"),
                DaemonMessage::Error(e) => eprintln!("amux: error: {}", e),
                other => eprintln!("amux: unexpected response: {:?}", other),
            }
        }
        Command::Ping => {
            let resp = client::request(&ClientMessage::Ping)?;
            match resp {
                DaemonMessage::Pong => println!("pong"),
                other => eprintln!("amux: unexpected response: {:?}", other),
            }
        }
        Command::New {
            name,
            detached,
            env,
            cmd,
        } => {
            ensure_daemon_running()?;
            let env_map = parse_env_vars(&env)?;
            if detached {
                let resp = client::request(&ClientMessage::CreateSession {
                    name,
                    command: cmd,
                    env: env_map,
                })?;
                match resp {
                    DaemonMessage::SessionCreated { name } => {
                        eprintln!("amux: created session '{}'", name);
                    }
                    DaemonMessage::Error(e) => {
                        eprintln!("amux: error: {}", e);
                        std::process::exit(1);
                    }
                    other => eprintln!("amux: unexpected: {:?}", other),
                }
            } else {
                // Create then attach.
                let resp = client::request(&ClientMessage::CreateSession {
                    name: name.clone(),
                    command: cmd,
                    env: env_map,
                })?;
                let session_name = match resp {
                    DaemonMessage::SessionCreated { name } => name,
                    DaemonMessage::Error(e) => {
                        eprintln!("amux: error: {}", e);
                        std::process::exit(1);
                    }
                    other => {
                        eprintln!("amux: unexpected: {:?}", other);
                        std::process::exit(1);
                    }
                };
                do_attach(&session_name)?;
            }
        }
        Command::Attach { name } => {
            do_attach(&name)?;
        }
        Command::Ls { json } => {
            let resp = client::request(&ClientMessage::ListSessions)?;
            match resp {
                DaemonMessage::SessionList(sessions) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string(&sessions)
                                .unwrap_or_else(|e| format!("{{\"error\":\"{}\"}}", e))
                        );
                    } else if sessions.is_empty() {
                        eprintln!("no sessions");
                    } else {
                        for s in &sessions {
                            let status = if s.alive { "" } else { " (dead)" };
                            println!(
                                "{}: {} (pid {}, up {}s, created {}){}", s.name, s.command, s.pid, s.uptime_secs, s.created_at, status
                            );
                        }
                    }
                }
                DaemonMessage::Error(e) => eprintln!("amux: error: {}", e),
                other => eprintln!("amux: unexpected: {:?}", other),
            }
        }
        Command::Kill { name, all } => {
            if all {
                do_kill_all()?;
            } else {
                let name = name.unwrap();
                let resp =
                    client::request(&ClientMessage::KillSession { name: name.clone() })?;
                match resp {
                    DaemonMessage::Ok => eprintln!("amux: killed session '{}'", name),
                    DaemonMessage::Error(e) => eprintln!("amux: error: {}", e),
                    other => eprintln!("amux: unexpected: {:?}", other),
                }
            }
        }
        Command::KillAll => {
            do_kill_all()?;
        }
        Command::Send {
            name,
            literal,
            text,
        } => {
            let joined = text.join(" ");
            let resp = client::request(&ClientMessage::SendInput {
                name: name.clone(),
                data: joined.into_bytes(),
                newline: !literal,
            })?;
            match resp {
                DaemonMessage::InputSent => {}
                DaemonMessage::Error(e) => eprintln!("amux: error: {}", e),
                other => eprintln!("amux: unexpected: {:?}", other),
            }
        }
        Command::Has { name } => {
            let resp = client::request(&ClientMessage::HasSession { name });
            match resp {
                Ok(DaemonMessage::SessionExists(true)) => {
                    std::process::exit(0);
                }
                _ => {
                    std::process::exit(1);
                }
            }
        }
        Command::Capture { name, lines, plain } => {
            let resp = client::request(&ClientMessage::CaptureScrollback {
                name: name.clone(),
                lines,
            })?;
            match resp {
                DaemonMessage::CaptureOutput(data) => {
                    use std::io::Write;
                    let output = if plain {
                        strip_ansi_escapes::strip(&data)
                    } else {
                        data
                    };
                    std::io::stdout().write_all(&output)?;
                }
                DaemonMessage::Error(e) => eprintln!("amux: error: {}", e),
                other => eprintln!("amux: unexpected: {:?}", other),
            }
        }
        Command::Env { action } => match action {
            EnvAction::Set { name, key, value } => {
                let resp = client::request(&ClientMessage::SetEnv {
                    name: name.clone(),
                    key: key.clone(),
                    value,
                })?;
                match resp {
                    DaemonMessage::Ok => {}
                    DaemonMessage::Error(e) => {
                        eprintln!("amux: error: {}", e);
                        std::process::exit(1);
                    }
                    other => eprintln!("amux: unexpected: {:?}", other),
                }
            }
            EnvAction::Get { name, key } => {
                let resp = client::request(&ClientMessage::GetEnv {
                    name: name.clone(),
                    key: key.clone(),
                })?;
                match resp {
                    DaemonMessage::EnvValue(Some(val)) => println!("{}", val),
                    DaemonMessage::EnvValue(None) => {
                        std::process::exit(1);
                    }
                    DaemonMessage::Error(e) => {
                        eprintln!("amux: error: {}", e);
                        std::process::exit(1);
                    }
                    other => eprintln!("amux: unexpected: {:?}", other),
                }
            }
            EnvAction::List { name } => {
                let resp = client::request(&ClientMessage::GetAllEnv {
                    name: name.clone(),
                })?;
                match resp {
                    DaemonMessage::EnvVars(vars) => {
                        let mut keys: Vec<_> = vars.keys().collect();
                        keys.sort();
                        for k in keys {
                            println!("{}={}", k, vars[k]);
                        }
                    }
                    DaemonMessage::Error(e) => {
                        eprintln!("amux: error: {}", e);
                        std::process::exit(1);
                    }
                    other => eprintln!("amux: unexpected: {:?}", other),
                }
            }
        },
    }

    Ok(())
}

/// Kill all sessions via the daemon.
fn do_kill_all() -> anyhow::Result<()> {
    let resp = client::request(&ClientMessage::KillAllSessions)?;
    match resp {
        DaemonMessage::KilledSessions { count } => {
            eprintln!("amux: killed {} session(s)", count);
        }
        DaemonMessage::Error(e) => {
            eprintln!("amux: error: {}", e);
            std::process::exit(1);
        }
        other => eprintln!("amux: unexpected: {:?}", other),
    }
    Ok(())
}

/// Ensure the daemon is running, starting it if needed.
fn ensure_daemon_running() -> anyhow::Result<()> {
    if !common::server_running() {
        daemon::fork_daemon()?;
        // Wait briefly for the daemon to start.
        std::thread::sleep(std::time::Duration::from_millis(200));
        if !common::server_running() {
            anyhow::bail!("failed to start daemon");
        }
    }
    Ok(())
}

/// Attach to a named session.
fn do_attach(name: &str) -> anyhow::Result<()> {
    use crate::protocol::codec::write_frame;

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

    let mut stream =
        client::connect().context("is the server running?")?;

    // Send Attach message.
    write_frame(
        &mut stream,
        &ClientMessage::Attach {
            name: name.to_string(),
            cols,
            rows,
        },
    )?;

    // Switch to async for bidirectional streaming.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let std_stream = stream.into_raw_fd();
        let tokio_stream = unsafe {
            tokio::net::UnixStream::from_std(
                std::os::unix::net::UnixStream::from_raw_fd(std_stream),
            )?
        };
        let (mut reader, mut writer) = tokio_stream.into_split();
        client::attach::run_attach(&mut reader, &mut writer).await
    })
}

use std::os::unix::io::{FromRawFd, IntoRawFd};

use anyhow::Context;

/// Parse `-e KEY=VALUE` strings into an env map. Returns None if no vars specified.
fn parse_env_vars(
    vars: &[String],
) -> anyhow::Result<Option<std::collections::HashMap<String, String>>> {
    if vars.is_empty() {
        return Ok(None);
    }
    let mut map = std::collections::HashMap::new();
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
