mod client;
mod common;
mod daemon;
mod protocol;

use clap::{Parser, Subcommand};

use crate::protocol::messages::{ClientMessage, DaemonMessage};

#[derive(Parser)]
#[command(name = "amux", about = "AI Agent Multiplexer", version)]
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
        /// Working directory for the session
        #[arg(short = 'c', long = "cwd")]
        cwd: Option<String>,
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
        /// Target session name
        #[arg(short = 't', long = "target")]
        name: String,
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
            cwd: None,
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
                DaemonMessage::Error(e) => {
                    eprintln!("amux: error: {}", e);
                    std::process::exit(1);
                }
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
            cwd,
            cmd,
        } => {
            if std::env::var("AMUX_DEBUG").is_ok() {
                eprintln!("amux-debug: ensure_daemon_running");
            }
            ensure_daemon_running()?;
            if std::env::var("AMUX_DEBUG").is_ok() {
                eprintln!("amux-debug: daemon running, parsing env");
            }
            let env_map = parse_env_vars(&env)?;
            if detached {
                let resp = client::request(&ClientMessage::CreateSession {
                    name,
                    command: cmd,
                    env: env_map,
                    cwd: cwd.clone(),
                    cols: None,
                    rows: None,
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
                if std::env::var("AMUX_DEBUG").is_ok() {
                    eprintln!("amux-debug: creating session (non-detached)");
                }
                let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
                let resp = client::request(&ClientMessage::CreateSession {
                    name: name.clone(),
                    command: cmd,
                    env: env_map,
                    cwd,
                    cols: Some(term_cols),
                    rows: Some(term_rows),
                })?;
                let session_name = match resp {
                    DaemonMessage::SessionCreated { name } => {
                        if std::env::var("AMUX_DEBUG").is_ok() {
                            eprintln!("amux-debug: session created: '{}', calling do_attach", name);
                        }
                        name
                    }
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
            ensure_daemon_running()?;
            do_attach(&name)?;
        }
        Command::Follow { name } => {
            ensure_daemon_running()?;
            do_follow(&name)?;
        }
        Command::Ls { json } => {
            ensure_daemon_running()?;
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
                                "{}: {} (pid {}, up {}s, idle {}s, created {}){}", s.name, s.command, s.pid, s.uptime_secs, s.idle_secs, s.created_at, status
                            );
                        }
                    }
                }
                DaemonMessage::Error(e) => {
                    eprintln!("amux: error: {}", e);
                    std::process::exit(1);
                }
                other => eprintln!("amux: unexpected: {:?}", other),
            }
        }
        Command::Info { name, json } => {
            ensure_daemon_running()?;
            let resp = client::request(&ClientMessage::GetSessionInfo {
                name: name.clone(),
            })?;
            match resp {
                DaemonMessage::SessionDetail(info) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string(&info)
                                .unwrap_or_else(|e| format!("{{\"error\":\"{}\"}}", e))
                        );
                    } else {
                        let status = if info.alive { "alive" } else { "dead" };
                        println!("name: {}", info.name);
                        println!("command: {}", info.command);
                        println!("pid: {}", info.pid);
                        println!("status: {}", status);
                        println!("created: {}", info.created_at);
                        println!("uptime: {}s", info.uptime_secs);
                        println!("last_activity: {}", info.last_activity);
                        println!("idle: {}s", info.idle_secs);
                    }
                }
                DaemonMessage::Error(e) => {
                    eprintln!("amux: error: {}", e);
                    std::process::exit(1);
                }
                other => eprintln!("amux: unexpected: {:?}", other),
            }
        }
        Command::Wait {
            name,
            timeout,
            exit_code,
        } => {
            ensure_daemon_running()?;
            let resp = client::request(&ClientMessage::WaitSession {
                name: name.clone(),
                timeout_secs: timeout,
            })?;
            match resp {
                DaemonMessage::SessionExited => {
                    if exit_code {
                        let resp =
                            client::request(&ClientMessage::GetExitCode { name: name.clone() })?;
                        match resp {
                            DaemonMessage::ExitCode(Some(code)) => {
                                println!("{}", code);
                                std::process::exit(code);
                            }
                            DaemonMessage::ExitCode(None) => {
                                eprintln!("amux: exit code unavailable for session '{}'", name);
                                std::process::exit(1);
                            }
                            DaemonMessage::Error(e) => {
                                eprintln!("amux: error: {}", e);
                                std::process::exit(1);
                            }
                            other => eprintln!("amux: unexpected: {:?}", other),
                        }
                    }
                }
                DaemonMessage::Error(e) => {
                    if e == "timeout" {
                        eprintln!("amux: wait timed out for session '{}'", name);
                        std::process::exit(2);
                    }
                    eprintln!("amux: error: {}", e);
                    std::process::exit(1);
                }
                other => eprintln!("amux: unexpected: {:?}", other),
            }
        }
        Command::Watch { sessions, json } => {
            ensure_daemon_running()?;
            do_watch(&sessions, json)?;
        }
        Command::Kill { name, all } => {
            ensure_daemon_running()?;
            if all {
                do_kill_all()?;
            } else {
                let name = name.unwrap();
                let resp =
                    client::request(&ClientMessage::KillSession { name: name.clone() })?;
                match resp {
                    DaemonMessage::Ok => eprintln!("amux: killed session '{}'", name),
                    DaemonMessage::Error(e) => {
                        eprintln!("amux: error: {}", e);
                        std::process::exit(1);
                    }
                    other => eprintln!("amux: unexpected: {:?}", other),
                }
            }
        }
        Command::KillAll => {
            ensure_daemon_running()?;
            do_kill_all()?;
        }
        Command::Send {
            name,
            literal,
            text,
        } => {
            ensure_daemon_running()?;
            let joined = text.join(" ");
            let resp = client::request(&ClientMessage::SendInput {
                name: name.clone(),
                data: joined.into_bytes(),
                newline: !literal,
            })?;
            match resp {
                DaemonMessage::InputSent => {}
                DaemonMessage::Error(e) => {
                    eprintln!("amux: error: {}", e);
                    std::process::exit(1);
                }
                other => eprintln!("amux: unexpected: {:?}", other),
            }
        }
        Command::Has { name } => {
            ensure_daemon_running()?;
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
            ensure_daemon_running()?;
            let resp = client::request(&ClientMessage::CaptureScrollback {
                name: name.clone(),
                lines,
            })?;
            match resp {
                DaemonMessage::CaptureOutput(data) => {
                    use std::io::Write;
                    let output = if plain { strip_ansi(&data) } else { data };
                    std::io::stdout().write_all(&output)?;
                }
                DaemonMessage::Error(e) => {
                    eprintln!("amux: error: {}", e);
                    std::process::exit(1);
                }
                other => eprintln!("amux: unexpected: {:?}", other),
            }
        }
        Command::Env { action } => {
            ensure_daemon_running()?;
            match action {
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
        }},
    }

    Ok(())
}

