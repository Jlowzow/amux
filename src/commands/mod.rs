mod attach;
mod query;
mod server;
mod session;
mod top;

use crate::cli::{Command, EnvAction};
use crate::protocol::messages::{ClientMessage, DaemonMessage};
use crate::util::ensure_daemon_running;
use crate::client;

pub fn dispatch(command: Command) -> anyhow::Result<()> {
    match command {
        Command::StartServer => {
            server::start_server()?;
        }
        Command::KillServer { force } => {
            server::kill_server(force)?;
        }
        Command::Ping => {
            server::ping()?;
        }
        Command::Top { once } => {
            if once {
                top::do_top_once()?;
            } else {
                top::do_top()?;
            }
        }
        Command::New {
            name,
            detached,
            env,
            cwd,
            worktree,
            init_message,
            cmd,
        } => {
            session::new_session(name, detached, env, cwd, worktree, init_message, cmd)?;
        }
        Command::Attach { name } => {
            ensure_daemon_running()?;
            attach::do_attach(&name)?;
        }
        Command::Follow { name, raw, plain: _ } => {
            ensure_daemon_running()?;
            attach::do_follow(&name, !raw)?;
        }
        Command::Ls { json } => {
            query::list_sessions(json)?;
        }
        Command::Info { name, json } => {
            query::session_info(&name, json)?;
        }
        Command::Wait {
            name,
            any,
            timeout,
            exit_code,
        } => {
            query::wait_session(name, any, timeout, exit_code)?;
        }
        Command::Watch { sessions, json, on_exit } => {
            ensure_daemon_running()?;
            query::do_watch(&sessions, json, on_exit.as_deref())?;
        }
        Command::Kill { name, all } => {
            ensure_daemon_running()?;
            if all {
                session::do_kill_all()?;
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
            session::do_kill_all()?;
        }
        Command::Send {
            name,
            literal,
            text,
        } => {
            session::send_keys(&name, literal, &text)?;
        }
        Command::Has { name } => {
            session::has_session(&name)?;
        }
        Command::Capture { name, lines, raw, plain: _ } => {
            session::capture_scrollback(&name, lines, !raw)?;
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
            }
        }
    }

    Ok(())
}
