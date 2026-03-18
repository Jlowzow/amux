use crate::protocol::messages::{ClientMessage, DaemonMessage};
use crate::{client, common, daemon};

pub fn start_server() -> anyhow::Result<()> {
    if common::server_running() {
        eprintln!("amux: server is already running");
        return Ok(());
    }
    daemon::fork_daemon()?;
    Ok(())
}

pub fn kill_server(force: bool) -> anyhow::Result<()> {
    if force {
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
    Ok(())
}

pub fn ping() -> anyhow::Result<()> {
    let resp = client::request(&ClientMessage::Ping)?;
    match resp {
        DaemonMessage::Pong => println!("pong"),
        other => eprintln!("amux: unexpected response: {:?}", other),
    }
    Ok(())
}