/// Watch multiple sessions for exit events.
fn do_watch(sessions: &[String], json: bool) -> anyhow::Result<()> {
    use anyhow::Context;
    use crate::protocol::codec::{read_frame, write_frame};

    let mut stream =
        client::connect().context("is the server running? try: amux start-server")?;
    write_frame(
        &mut stream,
        &ClientMessage::WatchSessions {
            sessions: sessions.to_vec(),
        },
    )?;

    loop {
        let resp: DaemonMessage = read_frame(&mut stream)?;
        match resp {
            DaemonMessage::WatchSessionExited {
                session,
                exit_code,
            } => {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "event": "session_exited",
                            "session": session,
                            "exit_code": exit_code,
                        })
                    );
                } else {
                    match exit_code {
                        Some(code) => println!("{}: exited ({})", session, code),
                        None => println!("{}: exited", session),
                    }
                }
            }
            DaemonMessage::WatchDone => {
                break;
            }
            DaemonMessage::Error(e) => {
                eprintln!("amux: error: {}", e);
                std::process::exit(1);
            }
            other => {
                eprintln!("amux: unexpected: {:?}", other);
                std::process::exit(1);
            }
        }
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
    let debug = std::env::var("AMUX_DEBUG").is_ok();

    if debug { eprintln!("amux-debug: do_attach('{}') start", name); }

    // Pre-check: verify session exists before attempting PTY attach.
    let resp = client::request(&ClientMessage::HasSession {
        name: name.to_string(),
    })?;
    if debug { eprintln!("amux-debug: HasSession response: {:?}", resp); }
    if !matches!(resp, DaemonMessage::SessionExists(true)) {
        eprintln!("amux: session '{}' not found", name);
        std::process::exit(1);
    }

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    if debug { eprintln!("amux-debug: terminal size: {}x{}", cols, rows); }

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
    if debug { eprintln!("amux-debug: Attach frame sent"); }

    // Switch to async for bidirectional streaming.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    if debug { eprintln!("amux-debug: tokio runtime built"); }

    rt.block_on(async {
        let raw_fd = stream.into_raw_fd();
        // Set O_NONBLOCK before converting to tokio — newer tokio panics on blocking fds.
        let old_flags = nix::fcntl::fcntl(raw_fd, nix::fcntl::FcntlArg::F_GETFL)
            .map_err(|e| anyhow::anyhow!("fcntl F_GETFL on socket: {}", e))?;
        let mut new_flags = nix::fcntl::OFlag::from_bits_truncate(old_flags);
        new_flags.insert(nix::fcntl::OFlag::O_NONBLOCK);
        nix::fcntl::fcntl(raw_fd, nix::fcntl::FcntlArg::F_SETFL(new_flags))
            .map_err(|e| anyhow::anyhow!("fcntl F_SETFL on socket: {}", e))?;
        if debug { eprintln!("amux-debug: socket set non-blocking, converting to tokio"); }
        let tokio_stream = unsafe {
            tokio::net::UnixStream::from_std(
                std::os::unix::net::UnixStream::from_raw_fd(raw_fd),
            )?
        };
        if debug { eprintln!("amux-debug: tokio stream created, entering run_attach"); }
        let (reader, mut writer) = tokio_stream.into_split();
        client::attach::run_attach(reader, &mut writer).await
    })
}

