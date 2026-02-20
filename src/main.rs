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
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new session
    New {
        /// Session name
        #[arg(short = 's', long = "session")]
        name: Option<String>,
        /// Start detached (don't attach after creation)
        #[arg(short, long)]
        detached: bool,
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
    Ls,
    /// Kill a session
    Kill {
        /// Target session name
        #[arg(short = 't', long = "target")]
        name: String,
    },
    /// Send text to a session
    Send {
        /// Target session name
        #[arg(short = 't', long = "target")]
        name: String,
        /// Text to send (newline appended)
        text: String,
    },
    /// Check if a session exists (exit 0 if yes, 1 if no)
    Has {
        /// Target session name
        #[arg(short = 't', long = "target")]
        name: String,
    },
    /// Start the daemon server
    StartServer,
    /// Stop daemon and all sessions
    KillServer,
    /// Ping the server (health check)
    Ping,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::StartServer => {
            if common::server_running() {
                eprintln!("amux: server is already running");
                return Ok(());
            }
            daemon::fork_daemon()?;
        }
        Command::KillServer => {
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
            cmd,
        } => {
            ensure_daemon_running()?;
            if detached {
                let resp = client::request(&ClientMessage::CreateSession {
                    name,
                    command: cmd,
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
        Command::Ls => {
            let resp = client::request(&ClientMessage::ListSessions)?;
            match resp {
                DaemonMessage::SessionList(sessions) => {
                    if sessions.is_empty() {
                        eprintln!("no sessions");
                    } else {
                        for s in &sessions {
                            let status = if s.alive { "" } else { " (dead)" };
                            println!(
                                "{}: {} (pid {}){}", s.name, s.command, s.pid, status
                            );
                        }
                    }
                }
                DaemonMessage::Error(e) => eprintln!("amux: error: {}", e),
                other => eprintln!("amux: unexpected: {:?}", other),
            }
        }
        Command::Kill { name } => {
            let resp = client::request(&ClientMessage::KillSession { name: name.clone() })?;
            match resp {
                DaemonMessage::Ok => eprintln!("amux: killed session '{}'", name),
                DaemonMessage::Error(e) => eprintln!("amux: error: {}", e),
                other => eprintln!("amux: unexpected: {:?}", other),
            }
        }
        Command::Send { name, text } => {
            let text_with_newline = format!("{}\n", text);
            let resp = client::request(&ClientMessage::SendText {
                name: name.clone(),
                text: text_with_newline,
            })?;
            match resp {
                DaemonMessage::Ok => {}
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
