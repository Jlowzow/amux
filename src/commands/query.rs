use crate::protocol::codec::{read_frame, write_frame};
use crate::protocol::messages::{ClientMessage, DaemonMessage};
use crate::util::ensure_daemon_running;
use crate::client;

use anyhow::Context;

pub fn list_sessions(json: bool) -> anyhow::Result<()> {
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
            } else if !sessions.is_empty() {
                for s in &sessions {
                    let status = if s.alive {
                        String::new()
                    } else {
                        match s.exit_code {
                            Some(code) => format!(" (exited({}))", code),
                            None => " (dead)".to_string(),
                        }
                    };
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
    Ok(())
}

pub fn session_info(name: &str, json: bool) -> anyhow::Result<()> {
    ensure_daemon_running()?;
    let resp = client::request(&ClientMessage::GetSessionInfo {
        name: name.to_string(),
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
                let status = if info.alive {
                    "alive".to_string()
                } else {
                    match info.exit_code {
                        Some(code) => format!("exited({})", code),
                        None => "dead".to_string(),
                    }
                };
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
    Ok(())
}

pub fn wait_session(
    name: Option<String>,
    any: Vec<String>,
    timeout: u64,
    exit_code: bool,
) -> anyhow::Result<()> {
    ensure_daemon_running()?;
    if !any.is_empty() {
        let resp = client::request(&ClientMessage::WaitAny {
            sessions: any,
            timeout_secs: timeout,
        })?;
        match resp {
            DaemonMessage::WaitAnyExited {
                session,
                exit_code: code,
            } => {
                println!("{}", session);
                if exit_code {
                    if let Some(c) = code {
                        std::process::exit(c);
                    }
                }
            }
            DaemonMessage::Error(e) => {
                if e == "timeout" {
                    eprintln!("amux: wait --any timed out");
                    std::process::exit(2);
                }
                eprintln!("amux: error: {}", e);
                std::process::exit(1);
            }
            other => eprintln!("amux: unexpected: {:?}", other),
        }
    } else {
        let name = name.unwrap();
        let resp = client::request(&ClientMessage::WaitSession {
            name: name.clone(),
            timeout_secs: timeout,
        })?;
        match resp {
            DaemonMessage::SessionExited => {
                if exit_code {
                    let resp = client::request(&ClientMessage::GetExitCode {
                        name: name.clone(),
                    })?;
                    match resp {
                        DaemonMessage::ExitCode(Some(code)) => {
                            println!("{}", code);
                            std::process::exit(code);
                        }
                        DaemonMessage::ExitCode(None) => {
                            eprintln!(
                                "amux: exit code unavailable for session '{}'",
                                name
                            );
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
    Ok(())
}

/// Watch multiple sessions for exit events.
pub fn do_watch(sessions: &[String], json: bool) -> anyhow::Result<()> {
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

#[cfg(test)]
mod tests {
    use crate::protocol::codec::{try_read_frame_async, write_frame_async};
    use crate::protocol::messages::{ClientMessage, DaemonMessage};

    /// Integration test: WatchSessions receives exit events for multiple sessions.
    #[tokio::test]
    async fn test_watch_sessions_multiple_exits() {
        use tokio::sync::broadcast;

        let dir = std::env::temp_dir().join(format!("amux-test-watch-{}", std::process::id()));
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
                name: Some("watch-a".to_string()),
                command: vec!["sh".to_string(), "-c".to_string(), "exit 0".to_string()],
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

        write_frame_async(
            &mut writer,
            &ClientMessage::CreateSession {
                name: Some("watch-b".to_string()),
                command: vec!["sh".to_string(), "-c".to_string(), "exit 42".to_string()],
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

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        write_frame_async(
            &mut writer,
            &ClientMessage::WatchSessions {
                sessions: vec!["watch-a".to_string(), "watch-b".to_string()],
            },
        )
        .await
        .unwrap();

        let mut events: std::collections::HashMap<String, Option<i32>> =
            std::collections::HashMap::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            tokio::select! {
                msg = try_read_frame_async::<DaemonMessage>(&mut reader) => {
                    match msg {
                        Some(Ok(DaemonMessage::WatchSessionExited { session, exit_code })) => {
                            events.insert(session, exit_code);
                        }
                        Some(Ok(DaemonMessage::WatchDone)) => break,
                        Some(Ok(other)) => panic!("unexpected message: {:?}", other),
                        Some(Err(e)) => panic!("read error: {}", e),
                        None => panic!("disconnected"),
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    panic!("timeout waiting for watch events, got: {:?}", events);
                }
            }
        }

        assert_eq!(events.len(), 2, "expected 2 exit events, got: {:?}", events);
        assert_eq!(events.get("watch-a"), Some(&Some(0)));
        assert_eq!(events.get("watch-b"), Some(&Some(42)));

        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration test: WatchSessions waits for live sessions to exit.
    #[tokio::test]
    async fn test_watch_sessions_live_then_exit() {
        use tokio::sync::broadcast;

        let dir =
            std::env::temp_dir().join(format!("amux-test-watch-live-{}", std::process::id()));
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
                name: Some("watch-live".to_string()),
                command: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "sleep 0.3; exit 7".to_string(),
                ],
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

        write_frame_async(
            &mut writer,
            &ClientMessage::WatchSessions {
                sessions: vec!["watch-live".to_string()],
            },
        )
        .await
        .unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut got_exit = false;
        loop {
            tokio::select! {
                msg = try_read_frame_async::<DaemonMessage>(&mut reader) => {
                    match msg {
                        Some(Ok(DaemonMessage::WatchSessionExited { session, exit_code })) => {
                            assert_eq!(session, "watch-live");
                            assert_eq!(exit_code, Some(7));
                            got_exit = true;
                        }
                        Some(Ok(DaemonMessage::WatchDone)) => {
                            assert!(got_exit, "got WatchDone before any exit event");
                            break;
                        }
                        Some(Ok(other)) => panic!("unexpected: {:?}", other),
                        Some(Err(e)) => panic!("read error: {}", e),
                        None => panic!("disconnected"),
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    panic!("timeout waiting for watch-live exit event");
                }
            }
        }

        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration test: WatchSessions returns error for nonexistent session.
    #[tokio::test]
    async fn test_watch_sessions_not_found() {
        use tokio::sync::broadcast;

        let dir =
            std::env::temp_dir().join(format!("amux-test-watch-nf-{}", std::process::id()));
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
            &ClientMessage::WatchSessions {
                sessions: vec!["nonexistent".to_string()],
            },
        )
        .await
        .unwrap();

        let resp: DaemonMessage = try_read_frame_async(&mut reader).await.unwrap().unwrap();
        assert!(
            matches!(resp, DaemonMessage::Error(ref e) if e.contains("not found")),
            "expected Error(not found), got: {:?}",
            resp
        );

        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration test: SessionInfo includes exit_code for exited sessions.
    #[tokio::test]
    async fn test_ls_shows_exit_code() {
        use tokio::sync::broadcast;

        let dir =
            std::env::temp_dir().join(format!("amux-test-ls-exit-{}", std::process::id()));
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
                name: Some("exit-test".to_string()),
                command: vec!["sh".to_string(), "-c".to_string(), "exit 42".to_string()],
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
            matches!(resp, DaemonMessage::SessionCreated { ref name } if name == "exit-test"),
            "expected SessionCreated, got: {:?}",
            resp
        );

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        write_frame_async(&mut writer, &ClientMessage::ListSessions)
            .await
            .unwrap();

        let resp: DaemonMessage = try_read_frame_async(&mut reader).await.unwrap().unwrap();
        match resp {
            DaemonMessage::SessionList(sessions) => {
                let session = sessions
                    .iter()
                    .find(|s| s.name == "exit-test")
                    .expect("exit-test session not found in listing");
                assert!(!session.alive, "session should be dead");
                assert_eq!(
                    session.exit_code,
                    Some(42),
                    "expected exit_code=Some(42), got {:?}",
                    session.exit_code
                );
            }
            other => panic!("expected SessionList, got: {:?}", other),
        }

        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Integration test: SessionInfo exit_code is None for running sessions.
    #[tokio::test]
    async fn test_ls_exit_code_none_for_alive() {
        use tokio::sync::broadcast;

        let dir =
            std::env::temp_dir().join(format!("amux-test-ls-alive-{}", std::process::id()));
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
                name: Some("alive-test".to_string()),
                command: vec!["sleep".to_string(), "60".to_string()],
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

        write_frame_async(&mut writer, &ClientMessage::ListSessions)
            .await
            .unwrap();

        let resp: DaemonMessage = try_read_frame_async(&mut reader).await.unwrap().unwrap();
        match resp {
            DaemonMessage::SessionList(sessions) => {
                let session = sessions
                    .iter()
                    .find(|s| s.name == "alive-test")
                    .expect("alive-test session not found");
                assert!(session.alive, "session should be alive");
                assert_eq!(session.exit_code, None, "alive session should have no exit code");
            }
            other => panic!("expected SessionList, got: {:?}", other),
        }

        write_frame_async(
            &mut writer,
            &ClientMessage::KillSession {
                name: "alive-test".to_string(),
            },
        )
        .await
        .unwrap();
        let _ = try_read_frame_async::<DaemonMessage>(&mut reader).await;

        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