/// Follow a session's output (read-only streaming, no stdin).
fn do_follow(name: &str) -> anyhow::Result<()> {
    use crate::protocol::codec::{try_read_frame_async, write_frame, write_frame_async};
    use std::io::Write;

    // Pre-check: verify session exists.
    let resp = client::request(&ClientMessage::HasSession {
        name: name.to_string(),
    })?;
    if !matches!(resp, DaemonMessage::SessionExists(true)) {
        eprintln!("amux: session '{}' not found", name);
        std::process::exit(1);
    }

    let mut stream = client::connect().context("is the server running?")?;

    // Send Follow message.
    write_frame(
        &mut stream,
        &ClientMessage::Follow {
            name: name.to_string(),
        },
    )?;

    // Switch to async for streaming.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let raw_fd = stream.into_raw_fd();
        let old_flags = nix::fcntl::fcntl(raw_fd, nix::fcntl::FcntlArg::F_GETFL)
            .map_err(|e| anyhow::anyhow!("fcntl F_GETFL on socket: {}", e))?;
        let mut new_flags = nix::fcntl::OFlag::from_bits_truncate(old_flags);
        new_flags.insert(nix::fcntl::OFlag::O_NONBLOCK);
        nix::fcntl::fcntl(raw_fd, nix::fcntl::FcntlArg::F_SETFL(new_flags))
            .map_err(|e| anyhow::anyhow!("fcntl F_SETFL on socket: {}", e))?;
        let tokio_stream = unsafe {
            tokio::net::UnixStream::from_std(
                std::os::unix::net::UnixStream::from_raw_fd(raw_fd),
            )?
        };
        let (mut reader, mut writer) = tokio_stream.into_split();

        // Handle Ctrl+C gracefully.
        let mut sigint = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::interrupt(),
        )?;

        let mut stdout = std::io::stdout();
        loop {
            tokio::select! {
                msg = try_read_frame_async::<DaemonMessage>(&mut reader) => {
                    match msg {
                        Some(Ok(DaemonMessage::Output(data))) => {
                            let _ = stdout.write_all(&data);
                            let _ = stdout.flush();
                        }
                        Some(Ok(DaemonMessage::SessionEnded)) => {
                            break;
                        }
                        Some(Ok(DaemonMessage::Error(e))) => {
                            eprintln!("amux: error: {}", e);
                            std::process::exit(1);
                        }
                        Some(Err(e)) => {
                            eprintln!("amux: connection error: {}", e);
                            std::process::exit(1);
                        }
                        None => {
                            eprintln!("amux: disconnected from server");
                            break;
                        }
                        _ => {}
                    }
                }
                _ = sigint.recv() => {
                    // Send Detach to cleanly disconnect.
                    let _ = write_frame_async(
                        &mut writer,
                        &ClientMessage::Detach,
                    ).await;
                    break;
                }
            }
        }

        Ok(())
    })
}

use std::os::unix::io::{FromRawFd, IntoRawFd};

use anyhow::Context;

/// Strip ANSI escape sequences from raw bytes.
///
/// Handles CSI sequences (colors, cursor movement), OSC sequences (terminal title),
/// and other Fe escape sequences commonly found in terminal output.
fn strip_ansi(input: &[u8]) -> Vec<u8> {
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
                    // Already handled '[' (0x5B) and ']' (0x5D) above
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

#[cfg(test)]
mod tests {
    use super::strip_ansi;
    use crate::protocol::codec::{try_read_frame_async, write_frame_async};
    use crate::protocol::messages::{ClientMessage, DaemonMessage};

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
        // ESC[31m = red, ESC[0m = reset
        let input = b"\x1b[31mhello\x1b[0m world";
        assert_eq!(strip_ansi(input), b"hello world");
    }

    #[test]
    fn test_strip_ansi_cursor_movement() {
        // ESC[2J = clear screen, ESC[H = cursor home
        let input = b"\x1b[2J\x1b[Hprompt$ ";
        assert_eq!(strip_ansi(input), b"prompt$ ");
    }

    #[test]
    fn test_strip_ansi_osc_bel_terminated() {
        // OSC: ESC ] 0;title BEL
        let input = b"\x1b]0;my terminal\x07prompt$ ";
        assert_eq!(strip_ansi(input), b"prompt$ ");
    }

    #[test]
    fn test_strip_ansi_osc_st_terminated() {
        // OSC: ESC ] 0;title ESC backslash
        let input = b"\x1b]0;my terminal\x1b\\prompt$ ";
        assert_eq!(strip_ansi(input), b"prompt$ ");
    }

    #[test]
    fn test_strip_ansi_complex_csi() {
        // ESC[?2004h = bracketed paste mode, ESC[1;32m = bold green
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
        // Simulates typical zsh prompt output with colors and cursor codes
        let input = b"\x1b[1m\x1b[7m%\x1b[27m\x1b[1m\x1b[0m \r \r\x1b[0m\x1b[27m\x1b[24m$ echo ALIVE\r\nALIVE\r\n";
        let result = strip_ansi(input);
        let result_str = String::from_utf8_lossy(&result);
        assert!(result_str.contains("ALIVE"));
        // Should not contain any ESC characters
        assert!(!result.contains(&0x1b));
    }

    /// Integration test: verify AttachInput reaches the child process via daemon.
    /// This bypasses crossterm entirely and tests the daemon's handle_attach path.
    #[tokio::test]
    async fn test_attach_input_reaches_session() {
        use tokio::sync::broadcast;

        // Use a temp socket to avoid conflicting with a running daemon.
        let dir = std::env::temp_dir().join(format!("amux-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");
        let _ = std::fs::remove_file(&sock_path);

        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
        let (shutdown_tx, _) = broadcast::channel::<()>(1);

        // Run server in background.
        let server_shutdown = shutdown_tx.clone();
        tokio::spawn(async move {
            crate::daemon::server::run_server(listener, server_shutdown).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Connect and create a session running `cat`.
        let stream = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = stream.into_split();

        write_frame_async(
            &mut writer,
            &ClientMessage::CreateSession {
                name: Some("input-test".to_string()),
                command: vec!["cat".to_string()],
                env: None,
                cwd: None,
                cols: None,
                rows: None,
            },
        )
        .await
        .unwrap();

        let resp: DaemonMessage = try_read_frame_async(&mut reader).await.unwrap().unwrap();
        assert!(
            matches!(resp, DaemonMessage::SessionCreated { .. }),
            "expected SessionCreated, got {:?}",
            resp
        );

        // Give the session time to start and initialize the PTY.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Attach to the session on the same connection.
        write_frame_async(
            &mut writer,
            &ClientMessage::Attach {
                name: "input-test".to_string(),
                cols: 80,
                rows: 24,
            },
        )
        .await
        .unwrap();

        // Read any initial scrollback/output (shell prompt etc).
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Drain any pending output before sending our input.
        loop {
            tokio::select! {
                msg = try_read_frame_async::<DaemonMessage>(&mut reader) => {
                    match msg {
                        Some(Ok(DaemonMessage::Output(_))) => continue,
                        _ => break,
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => break,
            }
        }

        // === Test 1: AttachInput with \n (like SendInput uses) ===
        write_frame_async(
            &mut writer,
            &ClientMessage::AttachInput(b"hello\n".to_vec()),
        )
        .await
        .unwrap();

        let mut output = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            tokio::select! {
                msg = try_read_frame_async::<DaemonMessage>(&mut reader) => {
                    match msg {
                        Some(Ok(DaemonMessage::Output(data))) => {
                            output.extend_from_slice(&data);
                            // cat echoes input + writes it to stdout.
                            // Look for "hello" in the output.
                            let plain = strip_ansi(&output);
                            if plain.windows(5).any(|w| w == b"hello") {
                                break;
                            }
                        }
                        Some(Ok(DaemonMessage::SessionEnded)) => {
                            panic!("session ended unexpectedly");
                        }
                        _ => {}
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let plain = strip_ansi(&output);
                    panic!(
                        "timeout waiting for 'hello' in output.\nRaw output ({} bytes): {:?}\nPlain: {:?}",
                        output.len(),
                        String::from_utf8_lossy(&output),
                        String::from_utf8_lossy(&plain)
                    );
                }
            }
        }

        // === Test 2: AttachInput with \r (what crossterm sends for Enter) ===
        output.clear();
        write_frame_async(
            &mut writer,
            &ClientMessage::AttachInput(b"world\r".to_vec()),
        )
        .await
        .unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            tokio::select! {
                msg = try_read_frame_async::<DaemonMessage>(&mut reader) => {
                    match msg {
                        Some(Ok(DaemonMessage::Output(data))) => {
                            output.extend_from_slice(&data);
                            let plain = strip_ansi(&output);
                            if plain.windows(5).any(|w| w == b"world") {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let plain = strip_ansi(&output);
                    panic!(
                        "timeout waiting for 'world' in output (\\r test).\nRaw output ({} bytes): {:?}\nPlain: {:?}",
                        output.len(),
                        String::from_utf8_lossy(&output),
                        String::from_utf8_lossy(&plain)
                    );
                }
            }
        }

        // Clean up: detach and kill session.
        let _ = write_frame_async(&mut writer, &ClientMessage::Detach).await;
        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration test: AttachInput with individual keystrokes (like crossterm sends).
    /// This mimics how the real attach client sends each key as a separate message.
    #[tokio::test]
    async fn test_attach_input_individual_keys() {
        use tokio::sync::broadcast;

        let dir = std::env::temp_dir().join(format!("amux-test-keys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");
        let _ = std::fs::remove_file(&sock_path);

        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
        let (shutdown_tx, _) = broadcast::channel::<()>(1);

        let server_shutdown = shutdown_tx.clone();
        tokio::spawn(async move {
            crate::daemon::server::run_server(listener, server_shutdown).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let stream = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = stream.into_split();

        write_frame_async(
            &mut writer,
            &ClientMessage::CreateSession {
                name: Some("keys-test".to_string()),
                command: vec!["cat".to_string()],
                env: None,
                cwd: None,
                cols: None,
                rows: None,
            },
        )
        .await
        .unwrap();

        let resp: DaemonMessage = try_read_frame_async(&mut reader).await.unwrap().unwrap();
        assert!(matches!(resp, DaemonMessage::SessionCreated { .. }));

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Attach.
        write_frame_async(
            &mut writer,
            &ClientMessage::Attach {
                name: "keys-test".to_string(),
                cols: 80,
                rows: 24,
            },
        )
        .await
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Drain initial output.
        loop {
            tokio::select! {
                msg = try_read_frame_async::<DaemonMessage>(&mut reader) => {
                    match msg {
                        Some(Ok(DaemonMessage::Output(_))) => continue,
                        _ => break,
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => break,
            }
        }

        // Send individual characters like crossterm would: h, e, l, l, o, \r
        for &byte in b"hello" {
            write_frame_async(
                &mut writer,
                &ClientMessage::AttachInput(vec![byte]),
            )
            .await
            .unwrap();
            // Small delay between keystrokes, simulating human typing.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // Send Enter as \r (what crossterm sends for KeyCode::Enter).
        write_frame_async(
            &mut writer,
            &ClientMessage::AttachInput(vec![b'\r']),
        )
        .await
        .unwrap();

        // Read output and look for "hello".
        let mut output = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            tokio::select! {
                msg = try_read_frame_async::<DaemonMessage>(&mut reader) => {
                    match msg {
                        Some(Ok(DaemonMessage::Output(data))) => {
                            output.extend_from_slice(&data);
                            let plain = strip_ansi(&output);
                            if plain.windows(5).any(|w| w == b"hello") {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let plain = strip_ansi(&output);
                    panic!(
                        "timeout waiting for 'hello' with individual keys.\nRaw ({} bytes): {:?}\nPlain: {:?}",
                        output.len(),
                        String::from_utf8_lossy(&output),
                        String::from_utf8_lossy(&plain)
                    );
                }
            }
        }

        let _ = write_frame_async(&mut writer, &ClientMessage::Detach).await;
        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_version_flag_recognized() {
        // --version should be recognized by the CLI parser.
        // clap returns Err(DisplayVersion) when --version is passed.
        use clap::Parser;
        let result = super::Cli::try_parse_from(["amux", "--version"]);
        match result {
            Err(e) => assert_eq!(e.kind(), clap::error::ErrorKind::DisplayVersion),
            Ok(_) => panic!("expected --version to produce DisplayVersion error"),
        }
    }

    /// Regression test: tokio::net::UnixStream::from_std requires O_NONBLOCK.
    /// Without it, newer tokio versions panic. This validates our fix in do_attach().
    #[tokio::test]
    async fn test_unix_stream_needs_nonblocking_for_tokio() {
        use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};

        let dir = std::env::temp_dir().join(format!("amux-test-nb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");
        let _ = std::fs::remove_file(&sock_path);

        let listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
        let std_stream = std::os::unix::net::UnixStream::connect(&sock_path).unwrap();

        // The fd starts as blocking
        let fd = std_stream.as_raw_fd();
        let flags = nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFL).unwrap();
        let oflags = nix::fcntl::OFlag::from_bits_truncate(flags);
        assert!(
            !oflags.contains(nix::fcntl::OFlag::O_NONBLOCK),
            "std socket should start blocking"
        );

        // Set O_NONBLOCK (this is what our fix does)
        let mut new_flags = oflags;
        new_flags.insert(nix::fcntl::OFlag::O_NONBLOCK);
        nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_SETFL(new_flags)).unwrap();

        // Now from_std should succeed without panic
        let raw_fd = std_stream.into_raw_fd();
        let rebuilt = unsafe { std::os::unix::net::UnixStream::from_raw_fd(raw_fd) };
        let tokio_stream = tokio::net::UnixStream::from_std(rebuilt);
        assert!(tokio_stream.is_ok(), "from_std must succeed with O_NONBLOCK set");

        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration test: verify SendInput path works (for comparison with AttachInput).
    #[tokio::test]
    async fn test_send_input_reaches_session() {
        use tokio::sync::broadcast;

        let dir = std::env::temp_dir().join(format!("amux-test-send-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");
        let _ = std::fs::remove_file(&sock_path);

        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
        let (shutdown_tx, _) = broadcast::channel::<()>(1);

        let server_shutdown = shutdown_tx.clone();
        tokio::spawn(async move {
            crate::daemon::server::run_server(listener, server_shutdown).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Connection 1: create session.
        let stream = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (mut reader, mut writer) = stream.into_split();

        write_frame_async(
            &mut writer,
            &ClientMessage::CreateSession {
                name: Some("send-test".to_string()),
                command: vec!["cat".to_string()],
                env: None,
                cwd: None,
                cols: None,
                rows: None,
            },
        )
        .await
        .unwrap();

        let resp: DaemonMessage = try_read_frame_async(&mut reader).await.unwrap().unwrap();
        assert!(matches!(resp, DaemonMessage::SessionCreated { .. }));

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Send input via SendInput (the working path).
        write_frame_async(
            &mut writer,
            &ClientMessage::SendInput {
                name: "send-test".to_string(),
                data: b"hello".to_vec(),
                newline: true,
            },
        )
        .await
        .unwrap();

        let resp: DaemonMessage = try_read_frame_async(&mut reader).await.unwrap().unwrap();
        assert!(
            matches!(resp, DaemonMessage::InputSent),
            "expected InputSent, got {:?}",
            resp
        );

        // Wait for output to be generated, then capture scrollback.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        write_frame_async(
            &mut writer,
            &ClientMessage::CaptureScrollback {
                name: "send-test".to_string(),
                lines: 10,
            },
        )
        .await
        .unwrap();

        let resp: DaemonMessage = try_read_frame_async(&mut reader).await.unwrap().unwrap();
        match resp {
            DaemonMessage::CaptureOutput(data) => {
                let plain = strip_ansi(&data);
                let output_str = String::from_utf8_lossy(&plain);
                assert!(
                    output_str.contains("hello"),
                    "SendInput: expected 'hello' in scrollback, got: {:?}",
                    output_str
                );
            }
            other => panic!("expected CaptureOutput, got {:?}", other),
        }

        // Clean up.
        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reproduce the exact do_attach code path: sync write Attach, then convert
    /// the socket to async and read Output frames. This tests whether the
    /// sync→async conversion loses data.
    #[tokio::test]
    async fn test_attach_sync_write_then_async_read() {
        use std::os::unix::io::{FromRawFd, IntoRawFd};
        use tokio::sync::broadcast;

        let dir = std::env::temp_dir().join(format!("amux-test-sync-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");
        let _ = std::fs::remove_file(&sock_path);

        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
        let (shutdown_tx, _) = broadcast::channel::<()>(1);

        let server_shutdown = shutdown_tx.clone();
        tokio::spawn(async move {
            crate::daemon::server::run_server(listener, server_shutdown).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Create session on an async connection (like ensure_daemon + client::request).
        let stream = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (mut r, mut w) = stream.into_split();
        write_frame_async(
            &mut w,
            &ClientMessage::CreateSession {
                name: Some("sync-attach-test".to_string()),
                command: vec!["cat".to_string()],
                env: None,
                cwd: None,
                cols: None,
                rows: None,
            },
        )
        .await
        .unwrap();
        let resp: DaemonMessage = try_read_frame_async(&mut r).await.unwrap().unwrap();
        assert!(matches!(resp, DaemonMessage::SessionCreated { .. }));
        drop(r);
        drop(w);

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Now mimic do_attach: connect with a SYNC std socket, write Attach
        // synchronously, then convert to async.
        let std_sock_path = sock_path.clone();
        let std_stream =
            std::os::unix::net::UnixStream::connect(&std_sock_path).unwrap();

        // Sync write (exactly like do_attach).
        let mut sync_stream = std_stream;
        crate::protocol::codec::write_frame(
            &mut sync_stream,
            &ClientMessage::Attach {
                name: "sync-attach-test".to_string(),
                cols: 80,
                rows: 24,
            },
        )
        .unwrap();

        // Convert to async (exactly like do_attach).
        let raw_fd = sync_stream.into_raw_fd();
        let old_flags = nix::fcntl::fcntl(raw_fd, nix::fcntl::FcntlArg::F_GETFL).unwrap();
        let mut new_flags = nix::fcntl::OFlag::from_bits_truncate(old_flags);
        new_flags.insert(nix::fcntl::OFlag::O_NONBLOCK);
        nix::fcntl::fcntl(raw_fd, nix::fcntl::FcntlArg::F_SETFL(new_flags)).unwrap();
        let tokio_stream = unsafe {
            tokio::net::UnixStream::from_std(
                std::os::unix::net::UnixStream::from_raw_fd(raw_fd),
            )
            .unwrap()
        };
        let (mut reader, mut writer) = tokio_stream.into_split();

        // Read — we should get at least one Output frame (scrollback or live output).
        let mut got_output = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            tokio::select! {
                msg = try_read_frame_async::<DaemonMessage>(&mut reader) => {
                    match msg {
                        Some(Ok(DaemonMessage::Output(_data))) => {
                            got_output = true;
                            break;
                        }
                        Some(Ok(other)) => {
                            panic!("unexpected message: {:?}", other);
                        }
                        Some(Err(e)) => {
                            panic!("read error: {}", e);
                        }
                        None => {
                            panic!("disconnected before receiving Output");
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    break;
                }
            }
        }

        // Send some input to generate output if scrollback was empty.
        if !got_output {
            write_frame_async(
                &mut writer,
                &ClientMessage::AttachInput(b"hello\n".to_vec()),
            )
            .await
            .unwrap();

            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                tokio::select! {
                    msg = try_read_frame_async::<DaemonMessage>(&mut reader) => {
                        match msg {
                            Some(Ok(DaemonMessage::Output(_))) => {
                                got_output = true;
                                break;
                            }
                            _ => {}
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        break;
                    }
                }
            }
        }

        assert!(got_output, "sync write → async read: never received Output from server");

        let _ = write_frame_async(&mut writer, &ClientMessage::Detach).await;
        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_follow_streams_output() {
        use tokio::sync::broadcast;
        let dir = std::env::temp_dir().join(format!("amux-test-follow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");
        let _ = std::fs::remove_file(&sock_path);
        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
        let (shutdown_tx, _) = broadcast::channel::<()>(1);
        let server_shutdown = shutdown_tx.clone();
        tokio::spawn(async move {
            crate::daemon::server::run_server(listener, server_shutdown).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let stream = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (mut r, mut w) = stream.into_split();
        write_frame_async(&mut w, &ClientMessage::CreateSession {
            name: Some("follow-test".to_string()),
            command: vec!["cat".to_string()],
            env: None, cwd: None, cols: None, rows: None,
        }).await.unwrap();
        let resp: DaemonMessage = try_read_frame_async(&mut r).await.unwrap().unwrap();
        assert!(matches!(resp, DaemonMessage::SessionCreated { .. }));
        write_frame_async(&mut w, &ClientMessage::SendInput {
            name: "follow-test".to_string(),
            data: b"hello-follow".to_vec(), newline: true,
        }).await.unwrap();
        let resp: DaemonMessage = try_read_frame_async(&mut r).await.unwrap().unwrap();
        assert!(matches!(resp, DaemonMessage::InputSent));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let stream2 = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (mut fr, mut fw) = stream2.into_split();
        write_frame_async(&mut fw, &ClientMessage::Follow {
            name: "follow-test".to_string(),
        }).await.unwrap();
        let mut output = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            tokio::select! {
                msg = try_read_frame_async::<DaemonMessage>(&mut fr) => {
                    match msg {
                        Some(Ok(DaemonMessage::Output(data))) => {
                            output.extend_from_slice(&data);
                            let plain = strip_ansi(&output);
                            if plain.windows(12).any(|w| w == b"hello-follow") { break; }
                        }
                        _ => {}
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    panic!("timeout waiting for 'hello-follow' in follow output");
                }
            }
        }
        output.clear();
        write_frame_async(&mut w, &ClientMessage::SendInput {
            name: "follow-test".to_string(),
            data: b"live-data".to_vec(), newline: true,
        }).await.unwrap();
        let _: DaemonMessage = try_read_frame_async(&mut r).await.unwrap().unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            tokio::select! {
                msg = try_read_frame_async::<DaemonMessage>(&mut fr) => {
                    match msg {
                        Some(Ok(DaemonMessage::Output(data))) => {
                            output.extend_from_slice(&data);
                            let plain = strip_ansi(&output);
                            if plain.windows(9).any(|w| w == b"live-data") { break; }
                        }
                        _ => {}
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    panic!("timeout waiting for 'live-data' in follow output");
                }
            }
        }
        let _ = write_frame_async(&mut fw, &ClientMessage::Detach).await;
        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_follow_session_ended() {
        use tokio::sync::broadcast;
        let dir = std::env::temp_dir().join(format!("amux-test-fend-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");
        let _ = std::fs::remove_file(&sock_path);
        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
        let (shutdown_tx, _) = broadcast::channel::<()>(1);
        let server_shutdown = shutdown_tx.clone();
        tokio::spawn(async move {
            crate::daemon::server::run_server(listener, server_shutdown).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let stream = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (mut r, mut w) = stream.into_split();
        write_frame_async(&mut w, &ClientMessage::CreateSession {
            name: Some("follow-end-test".to_string()),
            command: vec!["echo".to_string(), "bye".to_string()],
            env: None, cwd: None, cols: None, rows: None,
        }).await.unwrap();
        let resp: DaemonMessage = try_read_frame_async(&mut r).await.unwrap().unwrap();
        assert!(matches!(resp, DaemonMessage::SessionCreated { .. }));
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let stream2 = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (mut fr, mut fw) = stream2.into_split();
        write_frame_async(&mut fw, &ClientMessage::Follow {
            name: "follow-end-test".to_string(),
        }).await.unwrap();
        let mut got_ended = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            tokio::select! {
                msg = try_read_frame_async::<DaemonMessage>(&mut fr) => {
                    match msg {
                        Some(Ok(DaemonMessage::Output(_))) => continue,
                        Some(Ok(DaemonMessage::SessionEnded)) => { got_ended = true; break; }
                        other => panic!("unexpected: {:?}", other),
                    }
                }
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
        assert!(got_ended, "follow should receive SessionEnded when session exits");
        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_follow_nonexistent_session() {
        use tokio::sync::broadcast;
        let dir = std::env::temp_dir().join(format!("amux-test-fne-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");
        let _ = std::fs::remove_file(&sock_path);
        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
        let (shutdown_tx, _) = broadcast::channel::<()>(1);
        let server_shutdown = shutdown_tx.clone();
        tokio::spawn(async move {
            crate::daemon::server::run_server(listener, server_shutdown).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let stream = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        let (mut fr, mut fw) = stream.into_split();
        write_frame_async(&mut fw, &ClientMessage::Follow {
            name: "nonexistent".to_string(),
        }).await.unwrap();
        let resp: DaemonMessage = try_read_frame_async(&mut fr).await.unwrap().unwrap();
        match resp {
            DaemonMessage::Error(e) => assert!(e.contains("not found"), "got: {}", e),
            other => panic!("expected Error, got {:?}", other),
        }
        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
